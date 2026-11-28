//! The Hiss Reducer: turns down the steady hiss or static some stations
//! carry in the background.
//!
//! The sound is cut into overlapping blocks of about 23 ms, whatever the
//! sample rate, and each block into frequency bands (a short-time Fourier
//! transform with a square-root Hann window, half overlapped). Each band
//! keeps the quietest level it had over the last few seconds: music comes
//! and goes, the hiss under it stays, so that minimum is the hiss. Bands
//! that are hardly above it are turned down, by at most [`HISS_LIGHT_DB`]
//! or [`HISS_STRONG_DB`]; bands well above it pass. Only the treble is
//! touched (from [`HISS_FROM_HZ`], fully from [`HISS_FULL_HZ`]), where hiss
//! is heard and voices and instruments are thin. Gains are smoothed across
//! bands and over time so the leftover hiss doesn't warble.
//!
//! That rule alone eats music: steady treble (hi-hats, pads, reverb tails,
//! cymbal wash) sits within a few dB of its own quietest level too, and was
//! pushed down with the hiss. Two things hold it back:
//!
//! - **The masking limit.** Hiss under music is only heard where it rises
//!   above what the music masks. The threshold is the block's spectrum
//!   spread over a third of an octave, [`HISS_MASK_LIGHT_DB`] or
//!   [`HISS_MASK_STRONG_DB`] down, and it fades over about 60 ms after the
//!   sound that made it (forward masking). A band is never turned down
//!   further than it takes to bring its hiss under that, so where music
//!   plays the cut is small, and in the pauses, where the hiss is exposed,
//!   it is full.
//! - **The texture guard.** Some music's treble is itself a steady noise
//!   (dense electronic music, a wash of cymbals), and no single band can
//!   tell it from hiss. The whole picture can: over about 6 s, the "hiss"
//!   is loud against the programme yet nowhere near all of it (hiss alone
//!   would be), and the treble hardly ever rises above it. Then the floor
//!   is the music's texture and the reduction fades out, over about 2 s.
//!
//! RES-3 measured these on 7 clean and hissy tracks: the damage to clean
//! music was about halved, and more hiss came off as well.
//!
//! Off, the blocks are put back together without the transform, which
//! gives back the same samples half a block later. Switching on or off
//! therefore doesn't jump, and Off costs next to nothing.

use std::collections::VecDeque;
use std::num::NonZero;
use std::sync::Arc;
use std::time::Duration;

use rodio::Source;
use rustfft::{num_complex::Complex, Fft, FftPlanner};

use super::sound::{FilterSwitches, HissLevel};
use crate::config::sound::{
    HISS_FROM_HZ, HISS_FULL_HZ, HISS_LIGHT_DB, HISS_MASK_LIGHT_DB, HISS_MASK_STRONG_DB,
    HISS_MEMORY_SECS, HISS_STRONG_DB,
};

/// More channels than this pass through as they are
const MAX_CHANNELS: usize = 8;
/// How much of a band's last level is kept each block
const SMOOTH: f32 = 0.8;
/// The quietest level is below the hiss's average; this brings it up
const BIAS: f32 = 2.0;
/// The memory is kept in this many parts, so it can move on part by part
const PARTS: usize = 8;
/// A band's hiss is at most this much above the bands around it, so a
/// note held for seconds isn't taken for hiss
const TONE_CAP: f32 = 1.5;
/// Bands each side looked at for that
const TONE_SPAN: usize = 7;
/// How fast a band's gain comes back up and goes down, per block
const RISE: f32 = 0.6;
const FALL: f32 = 0.15;
/// A gain this close to 1 counts as untouched
const UNITY: f32 = 0.999;
/// About how long a block is (s): 1024 samples at 44.1 and 48 kHz, 512
/// at 22.05 kHz, so low-rate streams aren't smeared over twice the time
const BLOCK_SECS: f32 = 0.023;
/// How fast the masking threshold fades after the sound that made it (s)
const MASK_DECAY_SECS: f32 = 0.06;
/// A third of an octave each side, as a frequency ratio (2^(1/6))
const MASK_SPREAD: f32 = 1.122;
/// What the texture guard averages over (s)
const TEXTURE_SECS: f64 = 6.0;
/// How fast the guard comes on and goes off (s)
const TEXTURE_GLIDE_SECS: f32 = 2.0;
/// The guard starts when the treble's hiss is this loud against the
/// whole programme (dB), and is fully on 4 dB above...
const TEXTURE_LOUD_DB: f32 = -16.0;
/// ...and stops again as it nears the whole programme, as hiss alone
/// does: fully on 3 dB below this (dB)
const TEXTURE_ALL_DB: f32 = -3.0;
/// ...as long as the treble rises less than this above its hiss on
/// average (dB); fully on 2 dB below it
const TEXTURE_FLAT_DB: f32 = 4.0;

/// Block length for a sample rate: the power of two nearest
/// [`BLOCK_SECS`]
fn block_len(sample_rate: u32) -> usize {
    let want = BLOCK_SECS * sample_rate as f32;
    let mut len = 256;
    while (len as f32) < want * 0.75 {
        len *= 2;
    }
    len
}

