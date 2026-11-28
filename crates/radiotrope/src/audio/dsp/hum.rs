//! The Hum Filter: finds hum (mains at 50 or 60 Hz and its harmonics, from
//! bad cables or grounding at the station, or on an old recording) and
//! takes it out with very narrow notches. It only touches the sound while
//! it hears hum.
//!
//! Hum is a thin, steady line with a ladder of harmonics, but it is not
//! always at exactly 50 or 60 Hz. Mains itself drifts by a tenth of a hertz
//! or two, and a record or tape transferred a little off speed moves its
//! hum by the same ratio: 78 rpm transfers carry it at 49.25 or 50.22 Hz.
//! So the filter measures where the hum really is, within
//! [`HUM_SEARCH_HZ`] of 50 or 60 Hz, and follows it.
//!
//! **Listening.** A mono mix is cut down to about 2.8 kHz (enough for the
//! harmonics it removes, at a fifteenth of the cost) and every
//! [`HUM_CHECK_MS`] the last [`HUM_WINDOW_SECS`] are judged: one FFT of the
//! whole window gives the sound beside every line, and a few Goertzel
//! filters measure the lines themselves exactly.
//!
//! **Finding it.** The strongest ladder of the first three harmonics near
//! 50 or 60 Hz is the candidate; its pitch is then refined on its cleanest
//! harmonic (the k-th harmonic gives k times the precision). It may be hum
//! once the fundamental or the 2nd harmonic stands [`HUM_FOUND_DB`] above
//! the sound [`HUM_BESIDE_HZ`] beside it. Hum under speech is often only
//! the fundamental, since the voice covers the harmonics, so one line is
//! enough. What stops a bass note is time: near mains the pitch must hold
//! still for [`HUM_NEAR_CHECKS`] checks, and further off (where a note is
//! likelier than mains: G1 is 49.0 Hz) it must hold still in pitch and
//! level for [`HUM_FAR_CHECKS`] checks, about 8 s, which no played note
//! does. That is why off-speed hum takes about 12 s to go.
//!
//! **Following it.** Once found it is followed within [`HUM_TRACK_HZ`] each
//! check, each harmonic weighed against the sound beside it, so a loud
//! voice partial next to a line can't pull the notches away from the hum.
//!
//! **Removing it.** Each harmonic up to [`HUM_HARMONICS`] (and
//! [`HUM_MAX_HZ`]) gets its own notch while it stands [`HUM_LINE_DB`] above
//! its neighbours: a rich buzz is taken out whole, and harmonics that are
//! not there are left alone. Notches move with the hum without restarting
//! and fade in and out over [`HUM_FADE_MS`].
//!
//! **A known limit.** Hum that comes and goes (heard only between bass
//! notes, or only on some records of a programme) may never be steady for
//! long enough to be believed, and then stays.

use std::num::NonZero;
use std::sync::Arc;
use std::time::Duration;

use rodio::Source;
use rustfft::{num_complex::Complex, Fft, FftPlanner};

use super::filter::Biquad64;
use super::sound::{FilterSwitches, HumReadout};
use crate::config::sound::*;

/// Channels filtered; more pass through as they are
const MAX_CHANNELS: usize = 8;

/// Mains frequencies looked for
const MAINS_HZ: [f64; 2] = [50.0, 60.0];

/// The detector's rate, about: above twice [`HUM_MAX_HZ`] and the sound
/// beside it
const DETECT_RATE: f64 = 2800.0;

/// Where the mix is cut before it is cut down (Hz)
const DETECT_CUTOFF_HZ: f64 = 1300.0;

/// Steps of the first, rough search for the hum (Hz)
const SEARCH_STEP_HZ: f64 = 0.05;

/// Refining the pitch: points either side, and how far apart (Hz)
const REFINE_POINTS: usize = 4;
const REFINE_STEP_HZ: f64 = 0.025;

/// Following the hum: steps of its climb (Hz)
const TRACK_STEP_HZ: f64 = 0.02;

/// Harmonics that find and follow the hum
const SEARCH_HARMONICS: usize = 3;
const REFINE_HARMONICS: usize = 6;
const TRACK_HARMONICS: usize = 4;

/// Cuts the mono mix down to the detector's rate
struct Decimator {
    /// A windowed-sinc low-pass, summing to 1
    taps: Vec<f64>,
    /// The last samples, twice over so the newest `taps.len()` are always
    /// one slice
    history: Vec<f64>,
    pos: usize,
    factor: usize,
    count: usize,
}

impl Decimator {
    fn new(sample_rate: f64) -> Self {
        let factor = ((sample_rate / DETECT_RATE).floor() as usize).max(1);
        // Short and soft: the lines judged sit far below the cutoff, and
        // a little aliasing only adds to the sound beside them
        let len = 8 * factor + 1;
        let fc = DETECT_CUTOFF_HZ / sample_rate;
        let mid = (len / 2) as f64;
        let mut taps: Vec<f64> = (0..len)
            .map(|i| {
                let n = i as f64 - mid;
                let sinc = if n == 0.0 {
                    2.0 * fc
                } else {
                    (2.0 * std::f64::consts::PI * fc * n).sin() / (std::f64::consts::PI * n)
                };
                let hamming = 0.54
                    - 0.46
                        * (2.0 * std::f64::consts::PI * i as f64 / (len - 1).max(1) as f64).cos();
                sinc * hamming
            })
            .collect();
        let sum: f64 = taps.iter().sum();
        if sum.abs() > 1e-9 {
            taps.iter_mut().for_each(|t| *t /= sum);
        }
        Self {
            history: vec![0.0; 2 * len],
            taps,
            pos: 0,
            factor,
            count: 0,
        }
    }

    /// The rate it gives
    fn rate(&self, sample_rate: f64) -> f64 {
        sample_rate / self.factor as f64
    }

    fn push(&mut self, x: f64) -> Option<f64> {
        let len = self.taps.len();
        self.history[self.pos] = x;
        self.history[self.pos + len] = x;
        self.pos = (self.pos + 1) % len;
        let due = self.count == 0;
        self.count = (self.count + 1) % self.factor;
        if !due {
            return None;
        }
        // Oldest first from pos; the taps are symmetric
        let recent = &self.history[self.pos..self.pos + len];
        Some(recent.iter().zip(&self.taps).map(|(x, t)| x * t).sum())
    }
}

