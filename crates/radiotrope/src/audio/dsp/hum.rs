//! The Hum Filter: finds mains hum (50 or 60 Hz and its harmonics, from bad
//! cables or grounding at the station) and takes it out with very narrow
//! notches. It only touches the sound while it hears hum.
//!
//! Hum is a thin, steady line with a ladder of harmonics. Every
//! [`HUM_CHECK_MS`] it measures each candidate line and the sound
//! [`HUM_BESIDE_HZ`] either side of it (windowed Goertzel filters on a mono
//! mix). It counts as hum once enough lines stand [`HUM_ABOVE_DB`] above
//! their neighbours for [`HUM_FOUND_CHECKS`] checks in a row: a bass note
//! is not that thin, that steady, or that long. The notches fade in and
//! out over [`HUM_FADE_MS`].

use std::num::NonZero;
use std::sync::Arc;
use std::time::Duration;

use rodio::Source;

use super::filter::Biquad64;
use super::sound::{FilterSwitches, HumReadout};
use crate::config::sound::*;

/// Channels filtered; more pass through as they are
const MAX_CHANNELS: usize = 8;

/// Mains frequencies looked for
const MAINS_HZ: [u32; 2] = [50, 60];

/// One frequency's Goertzel filter over a check
#[derive(Clone, Copy, Debug)]
struct Goertzel {
    coeff: f64,
    s1: f64,
    s2: f64,
}

impl Goertzel {
    fn new(freq: f64, sample_rate: f64) -> Self {
        Self {
            coeff: 2.0 * (2.0 * std::f64::consts::PI * freq / sample_rate).cos(),
            s1: 0.0,
            s2: 0.0,
        }
    }

    fn push(&mut self, x: f64) {
        let s = x + self.coeff * self.s1 - self.s2;
        self.s2 = self.s1;
        self.s1 = s;
    }

    /// Power at the frequency, then start over
    fn take(&mut self) -> f64 {
        let p = self.s1 * self.s1 + self.s2 * self.s2 - self.coeff * self.s1 * self.s2;
        self.s1 = 0.0;
        self.s2 = 0.0;
        p
    }
}

/// Listens for hum, one check at a time
struct Detector {
    /// Hann window over a check
    window: Vec<f32>,
    pos: usize,
    /// Per mains frequency and harmonic: the line, below it, above it
    filters: Vec<[Goertzel; 3]>,
    /// Sum of the window, to turn power into amplitude
    window_sum: f64,
    /// Checks in a row that heard each mains frequency
    heard: [u32; 2],
    /// Checks in a row without the hum being removed
    missed: u32,
    found: Option<u32>,
}

