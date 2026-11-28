//! The Crackle Filter: takes the clicks and crackle of old records (78 rpm
//! and vinyl transfers) out of the sound.
//!
//! It is the classic model-based declicker (Vaseghi & Rayner 1990;
//! Godsill & Rayner, "Digital Audio Restoration", 1998, ch. 5):
//! 1. Music is predictable from the few milliseconds around it; a click is
//!    not. Each channel is cut into blocks of about 23 ms, and an all-pole
//!    (AR) model of order [`order`] is fitted to twice that much sound
//!    around each block (autocorrelation method, Levinson-Durbin).
//! 2. Every sample is predicted from the samples before it (forward) and
//!    from the samples after it (backward). A click spoils both
//!    predictions at once; the start of a drum hit or a consonant only the
//!    forward one, because what follows it is the hit itself. A sample is
//!    suspect when both errors are more than [`Strength::k`] robust sigmas
//!    (median absolute deviation over the block) away.
//! 3. Suspects closer than half the model order are one click, widened by
//!    [`WIDEN`] samples each side. A click longer than
//!    [`Strength::max_ms`] is a real attack and stays. So does one whose
//!    error doesn't stand [`Strength::iso`] times above the error around
//!    it: in busy music the error is high everywhere, and that is music.
//! 4. Each click is rebuilt by least-squares AR interpolation (Janssen,
//!    Veldhuis & Vries 1986): the samples that make the prediction error
//!    smallest given the good samples around them. That is a small
//!    symmetric Toeplitz system, solved by Cholesky. Once a block has
//!    clicks, the model is fitted again with them provisionally repaired
//!    (clicks bias the fit) and the block is looked at again.
//!
//! The two strengths come from measurements on the RES-3 test set (clean
//! modern masters and speech through MP3, AAC and Opus; real 78 rpm
//! transfers; synthetic clicks with a clean reference). Light only takes
//! the clicks no music makes: on modern masters it repairs nothing or
//! next to nothing, and on a 78 it still takes out every big click.
//! Strong also takes the fine crackle under the music, at the cost of
//! sometimes smoothing a hi-hat or drum attack of a modern master, so it is
//! for record stations. Nothing in between was safe on modern music and
//! still useful on records: on their own, dense modern masters and quiet
//! crackle look alike to the detector.
//!
//! The filter reads [`reach`] samples past the block it works on, so it
//! gives back each sample about 35 ms after it reads it, on or off. The
//! samples come back at the same place in the stream (nothing is shifted),
//! and Off gives back exactly what came in. A repair only replaces samples
//! inside a click with a curve joining the samples either side of it, so
//! switching on or off needs no fade and never jumps.

use std::collections::VecDeque;
use std::num::NonZero;
use std::sync::Arc;
use std::time::Duration;

use rodio::Source;

use super::sound::{CrackleLevel, FilterSwitches};

/// More channels than this pass through as they are
const MAX_CHANNELS: usize = 8;
/// Samples added each side of a click: its edges are spoilt too, but
/// stand out less
const WIDEN: usize = 2;
/// Samples each side of a click whose prediction error it must stand
/// above
const CONTEXT: usize = 256;
/// Suspects this close (in model orders) are one click
const BRIDGE: f64 = 0.5;
/// A click whose error peak is below this (about -90 dBFS) is never heard;
/// near digital silence the robust sigma is almost nothing
const MIN_PEAK: f64 = 3e-5;
/// Added to the model's zero-lag autocorrelation, as a share of it, so the
/// fit stays well conditioned on sound with empty bands
const WHITE_NOISE: f64 = 1e-5;
/// Median absolute deviation to the sigma of a normal distribution
const MAD_SIGMA: f64 = 1.4826;

/// How hard the filter looks for clicks
#[derive(Clone, Copy, Debug, PartialEq)]
struct Strength {
    /// Both prediction errors must exceed this many robust sigmas
    k: f64,
    /// The error peak must stand this many times above the error's RMS
    /// around the click
    iso: f64,
    /// Longest click (ms); anything longer is music
    max_ms: f32,
}

impl Strength {
    /// Longest click, in samples
    fn longest(self, sample_rate: u32) -> usize {
        ((self.max_ms * 1e-3 * sample_rate as f32) as usize).max(4)
    }
}