/// What a strength does
#[derive(Clone, Copy)]
struct Strength {
    /// How much of the hiss is subtracted
    over: f32,
    /// The most a band is turned down (a gain)
    floor: f32,
    /// The masking threshold below the music's spectrum (a power ratio)
    mask: f32,
}

fn strength(level: HissLevel) -> Option<Strength> {
    let gain = |db: f32| 10.0f32.powf(db / 20.0);
    let power = |db: f32| 10.0f32.powf(-db / 10.0);
    match level {
        HissLevel::Off => None,
        HissLevel::Light => Some(Strength {
            over: 1.5,
            floor: gain(HISS_LIGHT_DB),
            mask: power(HISS_MASK_LIGHT_DB),
        }),
        HissLevel::Strong => Some(Strength {
            over: 2.5,
            floor: gain(HISS_STRONG_DB),
            mask: power(HISS_MASK_STRONG_DB),
        }),
    }
}

/// The texture guard (see the module docs)
struct Texture {
    /// How much of each block's average is kept
    keep: f64,
    /// Long averages of the whole programme's power and the treble's.
    /// They start from nothing, so for their first seconds they read the
    /// hiss as louder against the programme, and the treble as flatter,
    /// than it is: the guard leans toward leaving the treble alone until
    /// it has heard enough. That is how RES-3 measured it.
    programme: f64,
    treble: f64,
    /// How far the guard moves toward where it wants to be, per block
    glide: f32,
    /// 0 (no texture, reduce as usual) to 1 (texture, leave it alone)
    amount: f32,
}

impl Texture {
    fn new(block_secs: f32) -> Self {
        Self {
            keep: (-f64::from(block_secs) / TEXTURE_SECS).exp(),
            programme: 0.0,
            treble: 0.0,
            glide: (block_secs / TEXTURE_GLIDE_SECS).min(1.0),
            amount: 0.0,
        }
    }

    /// One block's power: the whole spectrum's, and the treble's
    fn listen(&mut self, programme: f64, treble: f64) {
        let new = 1.0 - self.keep;
        self.programme = self.keep * self.programme + new * programme;
        self.treble = self.keep * self.treble + new * treble;
    }

    /// Move toward what the treble's hiss estimate says
    fn judge(&mut self, hiss: f64) {
        let db = |ratio: f64| (10.0 * ratio.max(1e-20).log10()) as f32;
        let level = db(hiss / self.programme.max(1e-20));
        let rise = db(self.treble / hiss.max(1e-20));
        let loud = ((level - TEXTURE_LOUD_DB) / 4.0).clamp(0.0, 1.0)
            * ((TEXTURE_ALL_DB - level) / 3.0).clamp(0.0, 1.0);
        let flat = ((TEXTURE_FLAT_DB - rise) / 2.0).clamp(0.0, 1.0);
        self.amount += (loud * flat - self.amount) * self.glide;
    }

    fn forget(&mut self) {
        self.programme = 0.0;
        self.treble = 0.0;
        self.amount = 0.0;
    }
}

struct Hiss {
    channels: usize,
    len: usize,
    hop: usize,
    /// Square-root Hann, and its square (plain Hann)
    window: Vec<f32>,
    hann: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex<f32>>,
    /// Per channel: the last block of input; the second half fills up
    input: Vec<Vec<f32>>,
    filled: usize,
    /// Per channel: blocks being added together
    sum: Vec<Vec<f32>>,
    spectra: Vec<Vec<Complex<f32>>>,
    /// Per band (0 to len / 2): this block's power, and its average
    block_power: Vec<f32>,
    power: Vec<f32>,
    part_min: Vec<f32>,
    part_blocks: usize,
    blocks_per_part: usize,
    parts: VecDeque<Vec<f32>>,
    noise: Vec<f32>,
    noise_ready: bool,
    weight: Vec<f32>,
    /// The first band fully in the treble
    treble_from: usize,
    /// Per band: the bands its masking threshold is spread over, and the
    /// threshold left from the blocks before
    spread: Vec<(usize, usize)>,
    mask: Vec<f32>,
    mask_keep: f32,
    /// Running sums of `block_power`, for the spread
    sums: Vec<f64>,
    texture: Texture,
    target: Vec<f32>,
    gain: Vec<f32>,
    /// Whether the transform is in use (on, or gliding back to Off)
    spectral: bool,
    started: bool,
    /// Leading frames that are only the first block's padding
    skip: usize,
    frames_in: u64,
    frames_out: u64,
}