impl Detector {
    fn new(sample_rate: u32) -> Self {
        let len = (u64::from(sample_rate) * u64::from(HUM_CHECK_MS) / 1000).max(16) as usize;
        let window: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f64 / len as f64;
                (0.5 - 0.5 * (2.0 * std::f64::consts::PI * t).cos()) as f32
            })
            .collect();
        let rate = f64::from(sample_rate.max(1));
        let beside = f64::from(HUM_BESIDE_HZ);
        let filters = MAINS_HZ
            .iter()
            .flat_map(|&mains| {
                (1..=HUM_HARMONICS).map(move |k| {
                    let f = f64::from(mains) * k as f64;
                    [
                        Goertzel::new(f, rate),
                        Goertzel::new(f - beside, rate),
                        Goertzel::new(f + beside, rate),
                    ]
                })
            })
            .collect();
        Self {
            window_sum: window.iter().map(|&w| f64::from(w)).sum(),
            window,
            pos: 0,
            filters,
            heard: [0; 2],
            missed: 0,
            found: None,
        }
    }

    /// Take one mono sample; at the end of a check, say what was decided
    fn push(&mut self, x: f32) -> Option<Option<u32>> {
        let w = f64::from(x * self.window[self.pos]);
        for bank in &mut self.filters {
            for g in bank {
                g.push(w);
            }
        }
        self.pos += 1;
        if self.pos < self.window.len() {
            return None;
        }
        self.pos = 0;
        Some(self.decide())
    }

    fn decide(&mut self) -> Option<u32> {
        let above = 10f64.powf(f64::from(HUM_ABOVE_DB) / 10.0);
        // Power of a sine of amplitude A is (A * window_sum / 2)^2
        let min_power = (f64::from(HUM_MIN_LEVEL) * self.window_sum / 2.0).powi(2);
        let mut lines = [0usize; 2];
        let mut low = [false; 2];
        for (i, bank) in self.filters.iter_mut().enumerate() {
            let (mains, k) = (i / HUM_HARMONICS, i % HUM_HARMONICS + 1);
            let [line, below, above_] = bank;
            let (p, b, a) = (line.take(), below.take(), above_.take());
            let beside = (b + a) / 2.0;
            if p > min_power && p > beside * above {
                lines[mains] += 1;
                if k <= 2 {
                    low[mains] = true;
                }
            }
        }
        for m in 0..2 {
            if lines[m] >= HUM_MIN_LINES && low[m] {
                self.heard[m] += 1;
            } else {
                self.heard[m] = 0;
            }
        }
        match self.found {
            Some(hz) => {
                let m = MAINS_HZ.iter().position(|&f| f == hz).unwrap_or(0);
                if self.heard[m] > 0 {
                    self.missed = 0;
                } else {
                    self.missed += 1;
                    if self.missed >= HUM_GONE_CHECKS {
                        self.found = None;
                        self.missed = 0;
                    }
                }
            }
            None => {
                // The one heard longest, if long enough
                let m = if self.heard[0] >= self.heard[1] { 0 } else { 1 };
                if self.heard[m] >= HUM_FOUND_CHECKS {
                    self.found = Some(MAINS_HZ[m]);
                    self.missed = 0;
                }
            }
        }
        self.found
    }
}

/// A `Source` adapter that removes mains hum while it hears some
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
    /// Notches per channel for the hum being removed
    notches: Vec<Vec<Biquad64>>,
    notched_hz: Option<u32>,
    /// How much of the notched sound plays, 0..1
    mix: f32,
    mix_step: f32,
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
            notches: Vec::new(),
            notched_hz: None,
            mix: 0.0,
            mix_step: mix_step(sample_rate),
        }
    }

    /// Start over in a new format: no hum known
    fn reset(&mut self, channels: u16, sample_rate: u32) {
        self.channels = channels;
        self.sample_rate = sample_rate;
        self.detector = Detector::new(sample_rate);
        self.notches.clear();
        self.notched_hz = None;
        self.mix = 0.0;
        self.mix_step = mix_step(sample_rate);
        self.readout.set_if(self.owner, None);
    }

    /// The hum to remove changed (or went)
    fn decided(&mut self, found: Option<u32>) {
        if found != self.notched_hz {
            if let Some(hz) = found {
                // A new hum: new notches, faded in from nothing
                let rate = f64::from(self.sample_rate.max(1));
                let bank: Vec<Biquad64> = (1..=HUM_HARMONICS)
                    .map(|k| f64::from(hz) * k as f64)
                    .filter(|&f| f < rate * 0.45)
                    .map(|f| Biquad64::notch(f, HUM_NOTCH_HZ, rate))
                    .collect();
                let count = usize::from(self.channels).min(MAX_CHANNELS);
                self.notches = vec![bank; count];
                self.mix = 0.0;
                self.notched_hz = Some(hz);
            }
            // Gone: the notches fade out, then stop (see next)
        }
        self.readout.set_if(self.owner, found);
    }
}