/// Exact power of `x` at each of `freqs` (Hz at `rate`), on the same scale
/// as an FFT bin's |X|²: Goertzel filters, four at a time so the CPU runs
/// them side by side
fn powers<const N: usize>(x: &[f64], freqs: &[f64; N], rate: f64) -> [f64; N] {
    let mut out = [0.0; N];
    for (fs, os) in freqs.chunks(4).zip(out.chunks_mut(4)) {
        let mut c = [0.0f64; 4];
        for (c, f) in c.iter_mut().zip(fs) {
            *c = 2.0 * (2.0 * std::f64::consts::PI * f / rate).cos();
        }
        let (mut s1, mut s2) = ([0.0f64; 4], [0.0f64; 4]);
        for &v in x {
            for l in 0..4 {
                let s = v + c[l] * s1[l] - s2[l];
                s2[l] = s1[l];
                s1[l] = s;
            }
        }
        for (l, o) in os.iter_mut().enumerate() {
            *o = s1[l] * s1[l] + s2[l] * s2[l] - c[l] * s1[l] * s2[l];
        }
    }
    out
}

/// Where the top of a peak sampled at three evenly spaced points lies,
/// in steps from the middle one (a parabola through their logs: a
/// windowed line's peak is close to one)
fn vertex(below: f64, at: f64, above: f64) -> f64 {
    let (a, b, c) = (
        below.max(1e-300).ln(),
        at.max(1e-300).ln(),
        above.max(1e-300).ln(),
    );
    let d = a - 2.0 * b + c;
    if d < 0.0 {
        (0.5 * (a - c) / d).clamp(-0.5, 0.5)
    } else {
        0.0
    }
}

/// Hum not yet believed
#[derive(Clone, Copy, Debug)]
struct Candidate {
    /// Where it was heard, the last [`HUM_FAR_CHECKS`] checks
    heard: [f64; HUM_FAR_CHECKS as usize],
    checks: u32,
    /// The quietest and loudest it has been (dB)
    low_db: f64,
    high_db: f64,
}

impl Candidate {
    /// The median of where it was heard
    fn middle(&self) -> f64 {
        let n = (self.checks as usize).clamp(1, self.heard.len());
        let mut heard = self.heard;
        let heard = &mut heard[..n];
        heard.sort_by(f64::total_cmp);
        if n % 2 == 1 {
            heard[n / 2]
        } else {
            0.5 * (heard[n / 2 - 1] + heard[n / 2])
        }
    }
}

/// What a check decided
#[derive(Clone, Copy, Debug, PartialEq)]
struct Decision {
    /// The hum being removed
    hz: Option<f64>,
    /// Its harmonics to notch: bit k-1 for the k-th
    lines: u32,
}

/// Listens for hum, one check at a time
struct Detector {
    sample_rate: f64,
    decimator: Decimator,
    /// The detector's rate
    rate: f64,
    /// The last window of the cut-down mix, a ring
    ring: Vec<f64>,
    ring_pos: usize,
    filled: usize,
    /// Cut-down samples between checks, and since the last
    step: usize,
    since: usize,
    hann: Vec<f64>,
    /// Sum of the window, to turn power into amplitude
    window_sum: f64,
    /// The window as judged: oldest first, windowed
    seg: Vec<f64>,
    fft: Arc<dyn Fft<f64>>,
    spectrum: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
    /// Power per FFT bin, up to half the rate
    power: Vec<f64>,
    bin_hz: f64,
    /// The highest harmonic notched (Hz)
    top_hz: f64,
    /// The hum being removed
    hum: Option<f64>,
    /// 50 or 60: the mains it was found near
    mains: f64,
    candidate: Option<Candidate>,
    /// Checks in a row without the hum being removed
    missed: u32,
    /// Per harmonic (index k-1): checks left to notch it, none at 0
    lines: [i32; HUM_HARMONICS],
}

