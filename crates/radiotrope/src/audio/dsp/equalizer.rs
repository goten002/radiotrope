//! 10-band parametric equalizer
//!
//! Provides a `Source` wrapper that applies per-channel biquad filtering
//! using shared, hot-swappable parameters (`SharedEqParams`).

use std::num::NonZero;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};
use rodio::Source;

use crate::config::eq::*;

// ---------------------------------------------------------------------------
// Shared EQ parameters
// ---------------------------------------------------------------------------

/// Thread-safe handle to the current EQ state.
pub type SharedEqParams = Arc<Mutex<EqParams>>;

/// Equalizer parameters: per-band gains, preamp, and enable flag.
pub struct EqParams {
    pub gains_db: [f32; NUM_BANDS],
    pub preamp_db: f32,
    pub enabled: bool,
    pub preset_name: Option<String>,
    dirty: bool,
}

impl Default for EqParams {
    fn default() -> Self {
        Self {
            gains_db: [0.0; NUM_BANDS],
            preamp_db: 0.0,
            enabled: false,
            preset_name: Some("Flat".to_string()),
            dirty: true,
        }
    }
}

impl EqParams {
    /// Set a single band gain (clamped to MIN/MAX).
    pub fn set_band(&mut self, band: usize, gain_db: f32) {
        if band < NUM_BANDS {
            self.gains_db[band] = gain_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
            self.preset_name = None;
            self.dirty = true;
        }
    }

    /// Set the preamp gain (clamped to MIN/MAX).
    pub fn set_preamp(&mut self, db: f32) {
        self.preamp_db = db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        self.dirty = true;
    }

    /// Set all band gains at once, optionally naming a preset.
    pub fn set_gains(&mut self, gains: [f32; NUM_BANDS], preset_name: Option<String>) {
        for (i, &g) in gains.iter().enumerate() {
            self.gains_db[i] = g.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        }
        self.preset_name = preset_name;
        self.dirty = true;
    }

    /// Apply a named preset.
    pub fn apply_preset(&mut self, preset: &EqPreset) {
        self.gains_db = preset.gains;
        self.preset_name = Some(preset.name.to_string());
        self.dirty = true;
    }

    /// Enable or disable the equalizer.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.dirty = true;
    }

    /// Whether the parameters have changed since last acknowledged.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clear the dirty flag.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Create a new `SharedEqParams` with default (flat) settings.
    pub fn new_shared() -> SharedEqParams {
        Arc::new(Mutex::new(Self::default()))
    }
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

/// A named EQ preset.
pub struct EqPreset {
    pub name: &'static str,
    pub gains: [f32; NUM_BANDS],
}

pub const PRESETS: &[EqPreset] = &[
    EqPreset {
        name: "Flat",
        gains: [0.0; 10],
    },
    EqPreset {
        name: "Radio Enhance",
        gains: [1.0, 2.0, 1.0, -1.0, -0.5, 0.0, 1.5, 2.5, 2.0, 1.0],
    },
    EqPreset {
        name: "Rock",
        gains: [5.0, 4.0, 3.0, 1.5, -0.5, -1.0, 0.5, 2.5, 3.5, 4.0],
    },
    EqPreset {
        name: "Pop",
        gains: [-1.5, -1.0, 0.0, 2.0, 4.0, 4.0, 2.0, 0.0, -1.0, -1.5],
    },
    EqPreset {
        name: "Classical",
        gains: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -2.0, -2.0, -2.0, -4.0],
    },
    EqPreset {
        name: "Jazz",
        gains: [4.0, 3.0, 1.0, 2.0, -1.5, -1.5, 0.0, 1.0, 3.0, 4.0],
    },
    EqPreset {
        name: "Bass Boost",
        gains: [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    EqPreset {
        name: "Treble Boost",
        gains: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 4.0, 5.0, 6.0],
    },
    EqPreset {
        name: "Dance",
        gains: [6.0, 4.0, 2.0, 0.0, 0.0, -2.0, -1.0, 1.0, 4.0, 3.0],
    },
    EqPreset {
        name: "Soft",
        gains: [0.0, 1.0, 1.5, 2.0, 1.0, 0.0, -1.0, -1.5, -1.0, 0.0],
    },
    EqPreset {
        name: "Live",
        gains: [-2.0, 0.0, 2.0, 3.0, 3.5, 3.5, 3.0, 2.0, 1.0, 0.0],
    },
    EqPreset {
        name: "Club",
        gains: [0.0, 0.0, 4.0, 3.0, 3.0, 3.0, 2.0, 0.0, 0.0, 0.0],
    },
    EqPreset {
        name: "Headphones",
        gains: [3.0, 5.0, 3.0, 1.0, -1.0, -0.5, 1.0, 3.0, 5.0, 6.0],
    },
    EqPreset {
        name: "Techno",
        gains: [5.0, 4.0, 1.0, -2.0, -1.0, 0.0, 1.0, 3.0, 4.0, 4.5],
    },
];

