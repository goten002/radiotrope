//! A lookahead limiter that keeps true peaks under the ceiling.
//!
//! The audio is held back a few milliseconds, so the gain is already down
//! when a peak comes out: a peak is turned down smoothly instead of having
//! its top cut off. Peaks between samples (which a DAC's reconstruction
//! produces) are estimated by interpolation, so they are caught too. One
//! gain for all channels keeps the stereo image in place.
//!
//! How the gain is worked out, per frame: the gain each frame needs
//! (ceiling over its peak) goes through a sliding minimum over the
//! lookahead, then an exponential release, then a moving average over the
//! lookahead. A frame comes out once every gain in that average was worked
//! out with it in view, so its gain is never above what it needs. The peak
//! between two samples is only known two frames later, so the minimum
//! reaches two frames further back and the audio waits two frames more.

use std::collections::VecDeque;
use std::num::NonZero;
use std::time::Duration;

use rodio::Source;

use crate::config::sound::*;

/// Most channels handled; more pass through unlimited
const MAX_CHANNELS: usize = 8;

/// The peak between `x1` and `x2`, estimated with a Catmull-Rom curve
/// through the four samples at quarter steps
fn between_peak(x0: f32, x1: f32, x2: f32, x3: f32) -> f32 {
    let mut peak = 0.0f32;
    for t in [0.25f32, 0.5, 0.75] {
        let t2 = t * t;
        let t3 = t2 * t;
        let v = 0.5
            * ((2.0 * x1)
                + (-x0 + x2) * t
                + (2.0 * x0 - 5.0 * x1 + 4.0 * x2 - x3) * t2
                + (-x0 + 3.0 * x1 - 3.0 * x2 + x3) * t3);
        peak = peak.max(v.abs());
    }
    peak
}

/// The gain work, separate from the `Source` plumbing so it can be tested
/// frame by frame
struct Limiter {
    channels: usize,
    ceiling: f32,
    /// Frames of lookahead (at least 1)
    lookahead: usize,
    release: f32,
    /// The last three samples of each channel, for the in-between peaks
    history: [[f32; 3]; MAX_CHANNELS],
    /// Frames held back, interleaved
    delay: VecDeque<f32>,
    /// (frame number, gain needed), increasing gains: the sliding minimum
    needed: VecDeque<(u64, f32)>,
    /// Released gains of the last `lookahead` frames, and their sum
    window: VecDeque<f32>,
    sum: f64,
    released: f32,
    frame: u64,
}

impl Limiter {
    fn new(channels: usize, sample_rate: u32) -> Self {
        let rate = sample_rate.max(1) as f32;
        let lookahead = ((LIMITER_LOOKAHEAD_MS / 1000.0 * rate) as usize).max(1);
        let release_frames = LIMITER_RELEASE_MS / 1000.0 * rate;
        Self {
            channels: channels.clamp(1, MAX_CHANNELS),
            ceiling: 10.0f32.powf(LIMITER_CEILING_DB / 20.0),
            lookahead,
            release: 1.0 - (-1.0 / release_frames.max(1.0)).exp(),
            history: [[0.0; 3]; MAX_CHANNELS],
            delay: VecDeque::with_capacity(lookahead * channels.clamp(1, MAX_CHANNELS)),
            needed: VecDeque::new(),
            window: std::iter::repeat_n(1.0, lookahead).collect(),
            sum: lookahead as f64,
            released: 1.0,
            frame: 0,
        }
    }