fn strength(level: CrackleLevel) -> Option<Strength> {
    match level {
        CrackleLevel::Off => None,
        // Measured on 23 modern music items (lossless masters and the same
        // through MP3, AAC and Opus, 22 to 48 kHz): 0 to 0.2 repairs a
        // second, at least 50 dB from the input. Every big click of the
        // HMV 78 still goes.
        CrackleLevel::Light => Some(Strength {
            k: 12.0,
            iso: 10.0,
            max_ms: 2.5,
        }),
        // The RES-3 prototype's best setting on synthetic crackle (SNR
        // 22.7 to 26.2 dB): 10 to 90 repairs a second on a crackly 78,
        // up to 19 a second on a dense modern master
        CrackleLevel::Strong => Some(Strength {
            k: 5.0,
            iso: 4.0,
            max_ms: 2.5,
        }),
    }
}

/// Block length: about 23 ms
fn block_len(sample_rate: u32) -> usize {
    if sample_rate <= 24_000 {
        512
    } else if sample_rate <= 48_000 {
        1024
    } else {
        2048
    }
}

/// Model order: enough poles for the band the stream carries
fn order(sample_rate: u32) -> usize {
    if sample_rate <= 24_000 {
        20
    } else {
        32
    }
}

/// Samples needed either side of a block: half a block for the model's
/// window, and for the errors around the widest click its context and two
/// model orders (one for the error, one for the backward error)
fn reach(block: usize, order: usize) -> usize {
    (block / 2).max(WIDEN + CONTEXT + 2 * order)
}

/// Levinson-Durbin: the AR coefficients `a` (with `a[0] = 1`) that best
/// predict a signal with autocorrelation `r`
fn levinson(r: &[f64], a: &mut [f64], tmp: &mut [f64]) {
    let p = a.len() - 1;
    a.fill(0.0);
    a[0] = 1.0;
    let mut err = r[0];
    if err <= 0.0 {
        return;
    }
    for i in 1..=p {
        let mut acc = r[i];
        for j in 1..i {
            acc += a[j] * r[i - j];
        }
        let k = -acc / err;
        tmp[..=i].copy_from_slice(&a[..=i]);
        for j in 1..i {
            a[j] = tmp[j] + k * tmp[i - j];
        }
        a[i] = k;
        err *= 1.0 - k * k;
        // A perfectly predictable signal: the rest stays zero
        if err <= 1e-12 * r[0] {
            break;
        }
    }
}

/// Four sums side by side: one running sum makes every addition wait
/// for the last, and the filter spends most of its time here
fn dot(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let (a, a_rest) = a[..n].as_chunks::<4>();
    let (b, b_rest) = b[..n].as_chunks::<4>();
    let mut sums = [0.0; 4];
    for (x, y) in a.iter().zip(b) {
        for k in 0..4 {
            sums[k] += x[k] * y[k];
        }
    }
    let rest: f64 = a_rest.iter().zip(b_rest).map(|(x, y)| x * y).sum();
    (sums[0] + sums[1]) + (sums[2] + sums[3]) + rest
}

/// `1.4826 * median(|v|)`, a sigma that a few clicks don't move. Sorts
/// `scratch`.
fn robust_sigma(v: &[f64], scratch: &mut Vec<f64>) -> f64 {
    scratch.clear();
    scratch.extend(v.iter().map(|x| x.abs()));
    if scratch.is_empty() {
        return 1e-9;
    }
    let mid = scratch.len() / 2;
    let (_, median, _) = scratch.select_nth_unstable_by(mid, f64::total_cmp);
    MAD_SIGMA * *median + 1e-9
}

/// What the filter keeps between blocks, and reuses so a block allocates
/// nothing
struct Work {
    order: usize,
    /// Plain Hann over the model's window
    hann: Vec<f64>,
    windowed: Vec<f64>,
    r: Vec<f64>,
    a: Vec<f64>,
    /// `a` back to front, so the forward error is a plain dot product
    a_rev: Vec<f64>,
    tmp: Vec<f64>,
    /// Forward and backward prediction errors
    ef: Vec<f64>,
    eb: Vec<f64>,
    sorted: Vec<f64>,
    runs: Vec<(usize, usize)>,
    /// The model's window with the clicks provisionally repaired
    repaired: Vec<f64>,
    /// The interpolation's normal equations
    matrix: Vec<f64>,
    rhs: Vec<f64>,
    ra: Vec<f64>,
}