impl Hiss {
    fn new(channels: usize, sample_rate: u32) -> Self {
        let len = block_len(sample_rate);
        let hop = len / 2;
        let bands = hop + 1;
        let hann: Vec<f32> = (0..len)
            .map(|i| {
                let phase = std::f64::consts::TAU * i as f64 / len as f64;
                (0.5 - 0.5 * phase.cos()) as f32
            })
            .collect();
        let window = hann.iter().map(|w| w.sqrt()).collect();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(len);
        let ifft = planner.plan_fft_inverse(len);
        let scratch_len = fft
            .get_inplace_scratch_len()
            .max(ifft.get_inplace_scratch_len());
        let rate = sample_rate.max(1) as f32;
        let weight: Vec<f32> = (0..bands)
            .map(|k| {
                let hz = k as f32 * rate / len as f32;
                let t = ((hz - HISS_FROM_HZ) / (HISS_FULL_HZ - HISS_FROM_HZ)).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            })
            .collect();
        let treble_from = weight.iter().position(|&w| w >= 1.0).unwrap_or(bands);
        let spread = (0..bands)
            .map(|k| {
                let lo = (k as f32 / MASK_SPREAD).floor() as usize;
                let hi = ((k as f32 * MASK_SPREAD).ceil() as usize).min(bands - 1);
                (lo, hi.max(lo))
            })
            .collect();
        let block_secs = hop as f32 / rate;
        let blocks = HISS_MEMORY_SECS / block_secs;
        Self {
            channels,
            len,
            hop,
            window,
            hann,
            fft,
            ifft,
            scratch: vec![Complex::default(); scratch_len],
            input: vec![vec![0.0; len]; channels],
            filled: 0,
            sum: vec![vec![0.0; len]; channels],
            spectra: vec![vec![Complex::default(); len]; channels],
            block_power: vec![0.0; bands],
            power: vec![0.0; bands],
            part_min: vec![f32::INFINITY; bands],
            part_blocks: 0,
            blocks_per_part: ((blocks / PARTS as f32).round() as usize).max(1),
            parts: VecDeque::with_capacity(PARTS + 1),
            noise: vec![0.0; bands],
            noise_ready: false,
            weight,
            treble_from,
            spread,
            mask: vec![0.0; bands],
            mask_keep: (-block_secs / MASK_DECAY_SECS).exp(),
            sums: vec![0.0; bands + 1],
            texture: Texture::new(block_secs),
            target: vec![1.0; bands],
            gain: vec![1.0; bands],
            spectral: false,
            started: false,
            skip: hop,
            frames_in: 0,
            frames_out: 0,
        }
    }

    /// Take one frame in; every half block, half a block comes out
    fn push(&mut self, frame: &[f32], level: HissLevel, out: &mut VecDeque<f32>) {
        self.frames_in += 1;
        self.take(frame, level, out);
    }

    fn take(&mut self, frame: &[f32], level: HissLevel, out: &mut VecDeque<f32>) {
        let at = self.hop + self.filled;
        for (c, input) in self.input.iter_mut().enumerate() {
            let x = frame.get(c).copied().unwrap_or(0.0);
            input[at] = if x.is_finite() { x } else { 0.0 };
        }
        self.filled += 1;
        if self.filled == self.hop {
            self.filled = 0;
            self.block(level, out);
        }
    }

    /// Give back everything still held, with silence pushed in after it
    fn flush(&mut self, out: &mut VecDeque<f32>) {
        let silence = vec![0.0; self.channels];
        while self.frames_out < self.frames_in {
            self.take(&silence, HissLevel::Off, out);
        }
    }

    fn block(&mut self, level: HissLevel, out: &mut VecDeque<f32>) {
        let want = strength(level);
        if want.is_some() && !self.spectral {
            self.spectral = true;
            self.gain.fill(1.0);
        }
        if self.spectral {
            self.spectral_block(want);
        } else {
            for (input, sum) in self.input.iter().zip(&mut self.sum) {
                for ((s, x), w) in sum.iter_mut().zip(input).zip(&self.hann) {
                    *s += x * w;
                }
            }
        }
        // The first half is complete now
        for i in 0..self.hop {
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            if self.frames_out >= self.frames_in {
                break;
            }
            self.frames_out += 1;
            out.extend(self.sum.iter().map(|sum| sum[i]));
        }
        for (input, sum) in self.input.iter_mut().zip(&mut self.sum) {
            input.copy_within(self.hop.., 0);
            sum.copy_within(self.hop.., 0);
            sum[self.hop..].fill(0.0);
        }
    }

    fn spectral_block(&mut self, want: Option<Strength>) {
        let len = self.len;
        let hop = self.hop;
        for (input, spectrum) in self.input.iter().zip(&mut self.spectra) {
            for ((s, x), w) in spectrum.iter_mut().zip(input).zip(&self.window) {
                *s = Complex::new(x * w, 0.0);
            }
            self.fft.process_with_scratch(spectrum, &mut self.scratch);
        }
        self.learn();
        match want {
            Some(strength) if self.noise_ready => self.aim(strength),
            _ => self.target.fill(1.0),
        }
        let mut done = want.is_none();
        for k in 0..=hop {
            let before = self.target[k.saturating_sub(1)];
            let after = self.target[(k + 1).min(hop)];
            let t = 0.25 * before + 0.5 * self.target[k] + 0.25 * after;
            let g = self.gain[k];
            let speed = if t > g { RISE } else { FALL };
            self.gain[k] = g + speed * (t - g);
            done &= self.gain[k] >= UNITY;
        }
        let scale = 1.0 / len as f32;
        for (spectrum, sum) in self.spectra.iter_mut().zip(&mut self.sum) {
            spectrum[0] *= self.gain[0];
            spectrum[hop] *= self.gain[hop];
            for k in 1..hop {
                spectrum[k] *= self.gain[k];
                spectrum[len - k] *= self.gain[k];
            }
            self.ifft.process_with_scratch(spectrum, &mut self.scratch);
            for ((s, x), w) in sum.iter_mut().zip(spectrum.iter()).zip(&self.window) {
                *s += x.re * w * scale;
            }
        }
        if done {
            // Back at unity: put the blocks together the cheap way again
            // and forget the hiss, which may be another station's by the
            // time it is switched on again
            self.spectral = false;
            self.forget();
        }
    }

