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

/// Filter coefficients for `band` at `gain_db`, or None when the band is
/// past what the sample rate can carry
fn band_coefficients(band: usize, gain_db: f32, sample_rate: f32) -> Option<Coefficients<f32>> {
    let freq = CENTER_FREQUENCIES[band];
    let (filter_type, freq, q) = if band == 0 {
        (Type::LowShelf(gain_db), freq, SHELF_Q)
    } else if band == NUM_BANDS - 1 {
        // The top of what the stream carries, at low sample rates
        let freq = freq.min(sample_rate * TOP_SHELF_MAX_FRACTION);
        (Type::HighShelf(gain_db), freq, SHELF_Q)
    } else {
        (Type::PeakingEQ(gain_db), freq, DEFAULT_Q)
    };
    if freq >= sample_rate / 2.0 {
        return None;
    }
    Coefficients::<f32>::from_params(filter_type, sample_rate.hz(), freq.hz(), q).ok()
}

/// Turns down peaks that would clip. One gain for all channels, so the
/// stereo image stays put: it drops at once on a peak and comes back over
/// `LIMITER_RELEASE_MS`.
struct Limiter {
    gain: f32,
    /// Share of the way back to full gain made each frame
    release: f32,
}

impl Limiter {
    fn new(sample_rate: u32) -> Self {
        let release_frames = LIMITER_RELEASE_MS / 1000.0 * sample_rate as f32;
        Self {
            gain: 1.0,
            release: 1.0 - (-1.0 / release_frames).exp(),
        }
    }

    /// Once per frame: let go a little
    fn release(&mut self) {
        self.gain += (1.0 - self.gain) * self.release;
        // Close enough: what doesn't clip passes untouched
        if self.gain > 0.9999 {
            self.gain = 1.0;
        }
    }