impl Work {
    fn new(order: usize, window: usize, errors: usize, longest: usize) -> Self {
        let hann = (0..window)
            .map(|i| {
                let phase = std::f64::consts::TAU * (i as f64 + 0.5) / window as f64;
                0.5 - 0.5 * phase.cos()
            })
            .collect();
        Self {
            order,
            hann,
            windowed: vec![0.0; window],
            r: vec![0.0; order + 1],
            a: vec![0.0; order + 1],
            a_rev: vec![0.0; order + 1],
            tmp: vec![0.0; order + 1],
            ef: vec![0.0; errors],
            eb: vec![0.0; errors],
            sorted: Vec::with_capacity(errors),
            runs: Vec::new(),
            repaired: vec![0.0; window],
            matrix: vec![0.0; longest * longest],
            rhs: vec![0.0; longest],
            ra: vec![0.0; order + 1],
        }
    }

    /// Fit the AR model to `seg` (Hann-windowed autocorrelation)
    fn fit(&mut self, seg: &[f64]) {
        for ((w, x), h) in self.windowed.iter_mut().zip(seg).zip(&self.hann) {
            *w = x * h;
        }
        let n = seg.len();
        for lag in 0..=self.order {
            self.r[lag] = dot(&self.windowed[lag..n], &self.windowed[..n - lag]);
        }
        self.r[0] = self.r[0] * (1.0 + WHITE_NOISE) + 1e-14;
        levinson(&self.r, &mut self.a, &mut self.tmp);
        for (rev, a) in self.a_rev.iter_mut().zip(self.a.iter().rev()) {
            *rev = *a;
        }
    }

    /// Find the clicks in `x[b0..b1]` with the model last fitted: into
    /// `runs`, as inclusive sample ranges
    fn detect(&mut self, x: &[f64], b0: usize, b1: usize, strength: Strength, longest: usize) {
        let p = self.order;
        // Errors are needed over the block, and either side of it for the
        // widest click's context
        let e0 = b0 - WIDEN - CONTEXT - p;
        let e1 = b1 + WIDEN + p + CONTEXT;
        for n in e0..e1 {
            self.ef[n - e0] = dot(&self.a_rev, &x[n - p..=n]);
            self.eb[n - e0] = dot(&self.a, &x[n..=n + p]);
        }
        let (r0, r1) = (b0 - e0, b1 - e0);
        let sf = strength.k * robust_sigma(&self.ef[r0..r1], &mut self.sorted);
        let sb = strength.k * robust_sigma(&self.eb[r0..r1], &mut self.sorted);
        let bridge = ((p as f64 * BRIDGE) as usize).max(3);
        self.runs.clear();
        for n in r0..r1 {
            if self.ef[n].abs() > sf && self.eb[n].abs() > sb {
                match self.runs.last_mut() {
                    Some(last) if n <= last.1 + bridge => last.1 = n,
                    _ => self.runs.push((n, n)),
                }
            }
        }
        let (ef, eb) = (&self.ef, &self.eb);
        self.runs.retain_mut(|run| {
            let (s, t) = (run.0 - WIDEN, run.1 + WIDEN);
            if t - s + 1 > longest {
                return false;
            }
            let peak = (s..=t)
                .map(|n| ef[n].abs().min(eb[n].abs()))
                .fold(0.0, f64::max);
            // The error around the click, beyond the reach of its own
            let around = ef[s - CONTEXT - p..s - p]
                .iter()
                .chain(&ef[t + p + 1..t + p + 1 + CONTEXT]);
            let rms = (around.map(|e| e * e).sum::<f64>() / (2 * CONTEXT) as f64).sqrt();
            if peak < strength.iso * rms || peak < MIN_PEAK {
                return false;
            }
            *run = (s + e0, t + e0);
            true
        });
    }

    /// Rebuild `x[s..=t]` from the samples around it (least-squares AR
    /// interpolation) with the model last fitted
    fn interpolate(&mut self, x: &mut [f64], s: usize, t: usize) {
        let p = self.order;
        let m = t - s + 1;
        for d in 0..=p {
            self.ra[d] = dot(&self.a[..=p - d], &self.a[d..]);
        }
        let ra = &self.ra;
        let rr = |d: usize| if d <= p { ra[d] } else { 0.0 };
        let mat = &mut self.matrix[..m * m];
        let rhs = &mut self.rhs[..m];
        for j in 0..m {
            for l in 0..m {
                mat[j * m + l] = rr(j.abs_diff(l));
            }
            let n = s + j;
            // The good samples within the model's reach, either side
            let before = x.iter().enumerate().take(s).skip(n.saturating_sub(p));
            let after = x.iter().enumerate().take(n + p + 1).skip(t + 1);
            rhs[j] = -before
                .chain(after)
                .map(|(q, v)| rr(n.abs_diff(q)) * v)
                .sum::<f64>();
        }
        // Cholesky, in place in the lower triangle
        for i in 0..m {
            for j in 0..=i {
                let mut sum = mat[i * m + j];
                for k in 0..j {
                    sum -= mat[i * m + k] * mat[j * m + k];
                }
                if i == j {
                    mat[i * m + i] = sum.max(1e-18).sqrt();
                } else {
                    mat[i * m + j] = sum / mat[j * m + j];
                }
            }
        }
        // Forward, then back substitution
        for i in 0..m {
            let mut sum = rhs[i];
            for k in 0..i {
                sum -= mat[i * m + k] * rhs[k];
            }
            rhs[i] = sum / mat[i * m + i];
        }
        for i in (0..m).rev() {
            let mut sum = rhs[i];
            for k in i + 1..m {
                sum -= mat[k * m + i] * x[s + k];
            }
            x[s + i] = sum / mat[i * m + i];
        }
    }
}