    /// Each band's gain to aim for: the hiss subtracted, but no further
    /// than the masking limit, and the floor raised by the texture guard
    fn aim(&mut self, strength: Strength) {
        let treble_hiss: f64 = self.noise[self.treble_from..]
            .iter()
            .map(|&n| f64::from(n))
            .sum();
        self.texture.judge(treble_hiss);
        // A gain's dB times (1 - amount)
        let floor = strength.floor.powf(1.0 - self.texture.amount);
        for k in 0..=self.hop {
            self.sums[k + 1] = self.sums[k] + f64::from(self.block_power[k]);
        }
        for k in 0..=self.hop {
            let p = self.power[k];
            let noise = self.noise[k].max(1e-20);
            let mut g = if p > 1e-12 {
                (1.0 - strength.over * noise / p).max(0.0)
            } else {
                1.0
            };
            let (lo, hi) = self.spread[k];
            let spread = ((self.sums[hi + 1] - self.sums[lo]) / (hi + 1 - lo) as f64) as f32;
            let mask = (spread * strength.mask).max(self.mask[k] * self.mask_keep);
            self.mask[k] = mask;
            // The hiss left, g² times the hiss, needn't go under the mask
            g = g.max((mask / noise).sqrt().min(1.0));
            let g = g.clamp(floor, 1.0);
            self.target[k] = 1.0 - self.weight[k] * (1.0 - g);
        }
    }

    /// Follow each band's level and its quietest level of late
    fn learn(&mut self) {
        let share = 1.0 / self.channels.max(1) as f32;
        let (mut programme, mut treble) = (0.0f64, 0.0f64);
        for k in 0..=self.hop {
            let p = self.spectra.iter().map(|s| s[k].norm_sqr()).sum::<f32>() * share;
            self.block_power[k] = p;
            programme += f64::from(p);
            if k >= self.treble_from {
                treble += f64::from(p);
            }
            self.power[k] = if self.started {
                SMOOTH * self.power[k] + (1.0 - SMOOTH) * p
            } else {
                p
            };
            self.part_min[k] = self.part_min[k].min(self.power[k]);
        }
        self.texture.listen(programme, treble);
        self.started = true;
        self.part_blocks += 1;
        if self.part_blocks < self.blocks_per_part {
            return;
        }
        self.part_blocks = 0;
        let part = std::mem::replace(&mut self.part_min, vec![f32::INFINITY; self.hop + 1]);
        self.parts.push_back(part);
        if self.parts.len() > PARTS {
            self.parts.pop_front();
        }
        self.noise_ready = self.parts.len() >= PARTS / 2;
        let bands = self.hop + 1;
        let lowest: Vec<f32> = (0..bands)
            .map(|k| {
                BIAS * self
                    .parts
                    .iter()
                    .map(|p| p[k])
                    .fold(f32::INFINITY, f32::min)
            })
            .collect();
        let mut near = Vec::with_capacity(2 * TONE_SPAN + 1);
        for k in 0..bands {
            near.clear();
            near.extend_from_slice(
                &lowest[k.saturating_sub(TONE_SPAN)..(k + TONE_SPAN + 1).min(bands)],
            );
            near.sort_unstable_by(f32::total_cmp);
            let median = near[near.len() / 2];
            self.noise[k] = lowest[k].min(TONE_CAP * median);
        }
    }

    fn forget(&mut self) {
        self.started = false;
        self.part_min.fill(f32::INFINITY);
        self.part_blocks = 0;
        self.parts.clear();
        self.noise_ready = false;
        self.mask.fill(0.0);
        self.texture.forget();
    }
}

