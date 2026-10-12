//! The rumble filter: a gentle high-pass below the lowest bass note. It
//! takes out DC offset and the subsonic rumble some streams carry, which
//! the speakers can't play but which eats headroom and moves woofers.

use std::num::NonZero;
use std::time::Duration;

use rodio::Source;

use super::filter::Biquad64;
use crate::config::sound::RUMBLE_HZ;

/// Channels filtered; more pass through as they are
const MAX_CHANNELS: usize = 8;

/// A `Source` adapter that high-passes every channel at [`RUMBLE_HZ`]
pub struct CleanupSource<S> {
    inner: S,
    filters: Vec<Biquad64>,
    channels: u16,
    sample_rate: u32,
    channel_index: usize,
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
        }
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
            }
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
        Some(filter.run(f64::from(x)) as f32)
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
}