fn mix_step(sample_rate: u32) -> f32 {
    1.0 / (HUM_FADE_MS / 1000.0 * sample_rate.max(1) as f32)
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
                    self.detector = Detector::new(self.sample_rate);
                    self.readout.set_if(self.owner, None);
                }
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
                if let Some(found) = self.detector.push(mono) {
                    self.decided(found);
                }
            }
        }

        let removing = self.on && self.detector.found.is_some();
        if ch == 0 {
            if removing {
                self.mix = (self.mix + self.mix_step).min(1.0);
            } else if self.mix > 0.0 {
                self.mix = (self.mix - self.mix_step).max(0.0);
                if self.mix == 0.0 {
                    self.notches.clear();
                    self.notched_hz = None;
                }
            }
        }
        if self.mix == 0.0 && !removing {
            return Some(x);
        }
        let Some(bank) = self.notches.get_mut(ch) else {
            return Some(x);
        };
        let mut y = f64::from(x);
        for notch in bank {
            y = notch.run(y);
        }
        Some(x + self.mix * (y as f32 - x))
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
    use std::f32::consts::TAU;

    const RATE: u32 = 48_000;

    /// Mono: the sum of (Hz, amplitude) sines, plus a little noise
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

    fn run(samples: Vec<f32>, params: &SoundParams) -> (Vec<f32>, Arc<HumReadout>) {
        let readout = HumReadout::new_shared();
        let out = HumSource::new(
            SamplesBuffer::new(
                NonZero::new(1).unwrap(),
                NonZero::new(RATE).unwrap(),
                samples,
            ),
            params.switches(),
            readout.clone(),
        )
        .collect();
        (out, readout)
    }

    /// Amplitude of `freq` in the last `secs` of `x`
    fn level(x: &[f32], freq: f32, secs: f32) -> f32 {
        let n = (secs * RATE as f32) as usize;
        let tail = &x[x.len() - n..];
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &v) in tail.iter().enumerate() {
            let ph = f64::from(TAU) * f64::from(freq) * i as f64 / f64::from(RATE);
            re += f64::from(v) * ph.cos();
            im += f64::from(v) * ph.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / n as f64) as f32
    }

    const MUSIC: (f32, f32) = (440.0, 0.3);

    #[test]
    fn mains_hum_is_found_and_removed() {
        let tones = [(50.0, 0.03), (100.0, 0.02), (150.0, 0.01), MUSIC];
        let (out, readout) = run(signal(12.0, &tones, 0.01), &SoundParams::default());
        assert_eq!(readout.hz(), Some(50));
        let hum = level(&out, 50.0, 2.0);
        assert!(hum < 0.03 / 10.0, "hum only down to {hum}");
        assert!(level(&out, 100.0, 2.0) < 0.02 / 10.0);
        let music = level(&out, 440.0, 2.0);
        assert!((music - 0.3).abs() < 0.3 * 0.06, "music changed: {music}");
    }

    #[test]
    fn sixty_hertz_hum_too() {
        let tones = [(60.0, 0.02), (120.0, 0.02), (180.0, 0.01), MUSIC];
        let (out, readout) = run(signal(12.0, &tones, 0.01), &SoundParams::default());
        assert_eq!(readout.hz(), Some(60));
        assert!(level(&out, 120.0, 2.0) < 0.002);
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

    #[test]
    fn hum_needs_time_to_be_believed() {
        let tones = [(50.0, 0.03), (100.0, 0.02), (150.0, 0.01)];
        let input = signal(4.0, &tones, 0.01);
        let (out, readout) = run(input.clone(), &SoundParams::default());
        assert_eq!(readout.hz(), None);
        assert_eq!(out, input);
    }

    #[test]
    fn removing_stops_once_the_hum_goes() {
        let mut input = signal(10.0, &[(50.0, 0.03), (100.0, 0.02), (150.0, 0.01)], 0.01);
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
        let input = signal(10.0, &[(50.0, 0.03), (100.0, 0.02), (150.0, 0.01)], 0.01);
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

    #[test]
    fn stereo_is_filtered_per_channel() {
        let mono = signal(10.0, &[(50.0, 0.03), (100.0, 0.02), (150.0, 0.01)], 0.01);
        let stereo: Vec<f32> = mono.iter().flat_map(|&x| [x, 0.0]).collect();
        let readout = HumReadout::new_shared();
        let out: Vec<f32> = HumSource::new(
            SamplesBuffer::new(
                NonZero::new(2).unwrap(),
                NonZero::new(RATE).unwrap(),
                stereo,
            ),
            SoundParams::default().switches(),
            readout.clone(),
        )
        .collect();
        assert_eq!(readout.hz(), Some(50));
        assert!(out.iter().skip(1).step_by(2).all(|&x| x == 0.0));
    }
}
