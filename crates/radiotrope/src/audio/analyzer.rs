//! Audio analysis source
//!
//! `AnalyzingSource` wraps any `rodio::Source<Item=f32>` and computes VU meter
//! levels and FFT spectrum data, writing results to shared `AudioAnalysis` state.

use std::num::NonZero;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::Source;
use rustfft::{num_complex::Complex, FftPlanner};

use crate::config::audio::{FFT_SIZE, SPECTRUM_BANDS, VU_DECAY};

use super::types::AudioAnalysis;

/// Wrapper source that captures samples for visualization
pub struct AnalyzingSource<S> {
    inner: S,
    analysis: Arc<Mutex<AudioAnalysis>>,
    buffer_left: Vec<f32>,
    buffer_right: Vec<f32>,
    channels: NonZero<u16>,
    sample_rate: NonZero<u32>,
    fft_planner: FftPlanner<f32>,
    local_sample_count: u64,
}

impl<S> AnalyzingSource<S>
where
    S: Source<Item = f32>,
{
    /// Create a new analyzing wrapper around the given source
    pub fn new(source: S, analysis: Arc<Mutex<AudioAnalysis>>) -> Self {
        let channels = source.channels();
        let sample_rate = source.sample_rate();
        Self {
            inner: source,
            analysis,
            buffer_left: Vec::with_capacity(FFT_SIZE),
            buffer_right: Vec::with_capacity(FFT_SIZE),
            channels,
            sample_rate,
            fft_planner: FftPlanner::new(),
            local_sample_count: 0,
        }
    }

    fn process_buffers(&mut self) {
        if self.buffer_left.len() < FFT_SIZE {
            return;
        }

        // Compute RMS for VU meters
        let rms_left = (self.buffer_left.iter().map(|s| s * s).sum::<f32>()
            / self.buffer_left.len() as f32)
            .sqrt();
        let rms_right = if self.channels.get() >= 2 {
            (self.buffer_right.iter().map(|s| s * s).sum::<f32>() / self.buffer_right.len() as f32)
                .sqrt()
        } else {
            rms_left
        };

        // Compute FFT for spectrum analyzer
        let fft = self.fft_planner.plan_fft_forward(FFT_SIZE);
        let mut fft_input: Vec<Complex<f32>> = self
            .buffer_left
            .iter()
            .take(FFT_SIZE)
            .enumerate()
            .map(|(i, &s)| {
                // Hann window
                let window =
                    0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos());
                Complex::new(s * window, 0.0)
            })
            .collect();

        fft.process(&mut fft_input);

        let mut spectrum = [0.0f32; SPECTRUM_BANDS];
        let nyquist = FFT_SIZE / 2;
        let fft_norm = 1.0 / FFT_SIZE as f32;

        // Logarithmic frequency distribution
        for (band, spectrum_val) in spectrum.iter_mut().enumerate() {
            let low_freq = (band as f32 / SPECTRUM_BANDS as f32).powf(2.0);
            let high_freq = ((band + 1) as f32 / SPECTRUM_BANDS as f32).powf(2.0);

            let start = (low_freq * nyquist as f32) as usize;
            let end = ((high_freq * nyquist as f32) as usize)
                .max(start + 1)
                .min(nyquist);

            let mut max_mag = 0.0f32;
            for item in &fft_input[start..end] {
                let mag = item.norm() * fft_norm;
                max_mag = max_mag.max(mag);
            }

            *spectrum_val = (max_mag * 8.0).sqrt().min(1.0);
        }

        if let Ok(mut analysis) = self.analysis.lock() {
            analysis.vu_left = analysis.vu_left * VU_DECAY + rms_left * 3.0 * (1.0 - VU_DECAY);
            analysis.vu_right = analysis.vu_right * VU_DECAY + rms_right * 3.0 * (1.0 - VU_DECAY);

            for (i, spectrum_val) in spectrum.iter().enumerate() {
                analysis.spectrum[i] =
                    analysis.spectrum[i] * VU_DECAY + spectrum_val.min(1.0) * (1.0 - VU_DECAY);
            }

            analysis.sample_count = self.local_sample_count;
        }

        self.buffer_left.clear();
        self.buffer_right.clear();
    }
}