impl Detector {
    fn new(sample_rate: u32) -> Self {
        let sample_rate = f64::from(sample_rate.max(1));
        let decimator = Decimator::new(sample_rate);
        let rate = decimator.rate(sample_rate);
        let len = ((HUM_WINDOW_SECS * rate) as usize).max(64);
        let hann: Vec<f64> = (0..len)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / len as f64).cos())
            .collect();
        // Padded to well over twice the window, so a bin is never more
        // than about 0.05 Hz from a line
        let fft_len = (2 * len).next_power_of_two();
        let fft = FftPlanner::new().plan_fft_forward(fft_len);
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        Self {
            sample_rate,
            decimator,
            rate,
            ring: vec![0.0; len],
            ring_pos: 0,
            filled: 0,
            step: ((f64::from(HUM_CHECK_MS) / 1000.0 * rate) as usize).max(1),
            since: 0,
            window_sum: hann.iter().sum(),
            hann,
            seg: vec![0.0; len],
            fft,
            spectrum: vec![Complex::default(); fft_len],
            scratch,
            power: vec![0.0; fft_len / 2 + 1],
            bin_hz: rate / fft_len as f64,
            top_hz: HUM_MAX_HZ.min(0.45 * sample_rate),
            hum: None,
            mains: 50.0,
            candidate: None,
            missed: 0,
            lines: [0; HUM_HARMONICS],
        }
    }

    /// Forget what was heard (switched off), keeping the buffers
    fn restart(&mut self) {
        self.decimator = Decimator::new(self.sample_rate);
        self.ring.iter_mut().for_each(|x| *x = 0.0);
        self.ring_pos = 0;
        self.filled = 0;
        self.since = 0;
        self.hum = None;
        self.candidate = None;
        self.missed = 0;
        self.lines = [0; HUM_HARMONICS];
    }

    /// Take one mono sample; at the end of a check, say what was decided
    fn push(&mut self, x: f32) -> Option<Decision> {
        let x = self.decimator.push(f64::from(x))?;
        self.ring[self.ring_pos] = x;
        self.ring_pos = (self.ring_pos + 1) % self.ring.len();
        self.filled = (self.filled + 1).min(self.ring.len());
        self.since += 1;
        if self.since < self.step || self.filled < self.ring.len() {
            return None;
        }
        self.since = 0;
        Some(self.decide())
    }

    /// Exact power at `freqs`
    fn exact<const N: usize>(&self, freqs: &[f64; N]) -> [f64; N] {
        powers(&self.seg, freqs, self.rate)
    }

    /// Power at `hz` from the FFT: the nearest bin
    fn bin(&self, hz: f64) -> f64 {
        let i = (hz / self.bin_hz).round().max(0.0) as usize;
        self.power[i.min(self.power.len() - 1)]
    }

    /// The sound beside a line at `hz`
    fn beside(&self, hz: f64) -> f64 {
        let [near, far] = HUM_BESIDE_HZ;
        (self.bin(hz - far) + self.bin(hz - near) + self.bin(hz + near) + self.bin(hz + far)) / 4.0
            + 1e-30
    }

    /// How far each of the first `N` harmonics of `hz` stands above the
    /// sound beside it (dB); lines quieter than [`HUM_MIN_LEVEL`] don't
    fn above<const N: usize>(&self, hz: f64) -> [f64; N] {
        let freqs: [f64; N] = std::array::from_fn(|k| (k + 1) as f64 * hz);
        let lines = self.exact(&freqs);
        // Power of a sine of amplitude A is (A * window_sum / 2)^2
        let min_power = (HUM_MIN_LEVEL * self.window_sum / 2.0).powi(2);
        std::array::from_fn(|k| {
            if freqs[k] > self.top_hz || lines[k] < min_power {
                f64::NEG_INFINITY
            } else {
                10.0 * (lines[k] / self.beside(freqs[k])).log10()
            }
        })
    }

    /// The fundamental's height above its neighbours, or the 2nd's
    fn fundamental_db(&self, hz: f64) -> f64 {
        let [a, b] = self.above::<2>(hz);
        a.max(b)
    }

    fn decide(&mut self) -> Decision {
        let len = self.ring.len();
        for i in 0..len {
            self.seg[i] = self.ring[(self.ring_pos + i) % len] * self.hann[i];
        }
        for (s, &x) in self.spectrum.iter_mut().zip(&self.seg) {
            *s = Complex::new(x, 0.0);
        }
        self.spectrum[len..]
            .iter_mut()
            .for_each(|s| *s = Complex::default());
        self.fft
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        for (p, s) in self.power.iter_mut().zip(&self.spectrum) {
            *p = s.norm_sqr();
        }

        match self.hum {
            None => self.find(),
            Some(hz) => self.follow(hz),
        }

        match self.hum {
            Some(hz) => {
                let above = self.above::<HUM_HARMONICS>(hz);
                for (k, line) in self.lines.iter_mut().enumerate() {
                    if (k + 1) as f64 * hz > self.top_hz {
                        break;
                    }
                    // The first two stay while the hum does: they carry
                    // most of it, and music passing over them would
                    // otherwise let it through in bursts
                    let on = above[k] > HUM_LINE_DB || (k < 2 && *line > 0);
                    *line = if on {
                        HUM_LINE_HOLD_CHECKS
                    } else {
                        (*line - 1).max(0)
                    };
                }
            }
            None => self.lines = [0; HUM_HARMONICS],
        }
        Decision {
            hz: self.hum,
            lines: self
                .lines
                .iter()
                .enumerate()
                .filter(|(_, &line)| line > 0)
                .fold(0, |bits, (k, _)| bits | 1 << k),
        }
    }

    /// No hum yet: look for some, and believe it once it has held still
    fn find(&mut self) {
        // The strongest ladder of the first harmonics, roughly
        let mut best = (f64::MIN, MAINS_HZ[0]);
        let reach = HUM_SEARCH_HZ + SEARCH_STEP_HZ;
        let steps = (2.0 * reach / SEARCH_STEP_HZ).round() as usize;
        for &mains in &MAINS_HZ {
            for i in 0..=steps {
                let hz = mains - reach + i as f64 * SEARCH_STEP_HZ;
                let score: f64 = (1..=SEARCH_HARMONICS)
                    .map(|k| self.bin(k as f64 * hz))
                    .sum();
                if score > best.0 {
                    best = (score, hz);
                }
            }
        }
        let mut hz = best.1;
        let mains = if hz < 55.0 { 50.0 } else { 60.0 };
        // A peak at the very edge is a note beside the range, not hum
        if (hz - mains).abs() > HUM_SEARCH_HZ {
            self.candidate = None;
            return;
        }
        // Refine on the cleanest harmonic: k times the precision
        let above = self.above::<REFINE_HARMONICS>(hz);
        let k = (0..REFINE_HARMONICS)
            .max_by(|&a, &b| above[a].total_cmp(&above[b]))
            .unwrap_or(0) as f64
            + 1.0;
        let grid: [f64; 2 * REFINE_POINTS + 1] =
            std::array::from_fn(|i| k * (hz + (i as f64 - REFINE_POINTS as f64) * REFINE_STEP_HZ));
        let p = self.exact(&grid);
        let i = (0..grid.len())
            .max_by(|&a, &b| p[a].total_cmp(&p[b]))
            .unwrap_or(REFINE_POINTS);
        let shift = if i == 0 || i == grid.len() - 1 {
            0.0
        } else {
            vertex(p[i - 1], p[i], p[i + 1])
        };
        hz += (i as f64 - REFINE_POINTS as f64 + shift) * REFINE_STEP_HZ;

        if self.fundamental_db(hz) <= HUM_FOUND_DB {
            self.candidate = None;
            return;
        }
        let [p1, p2] = self.exact(&[hz, 2.0 * hz]);
        let level_db = 10.0 * p1.max(p2).max(1e-30).log10();
        let near = (hz - mains).abs() <= HUM_NEAR_HZ;
        let (checks, steady) = if near {
            (HUM_NEAR_CHECKS, HUM_NEAR_STEADY_HZ)
        } else {
            (HUM_FAR_CHECKS, HUM_FAR_STEADY_HZ)
        };
        // Steady means close to where it has mostly been: measured
        // against the last check only, a line could creep onto a note
        let mut c = match self.candidate {
            Some(c) if (c.middle() - hz).abs() < steady => Candidate {
                low_db: c.low_db.min(level_db),
                high_db: c.high_db.max(level_db),
                ..c
            },
            _ => Candidate {
                heard: [hz; HUM_FAR_CHECKS as usize],
                checks: 0,
                low_db: level_db,
                high_db: level_db,
            },
        };
        c.heard[c.checks as usize % c.heard.len()] = hz;
        c.checks += 1;
        self.candidate = Some(c);
        if c.checks >= checks && (near || c.high_db - c.low_db < HUM_FAR_LEVEL_DB) {
            self.hum = Some(c.middle());
            self.mains = mains;
            self.missed = 0;
        }
    }

    /// Hum being removed: follow it, or let it go once it has gone
    fn follow(&mut self, hz: f64) {
        // Each harmonic weighed against the sound beside it, so a loud
        // partial beside a line can't pull the notches off the hum
        let weights: [f64; TRACK_HARMONICS] =
            std::array::from_fn(|k| 1.0 / self.beside((k + 1) as f64 * hz));
        let score = |f: f64| -> f64 {
            let freqs: [f64; TRACK_HARMONICS] = std::array::from_fn(|k| (k + 1) as f64 * f);
            let p = self.exact(&freqs);
            p.iter().zip(&weights).map(|(p, w)| p * w).sum()
        };
        // Climb from where the hum was to the top of its own line, not to
        // the loudest thing nearby: a bass note a hertz away is far louder
        // than the hum, and would take the notches with it
        let mut to = hz;
        let (mut below, mut at, mut above) = (
            score(to - TRACK_STEP_HZ),
            score(to),
            score(to + TRACK_STEP_HZ),
        );
        let steps = (HUM_TRACK_HZ / TRACK_STEP_HZ).round() as usize;
        for _ in 0..steps {
            if below > at && below >= above {
                to -= TRACK_STEP_HZ;
                (above, at) = (at, below);
                below = score(to - TRACK_STEP_HZ);
            } else if above > at {
                to += TRACK_STEP_HZ;
                (below, at) = (at, above);
                above = score(to + TRACK_STEP_HZ);
            } else {
                break;
            }
        }
        to += vertex(below, at, above) * TRACK_STEP_HZ;
        // Halfway there, and never fast: one odd check can't throw it off
        let next = hz + (0.5 * (to - hz)).clamp(-HUM_TRACK_STEP_HZ, HUM_TRACK_STEP_HZ);

        if (next - self.mains).abs() <= HUM_SEARCH_HZ && self.fundamental_db(next) > HUM_HOLD_DB {
            self.hum = Some(next);
            self.missed = 0;
        } else {
            self.missed += 1;
            if self.missed >= HUM_GONE_CHECKS {
                self.hum = None;
                self.candidate = None;
                self.missed = 0;
            }
        }
    }
}

