//! Streaming sample rate conversion
//!
//! Opus only takes 48 kHz, and a WAV file has one rate for the whole file,
//! so recordings at any other rate go through [`Resampler`] first. So does
//! playback when the station's rate isn't the device's
//! ([`ResampleSource`]): the output's own converter only interpolates
//! between samples, which dulls the treble and folds noise into it.
//! [`Resampler`] wraps rubato's FFT resampler, takes batches of any size,
//! and trims the resampler's start-up delay so the output lines up with
//! the input.

use std::num::NonZero;
use std::time::Duration;

use rodio::Source;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler as _};

/// Frames per resampler call on the input side.
const CHUNK_FRAMES: usize = 1024;

pub(crate) struct Resampler {
    fft: Fft<f32>,
    channels: usize,
    ratio: f64,
    /// Input not yet resampled, interleaved
    pending: Vec<f32>,
    /// Scratch output of one call, interleaved
    scratch: Vec<f32>,
    /// Output frames still to drop to remove the start-up delay
    to_trim: usize,
    /// Input frames taken in, and output frames given out (after trimming)
    frames_in: u64,
    frames_out: u64,
}

impl Resampler {
    pub(crate) fn new(from_rate: u32, to_rate: u32, channels: u16) -> Result<Self, String> {
        let channels = channels as usize;
        let fft = Fft::<f32>::new(
            from_rate as usize,
            to_rate as usize,
            CHUNK_FRAMES,
            channels,
            FixedSync::Input,
        )
        .map_err(|e| format!("Resampler: {e}"))?;
        let to_trim = fft.output_delay();
        Ok(Self {
            scratch: vec![0.0; fft.output_frames_max() * channels],
            fft,
            channels,
            ratio: to_rate as f64 / from_rate as f64,
            pending: Vec::new(),
            to_trim,
            frames_in: 0,
            frames_out: 0,
        })
    }

    /// Resample interleaved `input`, appending what is ready to `out`.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        self.pending.extend_from_slice(input);
        self.frames_in += (input.len() / self.channels) as u64;
        loop {
            let need = self.fft.input_frames_next();
            if self.pending.len() < need * self.channels {
                return Ok(());
            }
            let used = self.run(None, out)?;
            self.pending.drain(..used * self.channels);
        }
    }

    /// Resample what is left, and the resampler's delay, into `out`.
    pub(crate) fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
        let expected = (self.frames_in as f64 * self.ratio).round() as u64;
        // Feed the partial last chunk, then silence until all input is out.
        let mut partial = self.pending.len() / self.channels;
        let mut rounds = 0;
        while self.frames_out < expected && rounds < 64 {
            let need = self.fft.input_frames_next();
            self.pending.resize(need * self.channels, 0.0);
            let limit = expected - self.frames_out;
            let before = out.len();
            self.run(Some(partial), out)?;
            // Drop what goes past the end of the real input.
            let produced = ((out.len() - before) / self.channels) as u64;
            if produced > limit {
                out.truncate(before + limit as usize * self.channels);
                self.frames_out -= produced - limit;
            }
            self.pending.clear();
            partial = 0;
            rounds += 1;
        }
        Ok(())
    }

    /// One resampler call on the start of `pending`. Returns input frames used.
    fn run(&mut self, partial_len: Option<usize>, out: &mut Vec<f32>) -> Result<usize, String> {
        let need = self.fft.input_frames_next();
        let input =
            InterleavedSlice::new(&self.pending[..need * self.channels], self.channels, need)
                .map_err(|e| format!("Resampler: {e}"))?;
        let max_out = self.scratch.len() / self.channels;
        let mut output = InterleavedSlice::new_mut(&mut self.scratch, self.channels, max_out)
            .map_err(|e| format!("Resampler: {e}"))?;
        let indexing = Indexing {
            partial_len,
            ..Default::default()
        };
        let (used, produced) = self
            .fft
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|e| format!("Resampler: {e}"))?;

        let skip = self.to_trim.min(produced);
        self.to_trim -= skip;
        out.extend_from_slice(&self.scratch[skip * self.channels..produced * self.channels]);
        self.frames_out += (produced - skip) as u64;
        Ok(used)
    }
}

/// A `Source` adapter that brings the audio to the output device's
/// sample rate. Audio already at that rate passes as it is.
pub struct ResampleSource<S> {
    inner: S,
    target: Option<NonZero<u32>>,
    /// The format of what is in `out`
    channels: NonZero<u16>,
    sample_rate: NonZero<u32>,
    /// The rate coming in
    in_rate: NonZero<u32>,
    /// None while the rates match
    resampler: Option<Resampler>,
    batch: Vec<f32>,
    out: Vec<f32>,
    pos: usize,
    /// The first sample in a new format, read while `out` still held
    /// audio in the old one
    carry: Option<f32>,
    ended: bool,
}

