//! The rumble filter: a gentle high-pass below the lowest bass note. It
//! takes out DC offset and the subsonic rumble some streams carry, which
//! the speakers can't play but which eats headroom and moves woofers.
//!
//! Switching it crossfades between the filtered and the untouched sound
//! over [`RUMBLE_FADE_MS`]. A cut from one to the other would step by
//! whatever DC offset the stream carries, and a step is a thump. So that
//! there is something to fade from, the filter keeps running while it is
//! off, which costs next to nothing.

use std::num::NonZero;
use std::sync::Arc;
use std::time::Duration;

use rodio::Source;

use super::filter::Biquad64;
use super::sound::FilterSwitches;
use crate::config::sound::{RUMBLE_FADE_MS, RUMBLE_HZ};

/// Channels filtered; more pass through as they are
const MAX_CHANNELS: usize = 8;

/// A `Source` adapter that high-passes every channel at [`RUMBLE_HZ`]
pub struct CleanupSource<S> {
    inner: S,
    filters: Vec<Biquad64>,
    channels: u16,
    sample_rate: u32,
    channel_index: usize,
    /// The Rumble Filter's switch; always on without one
    switches: Option<Arc<FilterSwitches>>,
    /// How much of the filtered sound is heard, 0 to 1 (before the
    /// smoothstep), how far that moves each frame, and where it is going
    mix: f32,
    mix_step: f32,
    on: bool,
    /// The mix for the frame being played
    frame_mix: f32,
}

impl<S> CleanupSource<S>
where
    S: Source<Item = f32>,
{
    pub fn new(inner: S) -> Self {
        let channels = inner.channels().get();
        let sample_rate = inner.sample_rate().get();
        Self {
            filters: Self::filters(channels, sample_rate),
            inner,
            channels,
            sample_rate,
            channel_index: 0,
            switches: None,
            mix: 1.0,
            mix_step: Self::mix_step(sample_rate),
            on: true,
            frame_mix: 1.0,
        }
    }

    fn mix_step(sample_rate: u32) -> f32 {
        1.0 / (RUMBLE_FADE_MS / 1000.0 * sample_rate.max(1) as f32).max(1.0)
    }

    /// Filter only while the Rumble Filter is on
    pub fn with_switch(mut self, switches: Arc<FilterSwitches>) -> Self {
        // It starts where the switch is, without a fade
        self.on = switches.rumble_filter();
        self.mix = if self.on { 1.0 } else { 0.0 };
        self.frame_mix = self.mix;
        self.switches = Some(switches);
        self
    }

    fn filters(channels: u16, sample_rate: u32) -> Vec<Biquad64> {
        let count = usize::from(channels).min(MAX_CHANNELS);
        vec![Biquad64::high_pass(RUMBLE_HZ, f64::from(sample_rate.max(1))); count]
    }
}

impl<S> Iterator for CleanupSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let x = self.inner.next()?;
        // The format can change between frames (see EqSource)
        if self.channel_index == 0 {
            let channels = self.inner.channels().get();
            let sample_rate = self.inner.sample_rate().get();
            if channels != self.channels || sample_rate != self.sample_rate {
                self.channels = channels;
                self.sample_rate = sample_rate;
                self.filters = Self::filters(channels, sample_rate);
                self.mix_step = Self::mix_step(sample_rate);
            }
            if let Some(switches) = &self.switches {
                self.on = switches.rumble_filter();
            }
            let want: f32 = if self.on { 1.0 } else { 0.0 };
            self.mix = want.clamp(self.mix - self.mix_step, self.mix + self.mix_step);
            // Smoothstep: no corner at either end of the fade
            let t = self.mix;
            self.frame_mix = t * t * (3.0 - 2.0 * t);
        }
        let ch = self.channel_index;
        self.channel_index = (self.channel_index + 1) % usize::from(self.channels.max(1));

        let Some(filter) = self.filters.get_mut(ch) else {
            return Some(x);
        };
        if !x.is_finite() {
            // It would stay in the filter's memory
            filter.reset();
            return Some(0.0);
        }
        let filtered = filter.run(f64::from(x)) as f32;
        Some(match self.frame_mix {
            m if m >= 1.0 => filtered,
            m if m <= 0.0 => x,
            m => x + m * (filtered - x),
        })
    }
}