/// A `Source` adapter that removes hum while it hears some
pub struct HumSource<S> {
    inner: S,
    channels: u16,
    sample_rate: u32,
    channel_index: usize,
    switches: Arc<FilterSwitches>,
    readout: Arc<HumReadout>,
    owner: u64,
    detector: Detector,
    /// Mono mix of the frame so far
    frame_sum: f32,
    /// Per channel, a notch per harmonic (index k-1)
    notches: Vec<[Biquad64; HUM_HARMONICS]>,
    /// Harmonics to notch now: bit k-1 for the k-th
    wanted: u32,
    /// Harmonics heard at all now (notched or fading)
    sounding: u32,
    /// How much of each notch plays, 0..1
    mix: [f64; HUM_HARMONICS],
    mix_step: f64,
    on: bool,
}

impl<S> HumSource<S>
where
    S: Source<Item = f32>,
{
    pub fn new(inner: S, switches: Arc<FilterSwitches>, readout: Arc<HumReadout>) -> Self {
        let channels = inner.channels().get();
        let sample_rate = inner.sample_rate().get();
        let owner = readout.claim();
        readout.set_if(owner, None);
        Self {
            inner,
            channels,
            sample_rate,
            channel_index: 0,
            on: switches.hum_filter(),
            switches,
            readout,
            owner,
            detector: Detector::new(sample_rate),
            frame_sum: 0.0,
            notches: new_notches(channels),
            wanted: 0,
            sounding: 0,
            mix: [0.0; HUM_HARMONICS],
            mix_step: mix_step(sample_rate),
        }
    }

    /// Start over in a new format: no hum known
    fn reset(&mut self, channels: u16, sample_rate: u32) {
        self.channels = channels;
        self.sample_rate = sample_rate;
        self.detector = Detector::new(sample_rate);
        self.frame_sum = 0.0;
        self.notches = new_notches(channels);
        self.wanted = 0;
        self.sounding = 0;
        self.mix = [0.0; HUM_HARMONICS];
        self.mix_step = mix_step(sample_rate);
        self.readout.set_if(self.owner, None);
    }

    /// A check decided: move the notches onto the hum, or let them go
    fn decided(&mut self, d: Decision) {
        if let Some(hz) = d.hz {
            let rate = f64::from(self.sample_rate.max(1));
            for k in 0..HUM_HARMONICS {
                if d.lines & 1 << k == 0 {
                    continue;
                }
                let width = if k < 2 {
                    HUM_LOW_NOTCH_HZ
                } else {
                    HUM_NOTCH_HZ
                };
                let notch = Biquad64::notch((k + 1) as f64 * hz, width, rate);
                for bank in &mut self.notches {
                    if self.mix[k] == 0.0 {
                        // Silent until now: start clean
                        bank[k] = notch;
                    } else {
                        bank[k].retune(&notch);
                    }
                }
            }
        }
        // Lines no longer wanted fade out where they are
        self.wanted = d.lines;
        self.readout.set_if(self.owner, d.hz.map(|hz| hz as f32));
    }

    /// One frame on: each notch's mix a step towards on or off
    fn fade(&mut self) {
        let mut live = self.wanted | self.sounding;
        while live != 0 {
            let k = live.trailing_zeros() as usize;
            live &= live - 1;
            let m = &mut self.mix[k];
            if self.wanted & 1 << k != 0 {
                *m = (*m + self.mix_step).min(1.0);
                self.sounding |= 1 << k;
            } else {
                *m = (*m - self.mix_step).max(0.0);
                if *m == 0.0 {
                    self.sounding &= !(1 << k);
                }
            }
        }
    }
}

fn new_notches(channels: u16) -> Vec<[Biquad64; HUM_HARMONICS]> {
    let idle = Biquad64::notch(50.0, HUM_NOTCH_HZ, 48_000.0);
    vec![[idle; HUM_HARMONICS]; usize::from(channels).min(MAX_CHANNELS)]
}

fn mix_step(sample_rate: u32) -> f64 {
    1.0 / (f64::from(HUM_FADE_MS) / 1000.0 * f64::from(sample_rate.max(1)))
}