impl<S> ResampleSource<S>
where
    S: Source<Item = f32>,
{
    /// `target` is the device's rate; None passes everything as it is
    pub fn new(inner: S, target: Option<NonZero<u32>>) -> Self {
        let channels = inner.channels();
        let rate = inner.sample_rate();
        let mut s = Self {
            inner,
            target,
            channels,
            sample_rate: rate,
            in_rate: rate,
            resampler: None,
            batch: Vec::new(),
            out: Vec::new(),
            pos: 0,
            carry: None,
            ended: false,
        };
        s.set_format(channels, rate);
        s
    }

    fn set_format(&mut self, channels: NonZero<u16>, rate: NonZero<u32>) {
        self.channels = channels;
        self.in_rate = rate;
        self.resampler = self
            .target
            .filter(|&target| target != rate)
            .and_then(|target| Resampler::new(rate.get(), target.get(), channels.get()).ok());
        self.sample_rate = match (&self.resampler, self.target) {
            (Some(_), Some(target)) => target,
            _ => rate,
        };
    }

    /// The resampler's last samples, into `out`
    fn finish(&mut self) {
        if let Some(mut r) = self.resampler.take() {
            let _ = r.finish(&mut self.out);
        }
    }

    /// Read a batch and convert it into `out`; false at the end of the
    /// stream. Only called with `out` played.
    fn fill(&mut self) -> bool {
        self.out.clear();
        self.pos = 0;
        let first = match self.carry.take() {
            Some(x) => x,
            None => match self.inner.next() {
                Some(x) => x,
                None => {
                    self.finish();
                    return false;
                }
            },
        };
        let channels = self.inner.channels();
        let rate = self.inner.sample_rate();
        if channels != self.channels || rate != self.in_rate {
            self.finish();
            if !self.out.is_empty() {
                self.carry = Some(first);
                return true;
            }
            self.set_format(channels, rate);
        }

        let width = usize::from(channels.get());
        self.batch.clear();
        self.batch.push(first);
        'frames: while self.batch.len() < CHUNK_FRAMES * width {
            for i in self.batch.len() % width..width {
                let Some(x) = self.inner.next() else {
                    break 'frames;
                };
                // A new format starts the next batch
                if i == 0 && (self.inner.channels() != channels || self.inner.sample_rate() != rate)
                {
                    self.carry = Some(x);
                    break 'frames;
                }
                self.batch.push(x);
            }
        }
        // Whole frames only
        let partial = self.batch.len() % width;
        if partial != 0 {
            self.batch.resize(self.batch.len() + width - partial, 0.0);
        }

        match self.resampler.as_mut() {
            Some(r) => {
                if r.process(&self.batch, &mut self.out).is_err() {
                    // Play it at its own rate rather than not at all
                    self.resampler = None;
                    self.sample_rate = self.in_rate;
                    self.out.clear();
                    self.out.extend_from_slice(&self.batch);
                }
            }
            None => self.out.extend_from_slice(&self.batch),
        }
        true
    }
}

impl<S> Iterator for ResampleSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if let Some(&x) = self.out.get(self.pos) {
                self.pos += 1;
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

impl<S> Source for ResampleSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        // What is waiting is all in one format
        Some(self.out.len() - self.pos)
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

    fn sine(rate: u32, freq: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 / rate as f32 * freq * std::f32::consts::TAU).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn converts_44100_to_48000_with_matching_length() {
        let input = sine(44100, 1000.0, 44100);
        let mut r = Resampler::new(44100, 48000, 2).unwrap();
        let mut out = Vec::new();
        // Uneven batch sizes, like the taps send
        for chunk in input.chunks(2 * 777) {
            r.process(chunk, &mut out).unwrap();
        }
        r.finish(&mut out).unwrap();
        assert_eq!(out.len(), 48000 * 2);

        // Still a 1 kHz sine: zero crossings of one channel over one second
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        let crossings = left
            .windows(2)
            .skip(100)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        assert!((995..=1001).contains(&crossings), "{crossings}");
        // Start-up delay trimmed: the signal starts right away
        assert!(left[..200].iter().any(|v| v.abs() > 0.3));
    }

    #[test]
    fn short_input_still_comes_out() {
        let input = sine(22050, 440.0, 300);
        let mut r = Resampler::new(22050, 48000, 2).unwrap();
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        r.finish(&mut out).unwrap();
        let expected = (300.0 * 48000.0 / 22050.0_f64).round() as usize;
        assert_eq!(out.len(), expected * 2);
    }

    fn source_of(rate: u32, samples: Vec<f32>) -> rodio::buffer::SamplesBuffer {
        rodio::buffer::SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(rate).unwrap(),
            samples,
        )
    }

    #[test]
    fn plays_a_station_at_the_device_rate() {
        let input = sine(44_100, 1_000.0, 44_100);
        let s = ResampleSource::new(source_of(44_100, input), NonZero::new(48_000));
        assert_eq!(s.sample_rate().get(), 48_000);
        let out: Vec<f32> = s.collect();
        assert_eq!(out.len(), 48_000 * 2);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        let crossings = left
            .windows(2)
            .skip(100)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        assert!((995..=1001).contains(&crossings), "{crossings}");
    }

    #[test]
    fn the_same_rate_passes_unchanged() {
        let input = sine(48_000, 1_000.0, 4_800);
        let out: Vec<f32> =
            ResampleSource::new(source_of(48_000, input.clone()), NonZero::new(48_000)).collect();
        assert_eq!(out, input);
        let out: Vec<f32> = ResampleSource::new(source_of(48_000, input.clone()), None).collect();
        assert_eq!(out, input);
    }

    #[test]
    fn keeps_the_treble() {
        // 15 kHz survives 44.1 -> 48 kHz at full level (linear
        // interpolation loses several dB there)
        let input = sine(44_100, 15_000.0, 44_100);
        let out: Vec<f32> =
            ResampleSource::new(source_of(44_100, input), NonZero::new(48_000)).collect();
        let peak = out[20_000..80_000]
            .iter()
            .fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.47, "{peak}");
    }
}