    /// Take one frame in; once the lookahead is full, a frame comes out
    /// into `out`, limited
    fn push(&mut self, frame: &[f32], out: &mut VecDeque<f32>) {
        let mut peak = 0.0f32;
        for (ch, &x) in frame.iter().enumerate().take(self.channels) {
            let [x0, x1, x2] = self.history[ch];
            peak = peak.max(x.abs()).max(between_peak(x0, x1, x2, x));
            self.history[ch] = [x1, x2, x];
        }
        let need = if peak > self.ceiling && peak.is_finite() {
            self.ceiling / peak
        } else if peak.is_finite() {
            1.0
        } else {
            // A NaN or infinity: the clamp below deals with the sample
            1.0
        };

        // Sliding minimum over this frame and the `lookahead + 2` before it
        while self.needed.back().is_some_and(|&(_, g)| g >= need) {
            self.needed.pop_back();
        }
        self.needed.push_back((self.frame, need));
        while self
            .needed
            .front()
            .is_some_and(|&(n, _)| n + (self.lookahead as u64) + 2 < self.frame)
        {
            self.needed.pop_front();
        }
        let min = self.needed.front().map_or(1.0, |&(_, g)| g);

        // Down at once, back up over the release
        self.released = min.min(self.released + (1.0 - self.released) * self.release);

        // Smooth the way down over the lookahead
        self.window.push_back(self.released);
        self.sum += f64::from(self.released);
        if let Some(old) = self.window.pop_front() {
            self.sum -= f64::from(old);
        }
        // Rounding in the running sum: start it over now and then
        if self.frame.is_multiple_of(4096) {
            self.sum = self.window.iter().map(|&g| f64::from(g)).sum();
        }
        let gain = (self.sum / self.lookahead as f64) as f32;

        self.delay.extend(frame.iter().copied());
        self.frame += 1;
        let width = frame.len();
        if self.delay.len() >= (self.lookahead + 2) * width {
            for _ in 0..width {
                if let Some(x) = self.delay.pop_front() {
                    out.push_back(self.limit(x, gain));
                }
            }
        }
    }

    fn limit(&self, x: f32, gain: f32) -> f32 {
        if !x.is_finite() {
            return 0.0;
        }
        (x * gain).clamp(-self.ceiling, self.ceiling)
    }

    /// The frames still held back, at the gain now
    fn flush(&mut self, out: &mut VecDeque<f32>) {
        let gain = (self.sum / self.lookahead as f64) as f32;
        while let Some(x) = self.delay.pop_front() {
            out.push_back(self.limit(x, gain));
        }
    }
}

/// A `Source` adapter that limits every sample (see the module docs).
/// It holds the audio back by [`LIMITER_LOOKAHEAD_MS`] and a frame.
pub struct LimiterSource<S> {
    inner: S,
    limiter: Limiter,
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

impl<S> LimiterSource<S>
where
    S: Source<Item = f32>,
{
    pub fn new(inner: S) -> Self {
        let channels = inner.channels();
        let sample_rate = inner.sample_rate();
        Self {
            limiter: Limiter::new(usize::from(channels.get()), sample_rate.get()),
            inner,
            channels,
            sample_rate,
            frame: Vec::with_capacity(MAX_CHANNELS),
            out: VecDeque::new(),
            carry: None,
            ended: false,
        }
    }

    /// Read the next frame and limit it; false at the end of the stream.
    /// Only called with `out` empty.
    fn fill(&mut self) -> bool {
        let first = match self.carry.take() {
            Some(x) => x,
            None => match self.inner.next() {
                Some(x) => x,
                None => {
                    self.limiter.flush(&mut self.out);
                    return false;
                }
            },
        };
        // The format can change between frames (see EqSource): play out
        // what is held back, then start over in the new format
        let channels = self.inner.channels();
        let sample_rate = self.inner.sample_rate();
        if channels != self.channels || sample_rate != self.sample_rate {
            self.limiter.flush(&mut self.out);
            if !self.out.is_empty() {
                self.carry = Some(first);
                return true;
            }
            self.channels = channels;
            self.sample_rate = sample_rate;
            self.limiter = Limiter::new(usize::from(channels.get()), sample_rate.get());
        }
        self.frame.clear();
        self.frame.push(first);
        for _ in 1..channels.get() {
            match self.inner.next() {
                Some(x) => self.frame.push(x),
                None => break,
            }
        }
        let frame = std::mem::take(&mut self.frame);
        self.limiter.push(&frame, &mut self.out);
        self.frame = frame;
        true
    }
}

impl<S> Iterator for LimiterSource<S>
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

impl<S> Source for LimiterSource<S>
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
    use rodio::buffer::SamplesBuffer;

    fn ceiling() -> f32 {
        10.0f32.powf(LIMITER_CEILING_DB / 20.0)
    }

    fn sine(channels: u16, rate: u32, freq: f32, amplitude: f32, secs: f32) -> Vec<f32> {
        let frames = (rate as f32 * secs) as usize;
        (0..frames)
            .flat_map(|i| {
                let v =
                    amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin();
                std::iter::repeat_n(v, channels as usize)
            })
            .collect()
    }

    fn run(channels: u16, rate: u32, samples: Vec<f32>) -> Vec<f32> {
        LimiterSource::new(SamplesBuffer::new(
            NonZero::new(channels).unwrap(),
            NonZero::new(rate).unwrap(),
            samples,
        ))
        .collect()
    }