impl<S> Iterator for HumSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let x = self.inner.next()?;
        if self.channel_index == 0 {
            // The format can change between frames (see EqSource)
            let channels = self.inner.channels().get();
            let sample_rate = self.inner.sample_rate().get();
            if channels != self.channels || sample_rate != self.sample_rate {
                self.reset(channels, sample_rate);
            }
            let on = self.switches.hum_filter();
            if on != self.on {
                self.on = on;
                if !on {
                    // Forget what was heard; what is notched fades out
                    self.detector.restart();
                    self.frame_sum = 0.0;
                    self.wanted = 0;
                    self.readout.set_if(self.owner, None);
                }
            }
            if self.wanted | self.sounding != 0 {
                self.fade();
            }
        }
        let ch = self.channel_index;
        let channels = usize::from(self.channels.max(1));
        self.channel_index = (self.channel_index + 1) % channels;
        let x = if x.is_finite() { x } else { 0.0 };

        if self.on {
            self.frame_sum += x;
            if self.channel_index == 0 {
                let mono = self.frame_sum / channels as f32;
                self.frame_sum = 0.0;
                if let Some(d) = self.detector.push(mono) {
                    self.decided(d);
                }
            }
        }

        if self.sounding == 0 {
            return Some(x);
        }
        let Some(bank) = self.notches.get_mut(ch) else {
            return Some(x);
        };
        let mut y = f64::from(x);
        let mut live = self.sounding;
        while live != 0 {
            let k = live.trailing_zeros() as usize;
            live &= live - 1;
            let z = bank[k].run(y);
            y += self.mix[k] * (z - y);
        }
        Some(y as f32)
    }
}

impl<S> Source for HumSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> NonZero<u16> {
        self.inner.channels()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::SoundParams;
    use rodio::buffer::SamplesBuffer;
    use std::f64::consts::TAU;

    const RATE: u32 = 48_000;

    /// A little xorshift noise, the same every run
    struct Noise(u32);