/// A `Source` adapter for the Hiss Reducer (see the module docs). It holds
/// the audio back by half a block, about 12 ms at any rate, on or off.
pub struct HissSource<S> {
    inner: S,
    switches: Arc<FilterSwitches>,
    /// None with more channels than it handles: those pass through
    hiss: Option<Hiss>,
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

impl<S> HissSource<S>
where
    S: Source<Item = f32>,
{
    pub fn new(inner: S, switches: Arc<FilterSwitches>) -> Self {
        let channels = inner.channels();
        let sample_rate = inner.sample_rate();
        Self {
            hiss: Self::hiss(channels, sample_rate),
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

    fn hiss(channels: NonZero<u16>, sample_rate: NonZero<u32>) -> Option<Hiss> {
        let channels = usize::from(channels.get());
        (channels <= MAX_CHANNELS).then(|| Hiss::new(channels, sample_rate.get()))
    }

    fn flush(&mut self) {
        if let Some(hiss) = &mut self.hiss {
            hiss.flush(&mut self.out);
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
            self.hiss = Self::hiss(channels, sample_rate);
        }
        self.frame.clear();
        self.frame.push(first);
        for _ in 1..channels.get() {
            match self.inner.next() {
                Some(x) => self.frame.push(x),
                None => break,
            }
        }
        match &mut self.hiss {
            Some(hiss) => hiss.push(&self.frame, self.switches.hiss(), &mut self.out),
            None => self.out.extend(self.frame.iter().copied()),
        }
        true
    }
}

impl<S> Iterator for HissSource<S>
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

impl<S> Source for HissSource<S>
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
mod tests {
    use super::*;
    use crate::audio::dsp::filter::Biquad64;
    use crate::audio::dsp::sound::SoundParams;
    use rodio::buffer::SamplesBuffer;
    use std::f32::consts::TAU;

    const RATE: u32 = 48_000;

    /// Mono test signal: tones (Hz, amplitude) plus white noise
    fn signal(secs: f32, tones: &[(f32, f32)], noise: f32) -> Vec<f32> {
        let mut seed = 0x1234_5678u32;
        (0..(secs * RATE as f32) as usize)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let n = (seed as f32 / u32::MAX as f32 - 0.5) * 2.0 * noise;
                tones
                    .iter()
                    .map(|&(f, a)| a * (TAU * f * t).sin())
                    .sum::<f32>()
                    + n
            })
            .collect()
    }

    fn source(
        channels: u16,
        rate: u32,
        samples: Vec<f32>,
        params: &SoundParams,
    ) -> impl Iterator<Item = f32> {
        HissSource::new(
            SamplesBuffer::new(
                NonZero::new(channels).unwrap(),
                NonZero::new(rate).unwrap(),
                samples,
            ),
            params.switches(),
        )
    }

    fn run(samples: Vec<f32>, level: HissLevel) -> Vec<f32> {
        let mut params = SoundParams::default();
        params.set_hiss(level);
        source(1, RATE, samples, &params).collect()
    }

    fn tail(x: &[f32], secs: f32) -> &[f32] {
        &x[x.len() - (secs * RATE as f32) as usize..]
    }

    /// Amplitude of `freq` in `x`
    fn level(x: &[f32], freq: f32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &v) in x.iter().enumerate() {
            let ph = f64::from(TAU) * f64::from(freq) * i as f64 / f64::from(RATE);
            re += f64::from(v) * ph.cos();
            im += f64::from(v) * ph.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    /// Average amplitude over a spread of treble frequencies
    fn treble(x: &[f32]) -> f32 {
        let freqs: Vec<f32> = (0..40).map(|i| 5_000.0 + 237.0 * i as f32).collect();
        freqs.iter().map(|&f| level(x, f)).sum::<f32>() / freqs.len() as f32
    }

    fn db(ratio: f32) -> f32 {
        20.0 * ratio.log10()
    }

    #[test]
    fn off_gives_back_the_same_samples() {
        let input = signal(2.0, &[(440.0, 0.3), (3_000.0, 0.2)], 0.05);
        let out = run(input.clone(), HissLevel::Off);
        assert_eq!(out.len(), input.len());
        let worst = out
            .iter()
            .zip(&input)
            .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(worst < 1e-5, "{worst}");
    }

    #[test]
    fn strong_turns_hiss_down() {
        let input = signal(8.0, &[], 0.05);
        let out = run(input.clone(), HissLevel::Strong);
        assert_eq!(out.len(), input.len());
        let drop = db(treble(tail(&out, 3.0)) / treble(tail(&input, 3.0)));
        assert!(drop < -9.0, "{drop} dB");
    }

    #[test]
    fn light_is_gentler_than_strong() {
        let input = signal(8.0, &[], 0.05);
        let light = treble(tail(&run(input.clone(), HissLevel::Light), 3.0));
        let strong = treble(tail(&run(input.clone(), HissLevel::Strong), 3.0));
        let before = treble(tail(&input, 3.0));
        let light_db = db(light / before);
        let strong_db = db(strong / before);
        // Hiss alone: Light goes most of the way to its -9 dB floor
        assert!(light_db < -6.0 && light_db > -9.5, "{light_db} dB");
        assert!(strong_db < light_db - 2.0, "{strong_db} dB");
    }

    #[test]
    fn the_bass_and_middle_are_left_alone() {
        let input = signal(8.0, &[], 0.05);
        let out = run(input.clone(), HissLevel::Strong);
        let freqs: Vec<f32> = (0..20).map(|i| 100.0 + 41.0 * i as f32).collect();
        let band = |x: &[f32]| freqs.iter().map(|&f| level(x, f)).sum::<f32>();
        let change = db(band(tail(&out, 3.0)) / band(tail(&input, 3.0)));
        assert!(change.abs() < 0.5, "{change} dB");
    }

    #[test]
    fn a_held_note_keeps_its_level_while_the_hiss_goes() {
        let input = signal(8.0, &[(3_000.0, 0.3)], 0.03);
        let out = run(input.clone(), HissLevel::Strong);
        let note = db(level(tail(&out, 2.0), 3_000.0) / level(tail(&input, 2.0), 3_000.0));
        assert!(note.abs() < 1.0, "note {note} dB");
        let hiss = db(treble(tail(&out, 2.0)) / treble(tail(&input, 2.0)));
        assert!(hiss < -4.0, "hiss {hiss} dB");
    }

    #[test]
    fn clean_music_passes() {
        let input = signal(
            8.0,
            &[(1_000.0, 0.2), (5_000.0, 0.1), (11_000.0, 0.05)],
            0.0,
        );
        let out = run(input.clone(), HissLevel::Strong);
        for f in [1_000.0, 5_000.0, 11_000.0] {
            let change = db(level(tail(&out, 2.0), f) / level(tail(&input, 2.0), f));
            assert!(change.abs() < 1.0, "{f} Hz {change} dB");
        }
    }

    #[test]
    fn switching_doesnt_jump() {
        let input = signal(9.0, &[(3_000.0, 0.3)], 0.005);
        let mut params = SoundParams::default();
        let mut source = source(1, RATE, input.clone(), &params);
        let mut out = Vec::with_capacity(input.len());
        for i in 0..input.len() {
            if i == 2 * RATE as usize {
                params.set_hiss(HissLevel::Strong);
            }
            if i == 6 * RATE as usize {
                params.set_hiss(HissLevel::Off);
            }
            out.push(source.next().unwrap());
        }
        assert!(source.next().is_none());
        let step = |x: &[f32]| x.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()));
        assert!(
            step(&out) < step(&input) * 1.2,
            "{} vs {}",
            step(&out),
            step(&input)
        );
        // Back off, it is the input again
        let worst = tail(&out, 1.0)
            .iter()
            .zip(tail(&input, 1.0))
            .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(worst < 1e-5, "{worst}");
    }

