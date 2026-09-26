//! Streaming sample rate conversion for recordings
//!
//! Opus only takes 48 kHz, and a WAV file has one rate for the whole file,
//! so audio at any other rate goes through [`Resampler`] first. It wraps
//! rubato's FFT resampler, takes batches of any size, and trims the
//! resampler's start-up delay so the output lines up with the input.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler as _};

/// Frames per resampler call on the input side.
const CHUNK_FRAMES: usize = 1024;

pub(super) struct Resampler {
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
    pub(super) fn new(from_rate: u32, to_rate: u32, channels: u16) -> Result<Self, String> {
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
    pub(super) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
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
    pub(super) fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
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
}