/// Find a preset by name (case-insensitive).
pub fn find_preset(name: &str) -> Option<&'static EqPreset> {
    PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

// ---------------------------------------------------------------------------
// EqSource — rodio Source wrapper
// ---------------------------------------------------------------------------

/// A `Source` adapter that applies a 10-band parametric EQ to every sample.
pub struct EqSource<S> {
    inner: S,
    params: SharedEqParams,
    /// One set of biquad filters per channel.
    filters: Vec<[DirectForm1<f32>; NUM_BANDS]>,
    preamp_linear: f32,
    enabled: bool,
    channels: u16,
    sample_rate: u32,
    channel_index: usize,
}

impl<S> EqSource<S>
where
    S: Source<Item = f32>,
{
    /// Wrap `source` with an equalizer controlled by `params`.
    pub fn new(source: S, params: SharedEqParams) -> Self {
        let channels = source.channels().get();
        let sample_rate = source.sample_rate().get();
        let ch_count = (channels as usize).clamp(1, 8);

        let mut filters = Vec::with_capacity(ch_count);
        for _ in 0..ch_count {
            filters.push(Self::make_default_filters());
        }

        let mut s = Self {
            inner: source,
            params,
            filters,
            preamp_linear: 1.0,
            enabled: true,
            channels,
            sample_rate,
            channel_index: 0,
        };
        s.recompute_coefficients();
        s
    }

    /// Build a set of identity (0 dB peaking EQ) filters.
    fn make_default_filters() -> [DirectForm1<f32>; NUM_BANDS] {
        std::array::from_fn(|_| {
            let coeffs = Coefficients::<f32>::from_params(
                Type::PeakingEQ(0.0),
                44100.0.hz(),
                1000.0.hz(),
                DEFAULT_Q,
            )
            .unwrap();
            DirectForm1::<f32>::new(coeffs)
        })
    }

    /// Re-read shared params and rebuild filter coefficients.
    fn recompute_coefficients(&mut self) {
        let params = self.params.lock().unwrap_or_else(|e| e.into_inner());
        self.enabled = params.enabled;
        self.preamp_linear = 10.0_f32.powf(params.preamp_db / 20.0);

        let fs = self.sample_rate as f32;
        let nyquist = fs / 2.0;

        for (band, &freq) in CENTER_FREQUENCIES.iter().enumerate() {
            if freq >= nyquist {
                continue;
            }

            let gain = params.gains_db[band];
            let filter_type = if band == 0 {
                Type::LowShelf(gain)
            } else if band == NUM_BANDS - 1 {
                Type::HighShelf(gain)
            } else {
                Type::PeakingEQ(gain)
            };

            if let Ok(coeffs) =
                Coefficients::<f32>::from_params(filter_type, fs.hz(), freq.hz(), DEFAULT_Q)
            {
                for ch in 0..self.filters.len() {
                    self.filters[ch][band] = DirectForm1::<f32>::new(coeffs);
                }
            }
        }
        drop(params);
    }
}