    fn apply(&mut self, sample: f32) -> f32 {
        let peak = sample.abs();
        if peak * self.gain > LIMITER_CEILING {
            self.gain = LIMITER_CEILING / peak;
        }
        // Rounding can land a hair past the ceiling
        (sample * self.gain).clamp(-LIMITER_CEILING, LIMITER_CEILING)
    }
}

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
    limiter: Limiter,
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
            limiter: Limiter::new(sample_rate),
        };
        s.recompute_coefficients();
        s
    }

    /// Start over with fresh filters for a new channel count or sample rate
    fn set_format(&mut self, channels: u16, sample_rate: u32) {
        self.channels = channels;
        self.sample_rate = sample_rate;
        let ch_count = (channels as usize).clamp(1, 8);
        self.filters = (0..ch_count)
            .map(|_| Self::make_default_filters())
            .collect();
        self.limiter = Limiter::new(sample_rate);
        self.recompute_coefficients();
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

        for band in 0..NUM_BANDS {
            if let Some(coeffs) = band_coefficients(band, params.gains_db[band], fs) {
                // Keep the filter state: resetting it on every slider move
                // makes an audible click
                for ch in 0..self.filters.len() {
                    self.filters[ch][band].update_coefficients(coeffs);
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

        // The stream's format can change between packets (HE-AAC turning on
        // SBR/PS, a chained Ogg song, an HLS ad break). Check at each frame
        // start: stale values would run channels through each other's
        // filters, or tune every band for the wrong sample rate.
        if self.channel_index == 0 {
            let channels = self.inner.channels().get();
            let sample_rate = self.inner.sample_rate().get();
            if channels != self.channels || sample_rate != self.sample_rate {
                self.set_format(channels, sample_rate);
            }
        }

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
        if let Some(filters) = self.filters.get_mut(ch) {
            for filter in filters.iter_mut() {
                sample = filter.run(sample);
            }
            // A NaN or infinite sample would stay in the filters' memory
            // and silence the channel for the rest of the stream
            if !sample.is_finite() {
                filters.iter_mut().for_each(Biquad::reset_state);
            }
        }
        if !sample.is_finite() {
            sample = 0.0;
        }

        if ch == 0 {
            self.limiter.release();
        }
        let sample = self.limiter.apply(sample);

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
                    (MIN_GAIN_DB..=MAX_GAIN_DB).contains(&g),
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

    // --- Format changes and parameter changes mid-stream ---

    /// Mono samples, then stereo samples: a stream whose format changes
    /// between packets (as `SymphoniaSource` reports it)
    struct MonoThenStereo {
        mono: Vec<f32>,
        stereo: Vec<f32>,
        pos: usize,
    }

    impl Iterator for MonoThenStereo {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            let v = if self.pos < self.mono.len() {
                self.mono[self.pos]
            } else {
                *self.stereo.get(self.pos - self.mono.len())?
            };
            self.pos += 1;
            Some(v)
        }
    }

    impl Source for MonoThenStereo {
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn channels(&self) -> NonZero<u16> {
            // The format of the sample just returned, as with
            // `SymphoniaSource` (it decodes a packet on the first `next`)
            nz16(if self.pos <= self.mono.len() { 1 } else { 2 })
        }
        fn sample_rate(&self) -> NonZero<u32> {
            nz32(44100)
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    fn bass_boost() -> SharedEqParams {
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.set_enabled(true);
            p.set_band(0, 12.0);
            p.set_band(1, 12.0);
        }
        params
    }

    fn tone(i: usize) -> f32 {
        (i as f32 * 100.0 * std::f32::consts::TAU / 44100.0).sin() * 0.2
    }

    #[test]
    fn channel_change_mid_stream_keeps_channels_apart() {
        let mono: Vec<f32> = (0..4410).map(tone).collect();
        // Stereo part: tone on the left, silence on the right
        let stereo: Vec<f32> = (0..4410).flat_map(|i| [tone(i), 0.0]).collect();
        // Mono source: `new` sees one channel
        let src = MonoThenStereo {
            mono,
            stereo,
            pos: 0,
        };
        let out: Vec<f32> = EqSource::new(src, bass_boost()).collect();
        let right_peak = out[4410..]
            .iter()
            .skip(1)
            .step_by(2)
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(
            right_peak < 1e-6,
            "silent right channel picked up {right_peak} from the left"
        );
    }

    #[test]
    fn moving_a_slider_does_not_click() {
        let params = bass_boost();
        let samples: Vec<f32> = (0..44100).map(tone).collect();
        let mut eq = EqSource::new(
            SamplesBuffer::new(nz16(1), nz32(44100), samples),
            params.clone(),
        );
        let mut out: Vec<f32> = eq.by_ref().take(22050).collect();
        // A small change on an unrelated band, as while dragging a slider
        params.lock().unwrap().set_band(6, 0.5);
        out.extend(eq);

        let jump = |range: std::ops::Range<usize>| {
            range
                .map(|i| (out[i] - out[i - 1]).abs())
                .fold(0.0f32, f32::max)
        };
        let steady = jump(11025..22040);
        let at_change = jump(22040..22100);
        assert!(
            at_change < steady * 1.5,
            "jump of {at_change} at the change, {steady} in steady state"
        );
    }

    // --- Clipping, shelves and bad samples ---

    /// A 1 kHz sine at `amplitude`, `secs` long, on every channel
    fn sine(channels: u16, amplitude: f32, secs: f32) -> Vec<f32> {
        let frames = (44100.0 * secs) as usize;
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * 1000.0 * std::f32::consts::TAU / 44100.0).sin() * amplitude;
                std::iter::repeat_n(v, channels as usize)
            })
            .collect()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    /// Gain in dB of `coeffs` at `freq` Hz
    fn response_db(coeffs: &Coefficients<f32>, freq: f32, sample_rate: f32) -> f32 {
        let w = std::f64::consts::TAU * freq as f64 / sample_rate as f64;
        let c = |k: f64| (k * w).cos();
        let s = |k: f64| (k * w).sin();
        let (b0, b1, b2) = (coeffs.b0 as f64, coeffs.b1 as f64, coeffs.b2 as f64);
        let (a1, a2) = (coeffs.a1 as f64, coeffs.a2 as f64);
        let num = (b0 + b1 * c(1.0) + b2 * c(2.0)).hypot(b1 * s(1.0) + b2 * s(2.0));
        let den = (1.0 + a1 * c(1.0) + a2 * c(2.0)).hypot(a1 * s(1.0) + a2 * s(2.0));
        (20.0 * (num / den).log10()) as f32
    }

    #[test]
    fn boosts_are_limited_instead_of_clipping() {
        let params = EqParams::new_shared();
        {
            let mut p = params.lock().unwrap();
            p.set_gains([MAX_GAIN_DB; NUM_BANDS], None);
            p.set_preamp(MAX_GAIN_DB);
            p.set_enabled(true);
        }
        let input = sine(2, 0.9, 0.5);
        let out: Vec<f32> =
            EqSource::new(SamplesBuffer::new(nz16(2), nz32(44100), input), params).collect();
        let loudest = peak(&out);
        assert!(
            loudest <= LIMITER_CEILING,
            "peak {loudest} past the ceiling"
        );
        // Turned down to the ceiling, not below it
        assert!(peak(&out[22050..]) > LIMITER_CEILING * 0.95);
    }

    #[test]
    fn the_limiter_leaves_what_doesnt_clip_alone() {
        let params = EqParams::new_shared();
        params.lock().unwrap().set_enabled(true);
        // Close to full scale, as many stations are
        let input = sine(2, 0.99, 0.2);
        let out: Vec<f32> = EqSource::new(
            SamplesBuffer::new(nz16(2), nz32(44100), input.clone()),
            params,
        )
        .collect();
        for (i, (a, b)) in input.iter().zip(&out).enumerate() {
            // The filters themselves round a little
            assert!((a - b).abs() < 1e-3, "sample {i}: {a} became {b}");
        }
    }

    #[test]
    fn the_limiter_lets_go_after_a_peak_and_keeps_the_balance() {
        let params = EqParams::new_shared();
        params.lock().unwrap().set_enabled(true);
        // Loud for half a second, then quiet: the left channel is always
        // four times the right
        let loud = sine(1, 2.0, 0.5);
        let quiet = sine(1, 0.5, 1.0);
        let input: Vec<f32> = loud
            .iter()
            .chain(&quiet)
            .flat_map(|&v| [v, v / 4.0])
            .collect();
        let out: Vec<f32> =
            EqSource::new(SamplesBuffer::new(nz16(2), nz32(44100), input), params).collect();

        for frame in out.chunks(2).filter(|f| f[0].abs() > 0.1) {
            let ratio = frame[1] / frame[0];
            assert!((ratio - 0.25).abs() < 1e-3, "balance moved: {frame:?}");
        }
        // The last quarter second plays at full level again
        let tail = &out[out.len() - 22050..];
        let left_peak = peak(&tail.iter().step_by(2).copied().collect::<Vec<_>>());
        assert!(
            (left_peak - 0.5).abs() < 0.01,
            "still turned down: {left_peak}"
        );
    }

    #[test]
    fn shelves_dont_overshoot() {
        for band in [0, NUM_BANDS - 1] {
            for gain in [MAX_GAIN_DB, MIN_GAIN_DB] {
                let coeffs = band_coefficients(band, gain, 44100.0).unwrap();
                let (lo, hi) = (gain.min(0.0) - 0.05, gain.max(0.0) + 0.05);
                let mut freq = 10.0;
                while freq < 22_000.0 {
                    let db = response_db(&coeffs, freq, 44100.0);
                    assert!(
                        (lo..=hi).contains(&db),
                        "band {band} at {gain} dB: {db:.2} dB at {freq:.0} Hz"
                    );
                    freq *= 1.05;
                }
            }
        }
    }

    #[test]
    fn the_top_band_works_at_low_sample_rates() {
        for rate in [22050.0, 32000.0] {
            let coeffs = band_coefficients(NUM_BANDS - 1, 6.0, rate)
                .unwrap_or_else(|| panic!("no top band at {rate} Hz"));
            let top = response_db(&coeffs, rate * 0.49, rate);
            let mid = response_db(&coeffs, 1000.0, rate);
            assert!(top > 5.0, "top of a {rate} Hz stream raised by {top:.2} dB");
            assert!(mid.abs() < 0.1, "1 kHz moved by {mid:.2} dB at {rate} Hz");
        }
        // Where 16 kHz fits, the band stays at 16 kHz
        let at_44 = band_coefficients(NUM_BANDS - 1, 6.0, 44100.0).unwrap();
        assert!((response_db(&at_44, 16_000.0, 44100.0) - 3.0).abs() < 0.2);
    }

    #[test]
    fn a_bad_sample_doesnt_silence_the_rest() {
        let params = bass_boost();
        let mut input: Vec<f32> = (0..4410).map(tone).collect();
        input.push(f32::NAN);
        input.push(f32::INFINITY);
        input.extend((0..4410).map(tone));
        let out: Vec<f32> =
            EqSource::new(SamplesBuffer::new(nz16(1), nz32(44100), input), params).collect();
        assert!(out.iter().all(|s| s.is_finite()), "non-finite output");
        let tail_peak = peak(&out[out.len() - 2205..]);
        assert!(tail_peak > 0.1, "silent after the bad sample: {tail_peak}");
    }
}