    #[test]
    fn silence_and_bad_samples_come_out_as_silence() {
        let mut input = vec![0.0; RATE as usize * 4];
        input[1_000] = f32::NAN;
        input[2_000] = f32::INFINITY;
        let out = run(input, HissLevel::Strong);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn stereo_and_high_rates_keep_their_length() {
        let params = SoundParams::default();
        let stereo: Vec<f32> = signal(1.0, &[(440.0, 0.3)], 0.01)
            .into_iter()
            .flat_map(|x| [x, -x])
            .collect();
        let out: Vec<f32> = source(2, 96_000, stereo.clone(), &params).collect();
        assert_eq!(out.len(), stereo.len());
        assert!(out.chunks(2).all(|f| (f[0] + f[1]).abs() < 1e-5));
    }

    /// Mono first, then stereo: each sample comes with the format it is in
    #[test]
    fn a_format_change_plays_out_and_starts_over() {
        struct Two {
            left: usize,
        }
        impl Iterator for Two {
            type Item = f32;
            fn next(&mut self) -> Option<f32> {
                if self.left == 0 {
                    return None;
                }
                self.left -= 1;
                Some(if self.left >= 1_000 {
                    0.1
                } else if self.left.is_multiple_of(2) {
                    0.2
                } else {
                    -0.2
                })
            }
        }
        impl Source for Two {
            fn current_span_len(&self) -> Option<usize> {
                None
            }
            fn channels(&self) -> NonZero<u16> {
                NonZero::new(if self.left >= 1_000 { 1 } else { 2 }).unwrap()
            }
            fn sample_rate(&self) -> NonZero<u32> {
                NonZero::new(48_000).unwrap()
            }
            fn total_duration(&self) -> Option<Duration> {
                None
            }
        }
        let params = SoundParams::default();
        let mut source = HissSource::new(Two { left: 3_000 }, params.switches());
        let mut out = Vec::new();
        while let Some(x) = source.next() {
            let want = if out.len() < 2_000 { 1 } else { 2 };
            assert_eq!(source.channels().get(), want, "sample {}", out.len());
            out.push(x);
        }
        assert_eq!(out.len(), 3_000);
        assert!(out[1_000..2_000].iter().all(|&x| (x - 0.1).abs() < 1e-5));
        let tail = &out[2_600..2_700];
        assert!(tail.windows(2).all(|w| w[0] * w[1] < 0.0));
    }

    // Signals at any rate, for the tests below

    /// White noise, `amplitude` peak, from `seed`
    fn noise(frames: usize, amplitude: f32, mut seed: u32) -> Vec<f32> {
        (0..frames)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 - 0.5) * 2.0 * amplitude
            })
            .collect()
    }

    /// `x` through a 4th-order high-pass at `hz`
    fn high_passed(x: &[f32], rate: u32, hz: f64) -> Vec<f32> {
        let mut a = Biquad64::high_pass(hz, f64::from(rate));
        let mut b = Biquad64::high_pass(hz, f64::from(rate));
        x.iter()
            .map(|&v| b.run(a.run(f64::from(v))) as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// How loud the treble above `hz` is, over the last `secs`
    fn treble_rms(x: &[f32], rate: u32, hz: f64, secs: f32) -> f32 {
        let hp = high_passed(x, rate, hz);
        rms(&hp[hp.len() - (secs * rate as f32) as usize..])
    }

    fn run_at(rate: u32, channels: u16, samples: Vec<f32>, level: HissLevel) -> Vec<f32> {
        let mut params = SoundParams::default();
        params.set_hiss(level);
        source(channels, rate, samples, &params).collect()
    }

    /// A harmonic note: `f0` with `harmonics` overtones falling 3 dB each
    fn note(t: f32, f0: f32, harmonics: usize, rate: u32) -> f32 {
        (1..=harmonics)
            .map(|h| {
                let f = f0 * h as f32;
                if f < rate as f32 / 2.2 {
                    0.7f32.powi(h as i32 - 1) * (TAU * f * t).sin()
                } else {
                    0.0
                }
            })
            .sum()
    }

    #[test]
    fn blocks_are_the_same_length_in_time_at_every_rate() {
        assert_eq!(block_len(22_050), 512);
        assert_eq!(block_len(44_100), 1024);
        assert_eq!(block_len(48_000), 1024);
        assert_eq!(block_len(96_000), 2048);
        for rate in [22_050, 44_100, 48_000] {
            // Held back by half a block: about 11-12 ms everywhere
            let ms = (block_len(rate) / 2) as f32 * 1000.0 / rate as f32;
            assert!((10.5..12.0).contains(&ms), "{rate} Hz: {ms} ms");
        }
    }

    #[test]
    fn hiss_alone_is_turned_down_at_every_rate() {
        for rate in [22_050, 44_100, 48_000] {
            let input = noise(rate as usize * 8, 0.05, 7);
            let before = treble_rms(&input, rate, 3_000.0, 3.0);
            let light = treble_rms(
                &run_at(rate, 1, input.clone(), HissLevel::Light),
                rate,
                3_000.0,
                3.0,
            );
            let strong = treble_rms(
                &run_at(rate, 1, input.clone(), HissLevel::Strong),
                rate,
                3_000.0,
                3.0,
            );
            let (light, strong) = (db(light / before), db(strong / before));
            assert!(light < -6.0, "{rate} Hz Light {light} dB");
            assert!(strong < -9.5, "{rate} Hz Strong {strong} dB");
        }
    }

    #[test]
    fn stereo_channels_are_cleaned_and_kept_apart() {
        let rate = 44_100;
        let frames = rate as usize * 8;
        let left = noise(frames, 0.05, 11);
        let right = noise(frames, 0.05, 23);
        let tone: Vec<f32> = (0..frames)
            .map(|i| 0.3 * (TAU * 3_000.0 * i as f32 / rate as f32).sin())
            .collect();
        let input: Vec<f32> = (0..frames)
            .flat_map(|i| [left[i] + tone[i], right[i]])
            .collect();
        let out = run_at(rate, 2, input.clone(), HissLevel::Strong);
        assert_eq!(out.len(), input.len());
        let channel =
            |x: &[f32], c: usize| x.iter().skip(c).step_by(2).copied().collect::<Vec<_>>();
        let (in_r, out_r) = (channel(&input, 1), channel(&out, 1));
        let drop =
            db(treble_rms(&out_r, rate, 5_000.0, 3.0) / treble_rms(&in_r, rate, 5_000.0, 3.0));
        assert!(drop < -9.0, "right {drop} dB");
        // The tone is on the left only, and keeps its level there
        let at = |x: &[f32], c: usize| {
            let ch = channel(x, c);
            let tail = &ch[ch.len() - 2 * rate as usize..];
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, &v) in tail.iter().enumerate() {
                let ph = f64::from(TAU) * 3_000.0 * i as f64 / f64::from(rate);
                re += f64::from(v) * ph.cos();
                im += f64::from(v) * ph.sin();
            }
            (2.0 * (re * re + im * im).sqrt() / tail.len() as f64) as f32
        };
        let kept = db(at(&out, 0) / at(&input, 0));
        assert!(kept.abs() < 0.5, "left tone {kept} dB");
        assert!(
            at(&out, 1) < 0.003,
            "the tone leaked right: {}",
            at(&out, 1)
        );
    }

    #[test]
    fn a_tone_in_the_hiss_keeps_its_level() {
        let input = signal(8.0, &[(5_000.0, 0.2)], 0.05);
        for level in [HissLevel::Light, HissLevel::Strong] {
            let out = run(input.clone(), level);
            let change = db(level_of(&out, 5_000.0) / level_of(&input, 5_000.0));
            assert!(change.abs() < 0.5, "{level:?} {change} dB");
        }
    }

    fn level_of(x: &[f32], freq: f32) -> f32 {
        level(tail(x, 2.0), freq)
    }

    /// The masking limit: hiss beside a loud treble note is masked by it,
    /// so it is turned down less than hiss far from the note
    #[test]
    fn hiss_under_a_note_is_left_where_the_note_masks_it() {
        let input = signal(8.0, &[(6_000.0, 0.3)], 0.02);
        let out = run(input.clone(), HissLevel::Strong);
        let around = |x: &[f32], from: f32| {
            let t = tail(x, 2.0);
            (0..8)
                .map(|i| level(t, from + 37.0 * i as f32))
                .sum::<f32>()
        };
        let near = db(around(&out, 6_250.0) / around(&input, 6_250.0));
        let far = db(around(&out, 12_000.0) / around(&input, 12_000.0));
        assert!(far < -9.0, "far {far} dB");
        assert!(near > far + 3.0, "near {near} dB, far {far} dB");
    }

    /// The texture guard: a loud, steady noise in the treble over a bass
    /// line is the music's texture (a dense synth, a wash of cymbals). No
    /// one band can tell it from hiss, and before the guard Strong took
    /// about 12 dB off it.
    #[test]
    fn a_steady_noisy_texture_in_the_music_is_left_alone() {
        let rate = 44_100;
        let frames = rate as usize * 16;
        let texture = high_passed(&noise(frames, 0.14, 5), rate, 3_000.0);
        let bass = [55.0, 73.4, 82.4, 98.0];
        let input: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let f0 = bass[(t * 2.0) as usize % bass.len()];
                0.2 * note(t, f0, 4, rate) + texture[i]
            })
            .collect();
        // The texture is about 8 dB under the programme
        let share = db(rms(&texture) / rms(&input));
        assert!((-10.0..-6.0).contains(&share), "{share} dB");
        let mut hiss = Hiss::new(1, rate);
        let mut out = VecDeque::new();
        for &x in &input {
            hiss.push(&[x], HissLevel::Strong, &mut out);
        }
        assert!(hiss.texture.amount > 0.9, "guard {}", hiss.texture.amount);
        let out: Vec<f32> = out.into_iter().collect();
        let change =
            db(treble_rms(&out, rate, 4_000.0, 4.0) / treble_rms(&input, rate, 4_000.0, 4.0));
        assert!(change.abs() < 1.5, "texture {change} dB");
    }

    /// Hiss a good way under speech is still hiss: the guard stays off
    #[test]
    fn hiss_under_speech_still_goes() {
        let rate = 22_050;
        let frames = rate as usize * 12;
        let hiss = noise(frames, 0.03, 9);
        let input: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / rate as f32;
                // Syllables of 200 ms with short gaps, and a breath after
                // every sixth; each syllable swells and fades, as voices do
                let at = t % 0.3;
                let swell = if at < 0.2 && (t / 0.3) as u32 % 7 != 6 {
                    (std::f32::consts::PI * at / 0.2).sin().powi(2)
                } else {
                    0.0
                };
                let f0 = 110.0 + 9.0 * ((t / 0.3) as u32 % 5) as f32;
                0.25 * swell * note(t, f0, 24, rate) + hiss[i]
            })
            .collect();
        let mut state = Hiss::new(1, rate);
        let mut out = VecDeque::new();
        for &x in &input {
            state.push(&[x], HissLevel::Strong, &mut out);
        }
        assert!(state.texture.amount < 0.1, "guard {}", state.texture.amount);
        let out: Vec<f32> = out.into_iter().collect();
        // Above the voice's harmonics that matter: hiss
        let drop =
            db(treble_rms(&out, rate, 6_000.0, 4.0) / treble_rms(&input, rate, 6_000.0, 4.0));
        assert!(drop < -8.0, "{drop} dB");
    }

    /// Music damage, the regression RES-3 measured: clean, busy music
    /// (chords with bright overtones, a hi-hat on every eighth) with no
    /// hiss at all should come out with its treble about as it went in.
    /// The old rule took 2-9 dB off music like this at Strong.
    #[test]
    fn clean_busy_music_keeps_its_treble() {
        let rate = 44_100;
        let frames = rate as usize * 14;
        let hats = high_passed(&noise(frames, 0.6, 3), rate, 6_000.0);
        let pad = high_passed(&noise(frames, 0.02, 17), rate, 2_000.0);
        let chords = [
            [220.0, 277.2, 329.6],
            [196.0, 246.9, 293.7],
            [174.6, 220.0, 261.6],
        ];
        let input: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let chord = &chords[(t / 0.6) as usize % chords.len()];
                let since = t % 0.6;
                let notes: f32 = chord
                    .iter()
                    .map(|&f| note(t, f, 30, rate) * (-since * 3.0).exp())
                    .sum();
                let hat = hats[i] * (-(t % 0.125) * 60.0).exp();
                0.06 * notes + hat + pad[i]
            })
            .collect();
        for level in [HissLevel::Light, HissLevel::Strong] {
            let out = run_at(rate, 1, input.clone(), level);
            // 1/3-octave bands from 2 to 10 kHz
            let mut worst = 0.0f32;
            let mut sum = 0.0f32;
            let mut n = 0;
            let mut lo = 2_000.0f64;
            while lo < 10_000.0 {
                let hi = lo * 1.26;
                let band = |x: &[f32]| {
                    let hp = high_passed(x, rate, lo);
                    let mut lp_a = Biquad64::high_pass(hi, f64::from(rate));
                    let mut lp_b = Biquad64::high_pass(hi, f64::from(rate));
                    // Low-pass by taking the high-passed part above `hi` away
                    let above: Vec<f32> = hp
                        .iter()
                        .map(|&v| lp_b.run(lp_a.run(f64::from(v))) as f32)
                        .collect();
                    let inside: Vec<f32> = hp.iter().zip(&above).map(|(a, b)| a - b).collect();
                    rms(&inside[inside.len() - 6 * rate as usize..])
                };
                let change = db(band(&out) / band(&input));
                worst = worst.max(change.abs());
                sum += change.abs();
                n += 1;
                lo = hi;
            }
            let mean = sum / n as f32;
            assert!(mean < 1.0, "{level:?} mean {mean} dB");
            assert!(worst < 2.0, "{level:?} worst {worst} dB");
        }
    }
}