struct Crackle {
    channels: usize,
    block: usize,
    reach: usize,
    sample_rate: u32,
    /// Per channel: `reach` samples before the block, the block, `reach`
    /// after it
    buf: Vec<Vec<f64>>,
    /// Samples in `buf`
    filled: usize,
    /// Where in `buf` the next sample to give back is
    next_out: usize,
    /// Whether a block has been looked at yet: the first one's past is
    /// padding, not sound
    started: bool,
    work: Work,
    frames_in: u64,
    frames_out: u64,
    /// Clicks repaired, for the tests
    repairs: u64,
}

impl Crackle {
    fn new(channels: usize, sample_rate: u32) -> Self {
        let block = block_len(sample_rate);
        let order = order(sample_rate);
        let reach = reach(block, order);
        // The interpolation's matrix fits the longest click either
        // strength repairs
        let longest = [CrackleLevel::Light, CrackleLevel::Strong]
            .into_iter()
            .filter_map(strength)
            .map(|s| s.longest(sample_rate))
            .max()
            .unwrap_or(4);
        let len = reach + block + reach;
        Self {
            channels,
            block,
            reach,
            sample_rate,
            buf: vec![vec![0.0; len]; channels],
            // The past starts out as silence
            filled: reach,
            next_out: reach,
            started: false,
            work: Work::new(
                order,
                2 * block,
                block + 2 * (WIDEN + CONTEXT + order),
                longest,
            ),
            frames_in: 0,
            frames_out: 0,
            repairs: 0,
        }
    }

    /// Take one frame in; every block, a block comes out
    fn push(&mut self, frame: &[f32], level: CrackleLevel, out: &mut VecDeque<f32>) {
        self.frames_in += 1;
        self.take(frame, level, out);
    }

    fn take(&mut self, frame: &[f32], level: CrackleLevel, out: &mut VecDeque<f32>) {
        for (c, buf) in self.buf.iter_mut().enumerate() {
            let x = frame.get(c).copied().unwrap_or(0.0);
            buf[self.filled] = if x.is_finite() { f64::from(x) } else { 0.0 };
        }
        self.filled += 1;
        if self.filled == self.buf[0].len() {
            self.block(level, out);
        }
    }

    /// Give back everything still held, with silence pushed in after it
    fn flush(&mut self, out: &mut VecDeque<f32>) {
        let silence = vec![0.0; self.channels];
        while self.frames_out < self.frames_in {
            self.take(&silence, CrackleLevel::Off, out);
        }
    }

    fn block(&mut self, level: CrackleLevel, out: &mut VecDeque<f32>) {
        let b0 = self.reach;
        let b1 = b0 + self.block;
        if let Some(strength) = strength(level).filter(|_| self.started) {
            for c in 0..self.channels {
                self.repairs += self.repair(c, b0, b1, strength);
            }
        }
        self.started = true;
        // The next block may still widen a click back into the last
        // samples of this one
        let done = b1 - WIDEN;
        for i in self.next_out..done {
            if self.frames_out >= self.frames_in {
                break;
            }
            self.frames_out += 1;
            out.extend(self.buf.iter().map(|buf| buf[i] as f32));
        }
        for buf in &mut self.buf {
            buf.copy_within(self.block.., 0);
        }
        self.filled -= self.block;
        self.next_out = done - self.block;
    }