impl<S> Iterator for EqSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next()?;

        // Check dirty flag (~25 ns uncontended lock)
        {
            let mut params = self.params.lock().unwrap_or_else(|e| e.into_inner());
            if params.is_dirty() {
                params.clear_dirty();
                drop(params);
                self.recompute_coefficients();
            }
        }

        if !self.enabled {
            self.channel_index = (self.channel_index + 1) % self.channels as usize;
            return Some(sample);
        }

        // Apply preamp
        let mut sample = sample * self.preamp_linear;

        // Cascade through biquad filters for the current channel
        let ch = self.channel_index;
        if ch < self.filters.len() {
            for band in 0..NUM_BANDS {
                sample = self.filters[ch][band].run(sample);
            }
        }

        self.channel_index = (self.channel_index + 1) % self.channels as usize;
        Some(sample)
    }
}

impl<S> Source for EqSource<S>
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;
    use std::num::NonZero;

    fn nz16(v: u16) -> NonZero<u16> {
        NonZero::new(v).unwrap()
    }
    fn nz32(v: u32) -> NonZero<u32> {
        NonZero::new(v).unwrap()
    }

    #[test]
    fn eq_params_default_is_flat() {
        let p = EqParams::default();
        assert!(p.gains_db.iter().all(|&g| g == 0.0));
        assert_eq!(p.preamp_db, 0.0);
        assert!(!p.enabled);
        assert_eq!(p.preset_name.as_deref(), Some("Flat"));
        assert!(p.is_dirty());
    }

    #[test]
    fn set_band_clamps_and_dirties() {
        let mut p = EqParams::default();
        p.clear_dirty();
        p.set_band(0, 20.0);
        assert_eq!(p.gains_db[0], MAX_GAIN_DB);
        assert!(p.is_dirty());
        assert!(p.preset_name.is_none());
    }

    #[test]
    fn set_band_out_of_range_is_noop() {
        let mut p = EqParams::default();
        p.clear_dirty();
        p.set_band(99, 5.0);
        assert!(!p.is_dirty());
    }

    #[test]
    fn set_preamp_clamps() {
        let mut p = EqParams::default();
        p.set_preamp(-20.0);
        assert_eq!(p.preamp_db, MIN_GAIN_DB);
        p.set_preamp(20.0);
        assert_eq!(p.preamp_db, MAX_GAIN_DB);
    }

    #[test]
    fn apply_preset_updates_name() {
        let mut p = EqParams::default();
        let rock = find_preset("Rock").unwrap();
        p.apply_preset(rock);
        assert_eq!(p.preset_name.as_deref(), Some("Rock"));
        assert_eq!(p.gains_db, rock.gains);
    }

    #[test]
    fn set_enabled_dirties() {
        let mut p = EqParams::default();
        p.clear_dirty();
        p.set_enabled(true);
        assert!(p.enabled);
        assert!(p.is_dirty());
    }

    #[test]
    fn find_preset_case_insensitive() {
        assert!(find_preset("rock").is_some());
        assert!(find_preset("ROCK").is_some());
        assert!(find_preset("nonexistent").is_none());
    }

    #[test]
    fn eq_source_passes_through_when_flat() {
        let samples: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin()).collect();
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), samples.clone());
        let params = EqParams::new_shared();
        {
            params.lock().unwrap().set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        assert_eq!(output.len(), 1000);
        // With flat EQ (0 dB all bands), output should be very close to input
        for (i, (&inp, &out)) in samples.iter().zip(output.iter()).enumerate() {
            assert!(
                (inp - out).abs() < 0.01,
                "sample {} diverged: {} vs {}",
                i,
                inp,
                out,
            );
        }
    }

    #[test]
    fn eq_source_disabled_is_passthrough() {
        let samples: Vec<f32> = (0..100).map(|i| (i as f32 * 0.05).sin()).collect();
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), samples.clone());
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.set_enabled(false);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        assert_eq!(samples, output);
    }

    #[test]
    fn eq_source_preserves_channels_and_rate() {
        let buf = SamplesBuffer::new(nz16(2), nz32(48000), vec![0.0f32; 100]);
        let params = EqParams::new_shared();
        let eq = EqSource::new(buf, params);
        assert_eq!(eq.channels().get(), 2);
        assert_eq!(eq.sample_rate(), nz32(48000));
    }

    #[test]
    fn preamp_amplifies_signal() {
        let samples = vec![0.5f32; 100];
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), samples);
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.set_preamp(6.0); // ~2x
            p.set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        // With +6 dB preamp, output should be roughly 2x the input
        for &s in &output {
            assert!(s > 0.9, "expected amplified sample, got {}", s);
        }
    }

    #[test]
    fn hot_swap_params_mid_stream() {
        let samples = vec![0.5f32; 1000];
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), samples);
        let params = EqParams::new_shared();
        {
            params.lock().unwrap().set_enabled(true);
        }
        let mut eq = EqSource::new(buf, params.clone());

        // Consume half
        for _ in 0..500 {
            let _ = eq.next();
        }

        // Change preset mid-stream
        {
            let mut p = params.lock().unwrap();
            p.apply_preset(find_preset("Rock").unwrap());
        }

        // Consume rest — should not panic
        let rest: Vec<f32> = eq.collect();
        assert_eq!(rest.len(), 500);
    }

    #[test]
    fn set_gains_with_preset_name() {
        let mut p = EqParams::default();
        p.set_gains([1.0; NUM_BANDS], Some("Custom".to_string()));
        assert_eq!(p.preset_name.as_deref(), Some("Custom"));
        assert!(p.gains_db.iter().all(|&g| g == 1.0));
    }

    #[test]
    fn all_presets_have_correct_band_count() {
        for preset in PRESETS {
            assert_eq!(
                preset.gains.len(),
                NUM_BANDS,
                "preset '{}' has wrong band count",
                preset.name
            );
        }
    }

    #[test]
    fn all_presets_within_gain_limits() {
        for preset in PRESETS {
            for (i, &g) in preset.gains.iter().enumerate() {
                assert!(
                    g >= MIN_GAIN_DB && g <= MAX_GAIN_DB,
                    "preset '{}' band {} gain {} out of range",
                    preset.name,
                    i,
                    g,
                );
            }
        }
    }

    #[test]
    fn all_presets_findable_by_name() {
        for preset in PRESETS {
            assert!(
                find_preset(preset.name).is_some(),
                "preset '{}' not findable",
                preset.name,
            );
        }
    }

    #[test]
    fn flat_preset_is_all_zeros() {
        let flat = find_preset("Flat").unwrap();
        assert!(flat.gains.iter().all(|&g| g == 0.0));
    }

    #[test]
    fn set_band_clamps_negative() {
        let mut p = EqParams::default();
        p.set_band(0, -20.0);
        assert_eq!(p.gains_db[0], MIN_GAIN_DB);
    }

    #[test]
    fn set_band_clears_preset_name() {
        let mut p = EqParams::default();
        assert_eq!(p.preset_name.as_deref(), Some("Flat"));
        p.set_band(5, 3.0);
        assert!(p.preset_name.is_none());
    }

    #[test]
    fn set_gains_clamps_each_band() {
        let mut p = EqParams::default();
        let gains = [20.0, -20.0, 5.0, -5.0, 0.0, 12.0, -12.0, 100.0, -100.0, 0.0];
        p.set_gains(gains, None);
        assert_eq!(p.gains_db[0], MAX_GAIN_DB);
        assert_eq!(p.gains_db[1], MIN_GAIN_DB);
        assert_eq!(p.gains_db[2], 5.0);
        assert_eq!(p.gains_db[3], -5.0);
        assert_eq!(p.gains_db[7], MAX_GAIN_DB);
        assert_eq!(p.gains_db[8], MIN_GAIN_DB);
    }

    #[test]
    fn set_enabled_toggle_roundtrip() {
        let mut p = EqParams::default();
        assert!(!p.enabled);
        p.set_enabled(true);
        assert!(p.enabled);
        p.set_enabled(false);
        assert!(!p.enabled);
    }

    #[test]
    fn dirty_flag_lifecycle() {
        let mut p = EqParams::default();
        assert!(p.is_dirty()); // dirty on creation
        p.clear_dirty();
        assert!(!p.is_dirty());
        p.set_band(0, 1.0);
        assert!(p.is_dirty());
        p.clear_dirty();
        p.set_preamp(1.0);
        assert!(p.is_dirty());
        p.clear_dirty();
        p.set_enabled(false);
        assert!(p.is_dirty());
        p.clear_dirty();
        p.set_gains([0.0; NUM_BANDS], None);
        assert!(p.is_dirty());
        p.clear_dirty();
        p.apply_preset(&PRESETS[0]);
        assert!(p.is_dirty());
    }

    #[test]
    fn preamp_attenuates_signal() {
        let samples = vec![0.5f32; 100];
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), samples);
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.set_preamp(-6.0); // ~0.5x
            p.set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        for &s in &output {
            assert!(s < 0.3 && s > 0.2, "expected attenuated sample, got {}", s);
        }
    }

    #[test]
    fn bass_boost_increases_low_freq_energy() {
        // Generate a 100 Hz sine wave (should be boosted by bass boost)
        let sample_rate = 44100;
        let num_samples = 4096;
        let freq = 100.0;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| {
                (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin() * 0.5
            })
            .collect();

        let rms_input = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();

        let buf = SamplesBuffer::new(nz16(1), nz32(sample_rate), samples);
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.apply_preset(find_preset("Bass Boost").unwrap());
            p.set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();

        // Skip first 512 samples (filter transient)
        let stable = &output[512..];
        let rms_output = (stable.iter().map(|s| s * s).sum::<f32>() / stable.len() as f32).sqrt();

        assert!(
            rms_output > rms_input * 1.05,
            "bass boost should increase 100Hz energy: input rms={:.4}, output rms={:.4}",
            rms_input,
            rms_output,
        );
    }

    #[test]
    fn treble_boost_increases_high_freq_energy() {
        // Generate a 10kHz sine wave (should be boosted by treble boost)
        let sample_rate = 44100;
        let num_samples = 4096;
        let freq = 10000.0;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| {
                (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin() * 0.5
            })
            .collect();

        let rms_input = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();

        let buf = SamplesBuffer::new(nz16(1), nz32(sample_rate), samples);
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.apply_preset(find_preset("Treble Boost").unwrap());
            p.set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();

        let stable = &output[512..];
        let rms_output = (stable.iter().map(|s| s * s).sum::<f32>() / stable.len() as f32).sqrt();

        assert!(
            rms_output > rms_input * 1.3,
            "treble boost should increase 10kHz energy: input rms={:.4}, output rms={:.4}",
            rms_input,
            rms_output,
        );
    }

    #[test]
    fn stereo_channels_processed_independently() {
        // L=440Hz, R=silence — EQ should only affect L
        let num_samples = 2000; // 1000 per channel
        let mut samples = Vec::with_capacity(num_samples);
        for i in 0..1000 {
            let l = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin() * 0.5;
            samples.push(l);
            samples.push(0.0); // R is silence
        }

        let buf = SamplesBuffer::new(nz16(2), nz32(44100), samples);
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.apply_preset(find_preset("Bass Boost").unwrap());
            p.set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();

        // Right channel (odd indices) should remain near zero
        let r_energy: f32 = output.iter().skip(1).step_by(2).map(|s| s * s).sum();
        let l_energy: f32 = output.iter().step_by(2).map(|s| s * s).sum();

        assert!(
            r_energy < l_energy * 0.01,
            "right channel should be near-silent: L energy={:.4}, R energy={:.4}",
            l_energy,
            r_energy,
        );
    }

    #[test]
    fn eq_source_empty_input() {
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), Vec::<f32>::new());
        let params = EqParams::new_shared();
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        assert!(output.is_empty());
    }

    #[test]
    fn eq_source_single_sample() {
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), vec![0.5]);
        let params = EqParams::new_shared();
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        assert_eq!(output.len(), 1);
    }

    #[test]
    fn eq_source_low_sample_rate() {
        // At 8kHz, bands above 4kHz Nyquist should be skipped gracefully
        let samples: Vec<f32> = (0..500).map(|i| (i as f32 * 0.1).sin()).collect();
        let buf = SamplesBuffer::new(nz16(1), nz32(8000), samples);
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.apply_preset(find_preset("Treble Boost").unwrap());
            p.set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        assert_eq!(output.len(), 500);
        // Should not panic or produce NaN
        assert!(output.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn all_presets_produce_finite_output() {
        let samples: Vec<f32> = (0..2048)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44100.0).sin() * 0.5)
            .collect();

        for preset in PRESETS {
            let buf = SamplesBuffer::new(nz16(2), nz32(44100), samples.clone());
            let params = EqParams::new_shared();
            {
                let mut p = params.lock().unwrap();
                p.apply_preset(preset);
                p.set_enabled(true);
            }
            let eq = EqSource::new(buf, params);
            let output: Vec<f32> = eq.collect();
            assert!(
                output.iter().all(|s| s.is_finite()),
                "preset '{}' produced non-finite output",
                preset.name,
            );
        }
    }

    #[test]
    fn max_boost_all_bands_no_panic() {
        let samples: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin()).collect();
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), samples);
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.set_gains([MAX_GAIN_DB; NUM_BANDS], None);
            p.set_preamp(MAX_GAIN_DB);
            p.set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        assert_eq!(output.len(), 1000);
        assert!(output.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn max_cut_all_bands_attenuates() {
        let samples = vec![0.5f32; 1000];
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), samples);
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.set_gains([MIN_GAIN_DB; NUM_BANDS], None);
            p.set_preamp(MIN_GAIN_DB);
            p.set_enabled(true);
        }
        let eq = EqSource::new(buf, params);
        let output: Vec<f32> = eq.collect();
        // With -12dB on everything + -12dB preamp, output should be very quiet
        let rms = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
        assert!(rms < 0.1, "expected heavily attenuated signal, rms={}", rms);
    }

    #[test]
    fn concurrent_param_updates_no_panic() {
        let samples = vec![0.5f32; 10000];
        let buf = SamplesBuffer::new(nz16(1), nz32(44100), samples);
        let params = EqParams::new_shared();
        let mut eq = EqSource::new(buf, params.clone());

        // Spawn a thread that rapidly changes params
        let params2 = params.clone();
        let writer = std::thread::spawn(move || {
            for i in 0..100 {
                let mut p = params2.lock().unwrap();
                p.set_band(
                    i % NUM_BANDS,
                    (i as f32 - 50.0).clamp(MIN_GAIN_DB, MAX_GAIN_DB),
                );
                p.set_preamp((i as f32 * 0.1 - 5.0).clamp(MIN_GAIN_DB, MAX_GAIN_DB));
                if i % 10 == 0 {
                    p.set_enabled(i % 20 == 0);
                }
            }
        });

        // Consume all samples concurrently
        let output: Vec<f32> = eq.by_ref().collect();
        writer.join().unwrap();

        assert_eq!(output.len(), 10000);
        assert!(output.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn new_shared_returns_default() {
        let shared = EqParams::new_shared();
        let p = shared.lock().unwrap();
        assert!(p.gains_db.iter().all(|&g| g == 0.0));
        assert_eq!(p.preamp_db, 0.0);
        assert!(!p.enabled);
    }
}