impl<S> Iterator for AnalyzingSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.inner.next()?;
        self.local_sample_count += 1;

        if self.channels.get() == 1 {
            self.buffer_left.push(sample);
            self.buffer_right.push(sample);
        } else if self.buffer_left.len() == self.buffer_right.len() {
            self.buffer_left.push(sample);
        } else {
            self.buffer_right.push(sample);
        }

        if self.buffer_left.len() >= FFT_SIZE && self.buffer_right.len() >= FFT_SIZE {
            self.process_buffers();
        }

        Some(sample)
    }
}

impl<S> Source for AnalyzingSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
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
    use crate::config::audio::{FFT_SIZE, SPECTRUM_BANDS};
    use rodio::buffer::SamplesBuffer;
    use std::num::NonZero;

    // --- Passthrough behavior ---

    #[test]
    fn passthrough_samples_mono() {
        let input: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input.clone(),
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        let output: Vec<f32> = analyzing.collect();
        assert_eq!(output, input);
    }

    #[test]
    fn passthrough_samples_stereo() {
        let input: Vec<f32> = (0..200).map(|i| (i as f32 - 100.0) / 100.0).collect();
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44100).unwrap(),
            input.clone(),
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        let output: Vec<f32> = analyzing.collect();
        assert_eq!(output, input);
    }

    #[test]
    fn passthrough_empty_source() {
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            Vec::<f32>::new(),
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        let output: Vec<f32> = analyzing.collect();
        assert!(output.is_empty());
    }

    #[test]
    fn passthrough_single_sample() {
        let input = vec![0.5f32];
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input.clone(),
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        let output: Vec<f32> = analyzing.collect();
        assert_eq!(output, input);
    }

    #[test]
    fn passthrough_exact_fft_size() {
        // Exactly FFT_SIZE mono samples should trigger processing once
        let input: Vec<f32> = (0..FFT_SIZE).map(|i| (i as f32 * 0.01).sin()).collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input.clone(),
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        let output: Vec<f32> = analyzing.collect();
        assert_eq!(output, input);
    }

    #[test]
    fn passthrough_large_buffer() {
        let num_samples = FFT_SIZE * 10;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.02).sin() * 0.5)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input.clone(),
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        let output: Vec<f32> = analyzing.collect();
        assert_eq!(output, input);
    }

    // --- VU meter behavior ---

    #[test]
    fn vu_nonzero_for_loud_signal() {
        let num_samples = FFT_SIZE * 2;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.9)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let analyzing = AnalyzingSource::new(source, analysis);

        let _: Vec<f32> = analyzing.collect();

        let data = analysis_ref.lock().unwrap();
        assert!(
            data.vu_left > 0.0,
            "VU left should be nonzero for loud signal"
        );
        assert!(
            data.vu_right > 0.0,
            "VU right should be nonzero for mono signal"
        );
    }

    #[test]
    fn vu_near_zero_for_silence() {
        let num_samples = FFT_SIZE * 2;
        let input: Vec<f32> = vec![0.0; num_samples];
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let analyzing = AnalyzingSource::new(source, analysis);

        let _: Vec<f32> = analyzing.collect();

        let data = analysis_ref.lock().unwrap();
        assert!(data.vu_left < 0.001, "VU left should be ~zero for silence");
        assert!(
            data.vu_right < 0.001,
            "VU right should be ~zero for silence"
        );
    }

    #[test]
    fn vu_louder_signal_gives_higher_value() {
        let num_samples = FFT_SIZE * 4;

        // Quiet signal
        let quiet: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.1)
            .collect();
        let analysis_quiet = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_quiet_ref = analysis_quiet.clone();
        let source_quiet = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            quiet,
        );
        let _: Vec<f32> = AnalyzingSource::new(source_quiet, analysis_quiet).collect();

        // Loud signal
        let loud: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.9)
            .collect();
        let analysis_loud = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_loud_ref = analysis_loud.clone();
        let source_loud =
            SamplesBuffer::new(NonZero::new(1).unwrap(), NonZero::new(44100).unwrap(), loud);
        let _: Vec<f32> = AnalyzingSource::new(source_loud, analysis_loud).collect();

        let quiet_vu = analysis_quiet_ref.lock().unwrap().vu_left;
        let loud_vu = analysis_loud_ref.lock().unwrap().vu_left;
        assert!(
            loud_vu > quiet_vu,
            "Loud signal VU ({}) should exceed quiet signal VU ({})",
            loud_vu,
            quiet_vu
        );
    }

    #[test]
    fn vu_mono_mirrors_left_and_right() {
        let num_samples = FFT_SIZE * 2;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.5)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // For mono, left and right should be equal
        assert!(
            (data.vu_left - data.vu_right).abs() < 0.001,
            "Mono VU left ({}) and right ({}) should be equal",
            data.vu_left,
            data.vu_right
        );
    }

    #[test]
    fn vu_very_quiet_signal() {
        let num_samples = FFT_SIZE * 2;
        // Very quiet: amplitude 0.001
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.001)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // Should be very small but possibly nonzero
        assert!(
            data.vu_left < 0.01,
            "Very quiet signal VU should be near zero"
        );
    }

    // --- Stereo channel separation ---

    #[test]
    fn stereo_left_loud_right_silent() {
        let num_frames = FFT_SIZE * 2;
        let mut input = Vec::with_capacity(num_frames * 2);
        for i in 0..num_frames {
            input.push((i as f32 * 0.1).sin() * 0.9); // left: loud
            input.push(0.0); // right: silent
        }
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert!(
            data.vu_left > data.vu_right,
            "Left should be louder than right"
        );
        assert!(data.vu_left > 0.0);
        assert!(data.vu_right < 0.001);
    }

    #[test]
    fn stereo_right_loud_left_silent() {
        let num_frames = FFT_SIZE * 2;
        let mut input = Vec::with_capacity(num_frames * 2);
        for i in 0..num_frames {
            input.push(0.0); // left: silent
            input.push((i as f32 * 0.1).sin() * 0.9); // right: loud
        }
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert!(
            data.vu_right > data.vu_left,
            "Right should be louder than left"
        );
        assert!(data.vu_right > 0.0);
        assert!(data.vu_left < 0.001);
    }

    #[test]
    fn stereo_both_channels_equal() {
        let num_frames = FFT_SIZE * 2;
        let mut input = Vec::with_capacity(num_frames * 2);
        for i in 0..num_frames {
            let s = (i as f32 * 0.1).sin() * 0.5;
            input.push(s); // left
            input.push(s); // right (same)
        }
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert!(
            (data.vu_left - data.vu_right).abs() < 0.01,
            "Equal stereo channels should have similar VU: L={} R={}",
            data.vu_left,
            data.vu_right
        );
    }

    // --- Spectrum analysis ---

    #[test]
    fn spectrum_nonzero_for_tonal_signal() {
        let num_samples = FFT_SIZE * 4;
        // 440 Hz sine wave at 44100 Hz sample rate
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin() * 0.9)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // At least one spectrum band should be nonzero
        assert!(
            data.spectrum.iter().any(|&v| v > 0.0),
            "Spectrum should have nonzero bands for tonal signal"
        );
    }

    #[test]
    fn spectrum_zero_for_silence() {
        let num_samples = FFT_SIZE * 2;
        let input: Vec<f32> = vec![0.0; num_samples];
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert!(
            data.spectrum.iter().all(|&v| v < 0.001),
            "Spectrum should be ~zero for silence"
        );
    }

    #[test]
    fn spectrum_has_correct_band_count() {
        let num_samples = FFT_SIZE * 2;
        let input: Vec<f32> = (0..num_samples).map(|i| (i as f32 * 0.1).sin()).collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(data.spectrum.len(), SPECTRUM_BANDS);
    }

    #[test]
    fn spectrum_values_clamped_to_one() {
        // Even with a very loud signal, spectrum should be <= 1.0
        let num_samples = FFT_SIZE * 4;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 1.0)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        for (i, &v) in data.spectrum.iter().enumerate() {
            assert!(v <= 1.0, "Spectrum band {} = {} exceeds 1.0", i, v);
            assert!(v >= 0.0, "Spectrum band {} = {} is negative", i, v);
        }
    }

    // --- Source trait preservation ---

    #[test]
    fn source_properties_preserved_mono() {
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(22050).unwrap(),
            vec![0.0f32; 100],
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        assert_eq!(analyzing.channels().get(), 1);
        assert_eq!(analyzing.sample_rate().get(), 22050);
    }

    #[test]
    fn source_properties_preserved_stereo() {
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(48000).unwrap(),
            vec![0.0f32; 100],
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        assert_eq!(analyzing.channels().get(), 2);
        assert_eq!(analyzing.sample_rate().get(), 48000);
    }

    #[test]
    fn source_properties_preserved_high_sample_rate() {
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(96000).unwrap(),
            vec![0.0f32; 100],
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);

        assert_eq!(analyzing.sample_rate().get(), 96000);
    }

    #[test]
    fn total_duration_is_none() {
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            vec![0.0f32; 100],
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analyzing = AnalyzingSource::new(source, analysis);
        // SamplesBuffer has a known duration
        // AnalyzingSource should pass through whatever the inner source says
        let _ = analyzing.total_duration();
    }

    // --- Buffer boundary behavior ---

    #[test]
    fn no_processing_below_fft_size() {
        // Fewer samples than FFT_SIZE should not trigger analysis
        let num_samples = FFT_SIZE - 1;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.9)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // Should still be at defaults since buffer never reached FFT_SIZE
        assert_eq!(data.vu_left, 0.0);
        assert_eq!(data.vu_right, 0.0);
    }

    #[test]
    fn processing_at_exact_fft_size() {
        // Exactly FFT_SIZE mono samples should trigger processing once
        let input: Vec<f32> = (0..FFT_SIZE)
            .map(|i| (i as f32 * 0.1).sin() * 0.9)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert!(
            data.vu_left > 0.0,
            "Should have processed at FFT_SIZE boundary"
        );
    }

    #[test]
    fn stereo_below_fft_size_no_processing() {
        // For stereo, need FFT_SIZE frames = FFT_SIZE * 2 interleaved samples
        // Give fewer frames
        let num_frames = FFT_SIZE - 1;
        let mut input = Vec::with_capacity(num_frames * 2);
        for i in 0..num_frames {
            input.push((i as f32 * 0.1).sin() * 0.9);
            input.push((i as f32 * 0.1).sin() * 0.9);
        }
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(
            data.vu_left, 0.0,
            "Should not process below FFT_SIZE frames"
        );
    }

    // --- Decay behavior ---

    #[test]
    fn multiple_processing_windows_accumulate() {
        // Multiple windows of loud signal should build up VU
        let num_samples = FFT_SIZE * 8;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.9)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // After 8 windows of loud signal, VU should be well above zero
        assert!(
            data.vu_left > 0.1,
            "Multiple windows should accumulate VU: {}",
            data.vu_left
        );
    }

    #[test]
    fn loud_then_silence_decays() {
        // First: loud signal to build up VU
        let loud_samples = FFT_SIZE * 4;
        let silent_samples = FFT_SIZE * 4;
        let mut input = Vec::with_capacity(loud_samples + silent_samples);
        for i in 0..loud_samples {
            input.push((i as f32 * 0.1).sin() * 0.9);
        }
        for _ in 0..silent_samples {
            input.push(0.0);
        }

        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // After silence, VU should have decayed toward zero
        // It won't be exactly zero due to decay, but should be less than peak
        assert!(
            data.vu_left < 0.5,
            "VU should decay after silence: {}",
            data.vu_left
        );
    }

    // --- Shared state correctness ---

    #[test]
    fn analysis_shared_state_updated() {
        let num_samples = FFT_SIZE * 2;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.5)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));

        // Clone before passing to ensure we can read after
        let analysis_read = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        // Read from the cloned Arc
        let data = analysis_read.lock().unwrap();
        assert!(data.vu_left > 0.0);
    }

    #[test]
    fn sample_count_tracks_all_samples() {
        let num_samples = FFT_SIZE * 3;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.1).sin() * 0.5)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // sample_count is flushed during process_buffers, which runs at FFT_SIZE boundaries
        // With 3*FFT_SIZE mono samples, we process 3 times, last flush at sample FFT_SIZE*3
        assert_eq!(data.sample_count, num_samples as u64);
    }

    #[test]
    fn sample_count_zero_for_empty_source() {
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            Vec::<f32>::new(),
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(data.sample_count, 0);
    }

    #[test]
    fn sample_count_not_flushed_below_fft_size() {
        // Fewer than FFT_SIZE samples means process_buffers never runs
        let num_samples = FFT_SIZE - 1;
        let input: Vec<f32> = vec![0.5; num_samples];
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // sample_count is only flushed in process_buffers, so it stays 0
        assert_eq!(data.sample_count, 0);
    }

    #[test]
    fn analysis_not_mutated_for_empty_source() {
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            Vec::<f32>::new(),
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(data.vu_left, 0.0);
        assert_eq!(data.vu_right, 0.0);
        assert!(data.spectrum.iter().all(|&v| v == 0.0));
    }

    // --- Extensive sample_count tests ---

    #[test]
    fn sample_count_stereo_tracks_all_interleaved_samples() {
        // Stereo: 2*FFT_SIZE frames = 4*FFT_SIZE interleaved samples
        let num_frames = FFT_SIZE * 2;
        let mut input = Vec::with_capacity(num_frames * 2);
        for i in 0..num_frames {
            input.push((i as f32 * 0.1).sin() * 0.5); // L
            input.push((i as f32 * 0.1).cos() * 0.5); // R
        }
        let total_samples = input.len();
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // sample_count counts every call to next(), which is every interleaved sample
        assert_eq!(data.sample_count, total_samples as u64);
    }

    #[test]
    fn sample_count_accumulates_across_many_windows() {
        // 10 FFT windows of mono data
        let num_samples = FFT_SIZE * 10;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.05).sin() * 0.7)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(data.sample_count, num_samples as u64);
    }

    #[test]
    fn sample_count_exact_fft_size_boundary() {
        // Exactly one FFT window
        let input: Vec<f32> = (0..FFT_SIZE).map(|i| (i as f32 * 0.1).sin()).collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(data.sample_count, FFT_SIZE as u64);
    }

    #[test]
    fn sample_count_exact_two_fft_size() {
        let num_samples = FFT_SIZE * 2;
        let input: Vec<f32> = (0..num_samples).map(|i| (i as f32 * 0.1).sin()).collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(data.sample_count, num_samples as u64);
    }

    #[test]
    fn sample_count_fft_plus_one() {
        // FFT_SIZE + 1 samples: process_buffers runs once at FFT_SIZE,
        // the +1 sample increments local_sample_count but no flush
        let num_samples = FFT_SIZE + 1;
        let input: Vec<f32> = vec![0.5; num_samples];
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // Only flushed once at FFT_SIZE, the +1 wasn't flushed
        assert_eq!(data.sample_count, FFT_SIZE as u64);
    }

    #[test]
    fn sample_count_large_buffer() {
        // 100 FFT windows = lots of accumulated samples
        let num_samples = FFT_SIZE * 100;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (i as f32 * 0.02).sin() * 0.3)
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(data.sample_count, num_samples as u64);
    }

    #[test]
    fn sample_count_single_sample() {
        let input = vec![0.42f32];
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // 1 sample < FFT_SIZE, so process_buffers never runs, count stays 0
        assert_eq!(data.sample_count, 0);
    }

    #[test]
    fn sample_count_stereo_below_fft_frames() {
        // FFT_SIZE/2 frames in stereo = FFT_SIZE interleaved samples
        // But buffers need FFT_SIZE frames each, so this won't trigger process_buffers
        let num_frames = FFT_SIZE / 2;
        let mut input = Vec::with_capacity(num_frames * 2);
        for i in 0..num_frames {
            input.push((i as f32 * 0.1).sin());
            input.push((i as f32 * 0.1).cos());
        }
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let _: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        // Not enough frames for process_buffers to run
        assert_eq!(data.sample_count, 0);
    }

    #[test]
    fn sample_count_consistent_with_output_length() {
        // The local_sample_count should match the total samples iterated,
        // but flushed count may lag. Verify flushed count <= total iterated.
        let num_samples = FFT_SIZE * 3 + 42; // not a multiple of FFT_SIZE
        let input: Vec<f32> = (0..num_samples).map(|i| (i as f32 * 0.1).sin()).collect();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            input,
        );
        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_ref = analysis.clone();
        let output: Vec<f32> = AnalyzingSource::new(source, analysis).collect();

        let data = analysis_ref.lock().unwrap();
        assert_eq!(output.len(), num_samples);
        // Flushed count is FFT_SIZE*3 (the last 42 weren't flushed)
        assert_eq!(data.sample_count, (FFT_SIZE * 3) as u64);
        assert!(data.sample_count <= output.len() as u64);
    }
}