    #[test]
    fn output_is_as_long_as_input() {
        let input = sine(2, 44_100, 440.0, 0.5, 0.3);
        let out = run(2, 44_100, input.clone());
        assert_eq!(out.len(), input.len());
    }

    #[test]
    fn quiet_audio_passes_unchanged() {
        let input = sine(2, 48_000, 440.0, 0.5, 0.2);
        let out = run(2, 48_000, input.clone());
        for (i, (a, b)) in input.iter().zip(&out).enumerate() {
            assert!((a - b).abs() < 1e-6, "sample {i}: {a} became {b}");
        }
    }

    #[test]
    fn loud_audio_never_passes_the_ceiling() {
        let out = run(2, 44_100, sine(2, 44_100, 1_000.0, 1.8, 0.5));
        let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak <= ceiling() + 1e-6, "{peak}");
        // And not squashed far below it
        assert!(peak > ceiling() * 0.9, "{peak}");
    }

    #[test]
    fn a_peak_is_turned_down_before_it_arrives() {
        // A single loud click in quiet audio: the samples just before it
        // already come out turned down, and no sample is cut off flat
        let mut input = vec![0.3f32; 2_000];
        input[1_000] = 2.0;
        let out = run(1, 48_000, input);
        let lookahead = (LIMITER_LOOKAHEAD_MS / 1000.0 * 48_000.0) as usize;
        let click = out[1_000];
        assert!(click <= ceiling() && click > ceiling() * 0.9, "{click}");
        assert!(out[995] < 0.3, "gain not down before the peak");
        assert!(out[1_000 - 2 * lookahead] > 0.299, "turned down too early");
    }

    #[test]
    fn it_lets_go_after_a_peak() {
        let mut input = vec![0.5f32; 48_000];
        input[1_000] = 3.0;
        let out = run(1, 48_000, input);
        assert!((out[40_000] - 0.5).abs() < 0.001, "{}", out[40_000]);
    }

    #[test]
    fn inter_sample_peaks_are_caught() {
        // A tone at a quarter of the rate, phase-shifted: the samples sit
        // at 0.707 of the true peak, so they are under the ceiling and the
        // peak between them is over it
        let rate = 48_000;
        let input: Vec<f32> = (0..4_800)
            .map(|i| {
                1.2 * (std::f32::consts::FRAC_PI_2 * i as f32 + std::f32::consts::FRAC_PI_4).sin()
            })
            .collect();
        let sample_peak = input.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(sample_peak < ceiling());
        let out = run(1, rate, input);
        let late = &out[2_000..];
        let peak = late.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak < sample_peak * 0.97, "true peak not limited: {peak}");
    }

    #[test]
    fn a_bad_sample_comes_out_as_silence() {
        let mut input = vec![0.2f32; 1_000];
        input[500] = f32::NAN;
        let out = run(1, 48_000, input);
        assert!(out.iter().all(|x| x.is_finite()));
        assert!((out[900] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn channels_share_one_gain() {
        // Loud left, quiet right: the right is turned down as much
        let input: Vec<f32> = (0..4_800)
            .flat_map(|i| {
                let v = (2.0 * std::f32::consts::PI * 500.0 * i as f32 / 48_000.0).sin();
                [v * 2.0, v * 0.5]
            })
            .collect();
        let out = run(2, 48_000, input);
        let right = out
            .iter()
            .skip(1)
            .step_by(2)
            .skip(1_000)
            .fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(right < 0.5 * 0.6, "{right}");
    }

    /// Mono first, then stereo: the frames keep their channels apart
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
        // Each sample comes with the format it is in
        let mut source = LimiterSource::new(Two { left: 3_000 });
        let mut out = Vec::new();
        while let Some(x) = source.next() {
            let want = if out.len() < 2_000 { 1 } else { 2 };
            assert_eq!(source.channels().get(), want, "sample {}", out.len());
            out.push(x);
        }
        assert_eq!(out.len(), 3_000);
        assert!(out[..2_000].iter().all(|&x| (x - 0.1).abs() < 1e-6));
        // Stereo part: alternating signs survive (channels not mixed up)
        let tail = &out[2_600..2_700];
        assert!(tail.windows(2).all(|w| w[0] * w[1] < 0.0));
    }
}