impl<S> Source for CleanupSource<S>
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
    use rodio::buffer::SamplesBuffer;

    fn run(channels: u16, samples: Vec<f32>) -> Vec<f32> {
        CleanupSource::new(SamplesBuffer::new(
            NonZero::new(channels).unwrap(),
            NonZero::new(48_000).unwrap(),
            samples,
        ))
        .collect()
    }

    #[test]
    fn dc_goes_away() {
        let out = run(2, vec![0.3; 96_000]);
        let tail = &out[out.len() - 1_000..];
        assert!(tail.iter().all(|x| x.abs() < 1e-3), "{:?}", &tail[..4]);
    }

    #[test]
    fn music_passes() {
        let input: Vec<f32> = (0..48_000)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin())
            .collect();
        let out = run(1, input);
        // Shifted a little in time, but just as loud
        let peak = out[24_000..].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 0.005, "{peak}");
    }

    #[test]
    fn channels_are_filtered_apart() {
        // DC on the left only: the right stays silent
        let input: Vec<f32> = (0..20_000).flat_map(|_| [0.5, 0.0]).collect();
        let out = run(2, input);
        assert!(out.iter().skip(1).step_by(2).all(|&x| x == 0.0));
    }

    #[test]
    fn a_bad_sample_doesnt_stick() {
        let mut input = vec![0.0f32; 10_000];
        input[100] = f32::NAN;
        let out = run(1, input);
        assert!(out.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn switched_off_it_leaves_the_sound_alone() {
        let mut params = crate::audio::dsp::SoundParams::default();
        params.set_rumble_filter(false);
        let out: Vec<f32> = CleanupSource::new(SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(48_000).unwrap(),
            vec![0.3; 96_000],
        ))
        .with_switch(params.switches())
        .collect();
        assert!(out.iter().all(|&x| x == 0.3));
    }

    /// Plays `samples` (mono, or interleaved stereo), switching the Rumble
    /// Filter at the given frames
    fn switched(
        rate: u32,
        channels: u16,
        samples: Vec<f32>,
        start_on: bool,
        at: &[(usize, bool)],
    ) -> Vec<f32> {
        let mut params = crate::audio::dsp::SoundParams::default();
        params.set_rumble_filter(start_on);
        let mut source = CleanupSource::new(SamplesBuffer::new(
            NonZero::new(channels).unwrap(),
            NonZero::new(rate).unwrap(),
            samples,
        ))
        .with_switch(params.switches());
        let mut out = Vec::new();
        let mut frame = 0;
        loop {
            for &(when, on) in at {
                if when == frame {
                    params.set_rumble_filter(on);
                }
            }
            let mut got = false;
            for _ in 0..channels {
                if let Some(x) = source.next() {
                    out.push(x);
                    got = true;
                }
            }
            if !got {
                return out;
            }
            frame += 1;
        }
    }

    fn biggest_step(x: &[f32], channels: usize, channel: usize) -> f32 {
        let ch: Vec<f32> = x.iter().skip(channel).step_by(channels).copied().collect();
        ch.windows(2)
            .fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()))
    }

    /// A stream with a DC offset: switching off used to step straight
    /// back to it, a thump
    #[test]
    fn switching_on_and_off_doesnt_thump() {
        for rate in [22_050u32, 44_100, 48_000] {
            let second = rate as usize;
            for channels in [1u16, 2] {
                let samples = vec![0.3; 6 * second * usize::from(channels)];
                let out = switched(
                    rate,
                    channels,
                    samples,
                    true,
                    &[(2 * second, false), (4 * second, true)],
                );
                for c in 0..usize::from(channels) {
                    let step = biggest_step(
                        &out[second * usize::from(channels)..],
                        usize::from(channels),
                        c,
                    );
                    assert!(step < 1e-3, "{rate} Hz, {channels} ch: {step}");
                }
                // Off it is the stream again; on, the DC is gone again
                let frame = |f: usize| out[f * usize::from(channels)];
                assert!((frame(4 * second - 1) - 0.3).abs() < 1e-6);
                assert!(frame(6 * second - 1).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn the_crossfade_takes_about_fifty_ms() {
        let rate = 48_000u32;
        let out = switched(
            rate,
            1,
            vec![0.3; 2 * rate as usize],
            true,
            &[(rate as usize, false)],
        );
        let at = |ms: f32| out[rate as usize + (ms / 1000.0 * rate as f32) as usize];
        assert!(at(0.0).abs() < 1e-3);
        assert!((at(25.0) - 0.15).abs() < 0.02, "{}", at(25.0));
        assert!((at(RUMBLE_FADE_MS + 1.0) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn music_doesnt_click_when_switched() {
        // Music with a DC offset under it, switched twice
        let rate = 44_100u32;
        let input: Vec<f32> = (0..4 * rate as usize)
            .map(|i| {
                0.1 + 0.4 * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / rate as f32).sin()
            })
            .collect();
        let before = biggest_step(&input, 1, 0);
        let out = switched(
            rate,
            1,
            input,
            false,
            &[(rate as usize, true), (3 * rate as usize, false)],
        );
        let after = biggest_step(&out, 1, 0);
        assert!(after < before * 1.05, "{after} vs {before}");
    }

    #[test]
    fn starting_off_it_is_untouched_from_the_first_sample() {
        let out = switched(48_000, 1, vec![0.3; 4_800], false, &[]);
        assert!(out.iter().all(|&x| x == 0.3));
    }
}