    impl Noise {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            f64::from(self.0) / f64::from(u32::MAX) * 2.0 - 1.0
        }
    }

    /// Mono at `rate`: `f(t)` for each sample, plus a little noise
    fn render(rate: u32, secs: f64, noise: f64, f: impl Fn(f64) -> f64) -> Vec<f32> {
        let mut n = Noise(0x1234_5678);
        (0..(secs * f64::from(rate)) as usize)
            .map(|i| (f(i as f64 / f64::from(rate)) + noise * n.next()) as f32)
            .collect()
    }

    /// The sum of (Hz, amplitude) sines, plus a little noise
    fn signal(secs: f64, tones: &[(f64, f64)], noise: f64) -> Vec<f32> {
        let tones = tones.to_vec();
        render(RATE, secs, noise, move |t| {
            tones.iter().map(|&(f, a)| a * (TAU * f * t).sin()).sum()
        })
    }

    /// A hum at `hz` with `harmonics` (amplitudes of the 1st, 2nd...)
    fn hum(hz: f64, harmonics: &[f64]) -> Vec<(f64, f64)> {
        harmonics
            .iter()
            .enumerate()
            .map(|(k, &a)| ((k + 1) as f64 * hz, a))
            .collect()
    }

    fn add(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x + y).collect()
    }

    fn run_at(
        rate: u32,
        channels: u16,
        samples: Vec<f32>,
        params: &SoundParams,
    ) -> (Vec<f32>, Arc<HumReadout>) {
        let readout = HumReadout::new_shared();
        let out = HumSource::new(
            SamplesBuffer::new(
                NonZero::new(channels).unwrap(),
                NonZero::new(rate).unwrap(),
                samples,
            ),
            params.switches(),
            readout.clone(),
        )
        .collect();
        (out, readout)
    }

    fn run(samples: Vec<f32>, params: &SoundParams) -> (Vec<f32>, Arc<HumReadout>) {
        run_at(RATE, 1, samples, params)
    }

    /// Amplitude of `freq` in the last `secs` of mono `x` at `rate`
    fn level_at(rate: u32, x: &[f32], freq: f64, secs: f64) -> f64 {
        let n = (secs * f64::from(rate)) as usize;
        let tail = &x[x.len() - n..];
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &v) in tail.iter().enumerate() {
            // A Hann window, so the music beside a line doesn't leak in
            let w = 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos();
            let ph = TAU * freq * i as f64 / f64::from(rate);
            re += w * f64::from(v) * ph.cos();
            im += w * f64::from(v) * ph.sin();
        }
        4.0 * (re * re + im * im).sqrt() / n as f64
    }

    fn level(x: &[f32], freq: f64, secs: f64) -> f64 {
        level_at(RATE, x, freq, secs)
    }

    fn db(a: f64, b: f64) -> f64 {
        20.0 * (a / b).log10()
    }

    fn found(readout: &HumReadout) -> Option<f64> {
        readout.hz().map(f64::from)
    }

    const MUSIC: (f64, f64) = (440.0, 0.3);

    #[test]
    fn mains_hum_is_found_and_removed() {
        let mut tones = hum(50.0, &[0.03, 0.02, 0.01]);
        tones.push(MUSIC);
        let (out, readout) = run(signal(12.0, &tones, 0.01), &SoundParams::default());
        let hz = found(&readout).expect("hum found");
        assert!((hz - 50.0).abs() < 0.02, "found at {hz}");
        for (f, a) in &tones[..3] {
            let left = level(&out, *f, 2.0);
            assert!(db(left, *a) < -20.0, "{f} Hz only down to {left}");
        }
        let music = level(&out, 440.0, 2.0);
        assert!(db(music, 0.3).abs() < 0.1, "music changed: {music}");
    }

    #[test]
    fn sixty_hertz_hum_too() {
        let mut tones = hum(60.0, &[0.02, 0.02, 0.01]);
        tones.push(MUSIC);
        let (out, readout) = run(signal(12.0, &tones, 0.01), &SoundParams::default());
        let hz = found(&readout).expect("hum found");
        assert!((hz - 60.0).abs() < 0.02, "found at {hz}");
        assert!(db(level(&out, 120.0, 2.0), 0.02) < -20.0);
    }

    /// Hum where an old record transferred off speed carries it, or
    /// mains running a little fast, under music and noise
    #[test]
    fn hum_off_mains_is_found_where_it_is() {
        for (hz, secs) in [(49.25, 20.0), (50.0, 12.0), (60.4, 12.0), (50.9, 20.0)] {
            let mut tones = hum(hz, &[0.02, 0.01, 0.006, 0.004]);
            tones.push(MUSIC);
            tones.push((523.25, 0.2));
            let (out, readout) = run(signal(secs, &tones, 0.02), &SoundParams::default());
            let got = found(&readout).unwrap_or_else(|| panic!("{hz} Hz not found"));
            assert!((got - hz).abs() < 0.02, "{hz} Hz found at {got}");
            for &(f, a) in &tones[..2] {
                let drop = db(level(&out, f, 2.0), a);
                assert!(drop < -20.0, "{hz} Hz: {f} Hz only down {drop:.1} dB");
            }
            let music = db(level(&out, 440.0, 2.0), 0.3);
            assert!(music.abs() < 0.1, "{hz} Hz: music moved {music:.2} dB");
        }
    }

    #[test]
    fn off_mains_hum_needs_longer_proof() {
        // 49.25 Hz: not near mains, so 8 s of steadiness, after the first
        // 4 s window: not yet at 10 s, at 13 s it is
        let tones = hum(49.25, &[0.03, 0.01]);
        let (_, readout) = run(signal(10.0, &tones, 0.01), &SoundParams::default());
        assert_eq!(readout.hz(), None);
        let (_, readout) = run(signal(13.0, &tones, 0.01), &SoundParams::default());
        assert!(readout.hz().is_some());
    }

    #[test]
    fn a_bass_note_is_not_hum() {
        // A long A1 with its harmonics
        let tones = [(55.0, 0.2), (110.0, 0.1), (165.0, 0.05), MUSIC];
        let input = signal(12.0, &tones, 0.01);
        let (out, readout) = run(input.clone(), &SoundParams::default());
        assert_eq!(readout.hz(), None);
        assert_eq!(out, input, "untouched");
    }

    /// A bass line that keeps coming back to G1 (49.0 Hz, inside the
    /// search), changing note every 2 s, with a kick drum of `kick` on
    /// every beat
    fn bass_line(rate: u32, secs: f64, kick: f64) -> Vec<f32> {
        // G1, B1, D2, G1, E1, A1...: each note decays like a plucked string
        let notes = [49.0, 61.74, 73.42, 49.0, 41.2, 55.0, 49.0, 65.41];
        render(rate, secs, 0.005, move |t| {
            let i = (t / 2.0) as usize;
            let f = notes[i % notes.len()];
            let age = t - 2.0 * i as f64;
            let env = (-age * 1.2).exp() * (age * 200.0).min(1.0);
            let bass: f64 = (1..=6)
                .map(|k| 0.3 / k as f64 * (TAU * f * k as f64 * t).sin())
                .sum();
            // A kick on every beat, and a held chord above
            let beat = t % 0.5;
            let drum =
                (-beat * 30.0).exp() * (TAU * (50.0 + 80.0 * (-beat * 40.0).exp()) * beat).sin();
            env * bass
                + kick * drum
                + 0.1 * (TAU * 220.0 * t).sin()
                + 0.08 * (TAU * 329.6 * t).sin()
        })
    }

    #[test]
    fn bass_heavy_music_is_left_alone() {
        for rate in [22_050, 44_100, 48_000] {
            let input = bass_line(rate, 30.0, 0.4);
            let (out, readout) = run_at(rate, 1, input.clone(), &SoundParams::default());
            assert_eq!(readout.hz(), None, "at {rate}");
            assert!(out == input, "touched at {rate}");
        }
    }

    #[test]
    fn a_held_bass_note_off_mains_is_not_hum() {
        // A note held for 6 s at 49.0 Hz, as an organ pedal: it holds
        // still, but not for the 8 s off-mains hum must
        let mut input = render(RATE, 6.0, 0.005, |t| {
            (1..=4)
                .map(|k| 0.2 / k as f64 * (TAU * 49.0 * k as f64 * t).sin())
                .sum()
        });
        input.extend(signal(10.0, &[MUSIC], 0.01));
        let (out, readout) = run(input.clone(), &SoundParams::default());
        assert_eq!(readout.hz(), None);
        assert!(out == input, "touched");
    }

    /// Something like a voice: a buzz whose pitch glides between 100 and
    /// 180 Hz, its harmonics shaped by two formants, in syllables
    fn speech(rate: u32, secs: f64) -> Vec<f32> {
        let dt = 1.0 / f64::from(rate);
        let mut phase = 0.0f64;
        let n = (secs * f64::from(rate)) as usize;
        let mut noise = Noise(0x9e37_79b9);
        (0..n)
            .map(|i| {
                let t = i as f64 * dt;
                let pitch = 140.0 + 40.0 * (TAU * 0.7 * t).sin() + 15.0 * (TAU * 3.1 * t).sin();
                phase += pitch * dt;
                let syllable = (TAU * 4.0 * t).sin().max(0.0).powf(0.5)
                    * if (t % 3.0) < 2.5 { 1.0 } else { 0.0 };
                let voice: f64 = (1..=25)
                    .map(|k| {
                        let f = pitch * k as f64;
                        let formant = 1.0 / (1.0 + ((f - 600.0) / 250.0).powi(2))
                            + 0.6 / (1.0 + ((f - 1800.0) / 400.0).powi(2));
                        0.25 * formant * (TAU * k as f64 * phase).sin()
                    })
                    .sum();
                (syllable * voice + 0.003 * noise.next()) as f32
            })
            .collect()
    }

    #[test]
    fn hum_under_speech_is_found_by_its_fundamental() {
        // Only the fundamental stands out: the voice covers the rest,
        // which today's three-line rule never heard
        let voice = speech(RATE, 14.0);
        let hum_only = signal(14.0, &hum(50.0, &[0.02, 0.003]), 0.0);
        let (out, readout) = run(add(&voice, &hum_only), &SoundParams::default());
        let hz = found(&readout).expect("hum under speech found");
        assert!((hz - 50.0).abs() < 0.05, "found at {hz}");
        let before = level(&add(&voice, &hum_only), 50.0, 3.0);
        let after = level(&out, 50.0, 3.0);
        assert!(
            db(after, before) < -20.0,
            "only {:.1} dB",
            db(after, before)
        );
    }

    #[test]
    fn speech_alone_is_not_hum() {
        let input = speech(RATE, 20.0);
        let (out, readout) = run(input.clone(), &SoundParams::default());
        assert_eq!(readout.hz(), None);
        assert!(out == input, "touched");
    }

    #[test]
    fn drifting_hum_is_followed() {
        // Mains wandering by a tenth of a hertz each way, slowly, as it
        // does (0.005 Hz a second), with a rich buzz
        let secs = 40.0;
        let drift = |t: f64| 50.0 - 0.1 + 0.2 * t / secs;
        let amps = [0.02, 0.01, 0.008, 0.006, 0.005, 0.004, 0.003, 0.002];
        let input = render(RATE, secs, 0.01, move |t| {
            // The phase of a linear chirp
            let phase = (50.0 - 0.1) * t + 0.1 * t * t / secs;
            let hum: f64 = amps
                .iter()
                .enumerate()
                .map(|(k, a)| a * (TAU * (k + 1) as f64 * phase).sin())
                .sum();
            hum + 0.3 * (TAU * 440.0 * t).sin()
        });
        let (out, readout) = run(input.clone(), &SoundParams::default());
        let hz = found(&readout).expect("hum followed");
        // It hears the middle of its 4 s window, and goes halfway each
        // check: about 2.5 s behind
        assert!(
            (hz - drift(secs - 2.5)).abs() < 0.01,
            "at {hz}, hum 2.5 s ago at {}",
            drift(secs - 2.5)
        );
        // Over the last second the hum hardly moves (0.005 Hz)
        let now = drift(secs - 0.5);
        for k in [1.0, 2.0, 4.0, 8.0] {
            let drop = db(level(&out, k * now, 1.0), level(&input, k * now, 1.0));
            assert!(drop < -15.0, "harmonic {k} only down {drop:.1} dB");
        }
    }

    #[test]
    fn a_rich_buzz_is_removed_whole() {
        // A buzz at 50.5 Hz, the second harmonic the loudest (like the
        // 101 Hz line on an old-time radio stream), twenty lines up to
        // 1 kHz; nothing above it is touched
        let amps: Vec<f64> = (1..=20)
            .map(|k| {
                if k == 2 {
                    0.03
                } else {
                    0.01 / (k as f64).sqrt()
                }
            })
            .collect();
        let mut tones = hum(50.5, &amps);
        tones.push((1500.0, 0.2));
        tones.push(MUSIC);
        let (out, readout) = run(signal(20.0, &tones, 0.01), &SoundParams::default());
        let hz = found(&readout).expect("buzz found");
        assert!((hz - 50.5).abs() < 0.02, "found at {hz}");
        for &(f, a) in &tones[..20] {
            let drop = db(level(&out, f, 2.0), a);
            assert!(drop < -15.0, "{f} Hz only down {drop:.1} dB");
        }
        assert!(db(level(&out, 1500.0, 2.0), 0.2).abs() < 0.05);
    }

    #[test]
    fn hum_needs_time_to_be_believed() {
        let input = signal(4.0, &hum(50.0, &[0.03, 0.02, 0.01]), 0.01);
        let (out, readout) = run(input.clone(), &SoundParams::default());
        assert_eq!(readout.hz(), None);
        assert_eq!(out, input);
    }

    #[test]
    fn removing_stops_once_the_hum_goes() {
        let mut input = signal(10.0, &hum(50.0, &[0.03, 0.02, 0.01]), 0.01);
        input.extend(signal(13.0, &[MUSIC], 0.01));
        let (out, readout) = run(input.clone(), &SoundParams::default());
        assert_eq!(readout.hz(), None);
        // The last second plays untouched again
        let n = RATE as usize;
        assert_eq!(out[out.len() - n..], input[input.len() - n..]);
    }

    #[test]
    fn switched_off_it_leaves_the_sound_alone() {
        let mut params = SoundParams::default();
        params.set_hum_filter(false);
        let input = signal(10.0, &hum(50.0, &[0.03, 0.02, 0.01]), 0.01);
        let (out, readout) = run(input.clone(), &params);
        assert_eq!(out, input);
        assert_eq!(readout.hz(), None);
    }

    #[test]
    fn silence_stays_silent() {
        let (out, readout) = run(vec![0.0; 10 * RATE as usize], &SoundParams::default());
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(readout.hz(), None);
    }

    /// The biggest step from one sample to the next
    fn largest_step(x: &[f32]) -> f32 {
        x.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn switching_off_and_on_never_clicks() {
        let mut tones = hum(50.0, &[0.05, 0.03, 0.02]);
        tones.push((200.0, 0.3));
        let input = signal(30.0, &tones, 0.0);
        let mut params = SoundParams::default();
        let readout = HumReadout::new_shared();
        let mut source = HumSource::new(
            SamplesBuffer::new(
                NonZero::new(1).unwrap(),
                NonZero::new(RATE).unwrap(),
                input.clone(),
            ),
            params.switches(),
            readout.clone(),
        );
        let second = RATE as usize;
        // Hum found and being removed by 10 s; off at 10 s, on at 15 s
        let mut out: Vec<f32> = source.by_ref().take(10 * second).collect();
        assert!(readout.hz().is_some());
        params.set_hum_filter(false);
        out.extend(source.by_ref().take(5 * second));
        assert_eq!(readout.hz(), None);
        params.set_hum_filter(true);
        out.extend(source);
        assert!(readout.hz().is_some(), "found again");
        // The notches fade: no step bigger than the sound's own
        let own = largest_step(&input);
        for at in [7, 10, 15, 22] {
            let near = &out[(at * second - second / 2)..(at * second + second)];
            let step = largest_step(near);
            assert!(step < own * 1.05, "a click near {at} s: {step} > {own}");
        }
        // Off, the fade done, the sound is the input again
        assert_eq!(
            out[12 * second..15 * second],
            input[12 * second..15 * second]
        );
    }

    #[test]
    fn notches_fade_in() {
        // Not all at once: halfway into the fade the hum is half gone
        let tones = hum(50.0, &[0.05]);
        let input = signal(12.0, &tones, 0.0);
        let params = SoundParams::default();
        let readout = HumReadout::new_shared();
        let mut source = HumSource::new(
            SamplesBuffer::new(NonZero::new(1).unwrap(), NonZero::new(RATE).unwrap(), input),
            params.switches(),
            readout.clone(),
        );
        // Take samples until the hum is found
        let mut out = Vec::new();
        while readout.hz().is_none() {
            out.push(source.next().expect("found in time"));
        }
        let start = out.len();
        out.extend(source.by_ref().take(RATE as usize * 2));
        let fade = (HUM_FADE_MS / 1000.0 * RATE as f32) as usize;
        let peak = |from: usize| {
            out[from..from + RATE as usize / 25]
                .iter()
                .fold(0.0f32, |m, x| m.max(x.abs()))
        };
        let early = peak(start);
        let middle = peak(start + fade / 2);
        let late = peak(start + 3 * fade);
        assert!(early > 0.045, "{early}");
        assert!(middle > 0.01 && middle < 0.04, "{middle}");
        assert!(late < 0.005, "{late}");
    }

    #[test]
    fn stereo_is_filtered_per_channel() {
        let mono = signal(10.0, &hum(50.0, &[0.03, 0.02, 0.01]), 0.01);
        let stereo: Vec<f32> = mono.iter().flat_map(|&x| [x, 0.0]).collect();
        let (out, readout) = run_at(RATE, 2, stereo, &SoundParams::default());
        assert!(readout.hz().is_some());
        assert!(out.iter().skip(1).step_by(2).all(|&x| x == 0.0));
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!(db(level(&left, 50.0, 2.0), 0.03) < -20.0);
    }

    #[test]
    fn stereo_hum_leaves_both_channels() {
        let hum_only = signal(12.0, &hum(60.0, &[0.03, 0.02]), 0.0);
        let left = add(&hum_only, &signal(12.0, &[(440.0, 0.3)], 0.01));
        let right = add(&hum_only, &signal(12.0, &[(650.0, 0.3)], 0.01));
        let stereo: Vec<f32> = left
            .iter()
            .zip(&right)
            .flat_map(|(&l, &r)| [l, r])
            .collect();
        let (out, _) = run_at(RATE, 2, stereo, &SoundParams::default());
        for (c, f) in [(0, 440.0), (1, 650.0)] {
            let ch: Vec<f32> = out.iter().skip(c).step_by(2).copied().collect();
            assert!(db(level(&ch, 60.0, 2.0), 0.03) < -20.0, "channel {c}");
            assert!(db(level(&ch, 120.0, 2.0), 0.02) < -20.0, "channel {c}");
            assert!(db(level(&ch, f, 2.0), 0.3).abs() < 0.1, "channel {c}");
        }
    }

    #[test]
    fn every_common_rate() {
        for rate in [22_050, 44_100, 48_000] {
            let tones = hum(49.25, &[0.02, 0.01, 0.005]);
            let input = render(rate, 20.0, 0.01, |t| {
                tones
                    .iter()
                    .map(|&(f, a)| a * (TAU * f * t).sin())
                    .sum::<f64>()
                    + 0.3 * (TAU * 440.0 * t).sin()
            });
            let (out, readout) = run_at(rate, 1, input, &SoundParams::default());
            let hz = found(&readout).unwrap_or_else(|| panic!("not found at {rate}"));
            assert!((hz - 49.25).abs() < 0.02, "{hz} at {rate}");
            for &(f, a) in &tones {
                let drop = db(level_at(rate, &out, f, 2.0), a);
                assert!(drop < -20.0, "{f} Hz at {rate}: {drop:.1} dB");
            }
        }
    }

    /// The cost to the music: on bass-heavy music with hum, once the hum is
    /// out, the output matches the music without hum everywhere but at
    /// the hum's own lines
    #[test]
    fn music_damage_is_small() {
        let rate = 44_100;
        let secs = 24.0;
        // No kick: one on every beat, exactly, is a ladder of lines 2 Hz
        // apart, and hum under it rightly doesn't stand out
        let music = bass_line(rate, secs, 0.0);
        let hum_only = render(rate, secs, 0.0, |t| {
            hum(50.0, &[0.02, 0.01, 0.005, 0.004, 0.003])
                .iter()
                .map(|&(f, a)| a * (TAU * f * t).sin())
                .sum()
        });
        let (out, readout) = run_at(rate, 1, add(&music, &hum_only), &SoundParams::default());
        assert!(readout.hz().is_some());
        // Over the last 8 s, the level in twelfth-octave bands from 25 Hz
        // to 2 kHz, against the music alone
        let n = 8 * rate as usize;
        let bands = |x: &[f32]| twelfth_octaves(rate, &x[x.len() - n..]);
        let (clean, humming, filtered) =
            (bands(&music), bands(&add(&music, &hum_only)), bands(&out));
        let mut heard = 0.0f64;
        for (b, mid) in band_middles().enumerate() {
            let (hum_band, change) = (
                (1..=5).any(|k| (mid / (50.0 * k as f64)).log2().abs() < 1.0 / 8.0),
                filtered[b] - clean[b],
            );
            heard = heard.max(humming[b] - clean[b]);
            if hum_band {
                // Within a semitone or so of a line: the hum is gone,
                // and a bass note a hertz from a line
                // (G1 at 49 Hz, its 2nd at 98 Hz) loses some of its
                // fundamental while it is: the price of the notches
                assert!(change < 0.3, "hum left at {mid:.0} Hz: {change:+.2} dB");
                assert!(change > -7.0, "music lost at {mid:.0} Hz: {change:+.2} dB");
            } else {
                assert!(change.abs() < 0.5, "music at {mid:.0} Hz: {change:+.2} dB");
            }
        }
        assert!(heard > 1.0, "the hum is heard: {heard:.2} dB");
    }

    /// The middles of twelfth-octave bands from 25 Hz to 2 kHz
    fn band_middles() -> impl Iterator<Item = f64> {
        (0..76).map(|b| 25.0 * 2f64.powf(b as f64 / 12.0))
    }

    /// Levels (dB) of `x` in the bands of [`band_middles`]
    fn twelfth_octaves(rate: u32, x: &[f32]) -> Vec<f64> {
        let n = x.len();
        let mut buf: Vec<Complex<f64>> = x
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let w = 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos();
                Complex::new(w * f64::from(v), 0.0)
            })
            .collect();
        FftPlanner::new().plan_fft_forward(n).process(&mut buf);
        let hz = f64::from(rate) / n as f64;
        band_middles()
            .map(|mid| {
                let edge = 2f64.powf(1.0 / 24.0);
                let e: f64 = buf[(mid / edge / hz) as usize..(mid * edge / hz) as usize]
                    .iter()
                    .map(|c| c.norm_sqr())
                    .sum();
                10.0 * (e + 1e-30).log10()
            })
            .collect()
    }

    #[test]
    fn decimator_keeps_the_low_lines() {
        let rate = 44_100.0;
        let mut d = Decimator::new(rate);
        let out_rate = d.rate(rate);
        assert!((2800.0..3200.0).contains(&out_rate), "{out_rate}");
        let x: Vec<f64> = (0..rate as usize)
            .filter_map(|i| d.push((TAU * 150.0 * i as f64 / rate).sin()))
            .collect();
        let peak = x[x.len() / 2..].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!((peak - 1.0).abs() < 0.02, "{peak}");
    }

    #[test]
    fn goertzel_matches_the_fft() {
        let d = Detector::new(RATE);
        let rate = d.rate;
        let n = d.ring.len();
        let x: Vec<f64> = (0..n)
            .map(|i| (TAU * 50.0 * i as f64 / rate).sin())
            .collect();
        let fft_len = d.spectrum.len();
        let mut buf: Vec<Complex<f64>> = x.iter().map(|&v| Complex::new(v, 0.0)).collect();
        buf.resize(fft_len, Complex::default());
        d.fft.process(&mut buf);
        let bin = 300;
        let hz = bin as f64 * rate / fft_len as f64;
        let [g] = powers(&x, &[hz], rate);
        let f = buf[bin].norm_sqr();
        assert!((g - f).abs() < 1e-6 * f.max(1.0), "{g} vs {f}");
    }
}