    /// Find and repair the clicks in one channel's block; how many
    fn repair(&mut self, c: usize, b0: usize, b1: usize, strength: Strength) -> u64 {
        let buf = &mut self.buf[c];
        let work = &mut self.work;
        let w0 = b0 - self.block / 2;
        let w1 = b1 + self.block / 2;
        work.fit(&buf[w0..w1]);
        let longest = strength.longest(self.sample_rate);
        work.detect(buf, b0, b1, strength, longest);
        if work.runs.is_empty() {
            return 0;
        }
        // The clicks pulled the model towards them: fit it again with them
        // provisionally repaired, and look again
        work.repaired.copy_from_slice(&buf[w0..w1]);
        let mut repaired = std::mem::take(&mut work.repaired);
        let runs = std::mem::take(&mut work.runs);
        for &(s, t) in &runs {
            work.interpolate(&mut repaired, s - w0, t - w0);
        }
        work.runs = runs;
        work.fit(&repaired);
        work.repaired = repaired;
        work.detect(buf, b0, b1, strength, longest);
        let runs = std::mem::take(&mut work.runs);
        for &(s, t) in &runs {
            work.interpolate(buf, s, t);
        }
        let found = runs.len() as u64;
        work.runs = runs;
        found
    }
}

/// A `Source` adapter for the Crackle Filter (see the module docs). It
/// reads about 35 ms ahead of what it gives back, on or off.
pub struct CrackleSource<S> {
    inner: S,
    switches: Arc<FilterSwitches>,
    /// None with more channels than it handles: those pass through
    crackle: Option<Crackle>,
    /// The format of what is in `out`
    channels: NonZero<u16>,
    sample_rate: NonZero<u32>,
    frame: Vec<f32>,
    out: VecDeque<f32>,
    /// The first sample in a new format, read while `out` still held
    /// audio in the old one
    carry: Option<f32>,
    ended: bool,
}

impl<S> CrackleSource<S>
where
    S: Source<Item = f32>,
{
    pub fn new(inner: S, switches: Arc<FilterSwitches>) -> Self {
        let channels = inner.channels();
        let sample_rate = inner.sample_rate();
        Self {
            crackle: Self::crackle(channels, sample_rate),
            inner,
            switches,
            channels,
            sample_rate,
            frame: Vec::with_capacity(MAX_CHANNELS),
            out: VecDeque::new(),
            carry: None,
            ended: false,
        }
    }

    fn crackle(channels: NonZero<u16>, sample_rate: NonZero<u32>) -> Option<Crackle> {
        let channels = usize::from(channels.get());
        (channels <= MAX_CHANNELS).then(|| Crackle::new(channels, sample_rate.get()))
    }

    /// Clicks repaired so far in this format
    #[cfg(test)]
    fn repairs(&self) -> u64 {
        self.crackle.as_ref().map_or(0, |c| c.repairs)
    }

    fn flush(&mut self) {
        if let Some(crackle) = &mut self.crackle {
            crackle.flush(&mut self.out);
        }
    }

    /// Read the next frame; false at the end of the stream. Only called
    /// with `out` empty.
    fn fill(&mut self) -> bool {
        let first = match self.carry.take() {
            Some(x) => x,
            None => match self.inner.next() {
                Some(x) => x,
                None => {
                    self.flush();
                    return false;
                }
            },
        };
        // The format can change between frames (see EqSource): play out
        // what is held back, then start over in the new format
        let channels = self.inner.channels();
        let sample_rate = self.inner.sample_rate();
        if channels != self.channels || sample_rate != self.sample_rate {
            self.flush();
            if !self.out.is_empty() {
                self.carry = Some(first);
                return true;
            }
            self.channels = channels;
            self.sample_rate = sample_rate;
            self.crackle = Self::crackle(channels, sample_rate);
        }
        self.frame.clear();
        self.frame.push(first);
        for _ in 1..channels.get() {
            match self.inner.next() {
                Some(x) => self.frame.push(x),
                None => break,
            }
        }
        match &mut self.crackle {
            Some(crackle) => crackle.push(&self.frame, self.switches.crackle(), &mut self.out),
            None => self.out.extend(self.frame.iter().copied()),
        }
        true
    }
}

impl<S> Iterator for CrackleSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if let Some(x) = self.out.pop_front() {
                return Some(x);
            }
            if self.ended {
                return None;
            }
            if !self.fill() {
                self.ended = true;
            }
        }
    }
}

impl<S> Source for CrackleSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        // What is waiting is all in one format; the format is looked at
        // again after it
        Some(self.out.len())
    }

    fn channels(&self) -> NonZero<u16> {
        self.channels
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

#[cfg(test)]
#[path = "crackle_test.rs"]
mod tests;
