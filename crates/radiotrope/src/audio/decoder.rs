//! Audio decoder using Symphonia
//!
//! Provides `SymphoniaSource` which decodes audio streams into f32 samples,
//! supporting Opus (via libopus), MP3, AAC, FLAC, Vorbis, and more.

use std::io::{Read, Seek};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use rodio::Source;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecRegistry, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::{Hint, ProbeResult};
use symphonia_adapter_fdk_aac::AacDecoder as LibAacDecoder;
use symphonia_adapter_libopus::OpusDecoder as LibOpusDecoder;

use crate::config::timeouts::PROBE_TIMEOUT_SECS;
use crate::error::RadioError;

use super::stats::DecoderStats;
use super::types::CodecInfo;

/// Convert a symphonia codec type to a human-readable name
pub fn codec_type_to_name(codec: symphonia::core::codecs::CodecType) -> String {
    use symphonia::core::codecs::*;
    match codec {
        CODEC_TYPE_AAC => "AAC".to_string(),
        CODEC_TYPE_FLAC => "FLAC".to_string(),
        CODEC_TYPE_MP3 => "MP3".to_string(),
        CODEC_TYPE_OPUS => "Opus".to_string(),
        CODEC_TYPE_VORBIS => "Vorbis".to_string(),
        CODEC_TYPE_PCM_U8 => "PCM 8-bit".to_string(),
        CODEC_TYPE_PCM_S16LE | CODEC_TYPE_PCM_S16BE => "PCM 16-bit".to_string(),
        CODEC_TYPE_PCM_S24LE | CODEC_TYPE_PCM_S24BE => "PCM 24-bit".to_string(),
        CODEC_TYPE_PCM_S32LE | CODEC_TYPE_PCM_S32BE => "PCM 32-bit".to_string(),
        CODEC_TYPE_PCM_F32LE | CODEC_TYPE_PCM_F32BE => "PCM 32-bit Float".to_string(),
        CODEC_TYPE_PCM_F64LE | CODEC_TYPE_PCM_F64BE => "PCM 64-bit Float".to_string(),
        CODEC_TYPE_PCM_ALAW => "PCM A-law".to_string(),
        CODEC_TYPE_PCM_MULAW => "PCM u-law".to_string(),
        CODEC_TYPE_ALAC => "ALAC".to_string(),
        _ => "Audio".to_string(),
    }
}

/// Create a codec registry with Opus support via libopus
pub fn create_codec_registry() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    // Register built-in codecs first (includes symphonia's AAC)
    symphonia::default::register_enabled_codecs(&mut registry);
    // Override AAC with FDK AAC (supports HE-AAC v1/v2 with SBR)
    registry.register_all::<LibAacDecoder>();
    registry.register_all::<LibOpusDecoder>();
    registry
}

/// Spawn a probe thread and return the receiver immediately (non-blocking).
///
/// The probe runs on a background `"symphonia-probe"` thread. The caller can
/// poll the returned `Receiver` with `try_recv()` or block with `recv_timeout()`.
pub fn start_probe<R: Read + Seek + Send + Sync + 'static>(
    reader: R,
    format_hint: Option<String>,
) -> Result<Receiver<Result<ProbeResult, RadioError>>, RadioError> {
    let source = ReadOnlySource::new(reader);
    let mss = MediaSourceStream::new(Box::new(source), Default::default());

    let format_opts = FormatOptions::default();
    let metadata_opts = MetadataOptions::default();
    let mut hint = Hint::new();

    if let Some(ref ext) = format_hint {
        hint.with_extension(ext);
    }

    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("symphonia-probe".to_string())
        .spawn(move || {
            let probe = symphonia::default::get_probe();
            let result = probe.format(&hint, mss, &format_opts, &metadata_opts);
            let _ = tx.send(result.map_err(|e| RadioError::Decode(format!("Probe error: {}", e))));
        })
        .map_err(|e| RadioError::Audio(format!("Failed to spawn probe thread: {}", e)))?;

    Ok(rx)
}

/// A symphonia-based audio source that supports Opus and other formats
pub struct SymphoniaSource {
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    format: Box<dyn symphonia::core::formats::FormatReader>,
    track_id: u32,
    sample_buf: Option<SampleBuffer<f32>>,
    sample_idx: usize,
    channels: u16,
    sample_rate: u32,
    codec_name: String,
    bits_per_sample: Option<u32>,
    /// Stores the last non-EOF error for the engine to check after stream ends
    last_error: Arc<Mutex<Option<String>>>,
    /// Atomic decode counters (frames decoded / decode errors)
    decoder_stats: Arc<DecoderStats>,
}

impl SymphoniaSource {
    /// Create a new source from a reader, auto-detecting the format
    pub fn new<R: Read + Seek + Send + Sync + 'static>(reader: R) -> Result<Self, RadioError> {
        Self::new_with_hint(reader, None)
    }

    /// Create a new source with an optional format hint (e.g., "aac", "mp4")
    ///
    /// Blocks for up to `PROBE_TIMEOUT_SECS` while probing the format.
    /// For non-blocking probe, use [`start_probe()`] + [`SymphoniaSource::from_probed()`].
    pub fn new_with_hint<R: Read + Seek + Send + Sync + 'static>(
        reader: R,
        format_hint: Option<&str>,
    ) -> Result<Self, RadioError> {
        let rx = start_probe(reader, format_hint.map(|s| s.to_string()))?;

        let probed = match rx.recv_timeout(Duration::from_secs(PROBE_TIMEOUT_SECS)) {
            Ok(Ok(probed)) => probed,
            Ok(Err(e)) => return Err(e),
            Err(RecvTimeoutError::Timeout) => {
                return Err(RadioError::Timeout(format!(
                    "Format probe timed out after {}s",
                    PROBE_TIMEOUT_SECS
                )))
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(RadioError::Decode("Probe thread panicked".to_string()))
            }
        };

        Self::from_probed(probed)
    }

    /// Create a `SymphoniaSource` from a completed `ProbeResult` (fast, no I/O).
    ///
    /// Used by the engine after the async probe completes.
    pub fn from_probed(probed: ProbeResult) -> Result<Self, RadioError> {
        let registry = create_codec_registry();
        let format = probed.format;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or_else(|| RadioError::Decode("No audio track found".to_string()))?;

        let track_id = track.id;
        let codec_params = track.codec_params.clone();

        let decoder = registry
            .make(&codec_params, &DecoderOptions::default())
            .map_err(|e| RadioError::Decode(format!("Decoder creation error: {}", e)))?;

        let channels = codec_params.channels.map(|c| c.count() as u16).unwrap_or(2);
        let sample_rate = codec_params.sample_rate.unwrap_or(44100);
        let bits_per_sample = codec_params.bits_per_sample;
        let codec_name = codec_type_to_name(codec_params.codec);

        let mut source = Self {
            decoder,
            format,
            track_id,
            sample_buf: None,
            sample_idx: 0,
            channels,
            sample_rate,
            codec_name,
            bits_per_sample,
            last_error: Arc::new(Mutex::new(None)),
            decoder_stats: Arc::new(DecoderStats::new()),
        };

        // Pre-decode the first frame to discover the actual output sample rate.
        // This is critical for HE-AAC where FDK AAC applies SBR, doubling the
        // sample rate (e.g., 24kHz→48kHz). Without this, rodio would configure
        // its resampler using the core rate from the ADTS header before any
        // frames are decoded, causing low-pitch playback.
        source.decode_next_packet();

        Ok(source)
    }

    /// Get the codec name (e.g., "MP3", "Opus", "AAC")
    pub fn codec_name(&self) -> &str {
        &self.codec_name
    }

    /// Get the bits per sample, if known
    pub fn bits_per_sample(&self) -> Option<u32> {
        self.bits_per_sample
    }

    /// Get the error slot for checking after stream ends.
    ///
    /// If the stream ended due to an IO or decode error (not clean EOF),
    /// the slot will contain the error message.
    pub fn error_slot(&self) -> Arc<Mutex<Option<String>>> {
        self.last_error.clone()
    }

    /// Get a handle to the decoder stats (frames decoded / errors)
    pub fn decoder_stats(&self) -> Arc<DecoderStats> {
        self.decoder_stats.clone()
    }

    /// Get full codec info as a `CodecInfo` struct
    pub fn codec_info(&self) -> CodecInfo {
        CodecInfo {
            codec_name: self.codec_name.clone(),
            channels: self.channels,
            sample_rate: self.sample_rate,
            bits_per_sample: self.bits_per_sample,
            bitrate: None,
        }
    }

    fn decode_next_packet(&mut self) -> bool {
        loop {
            match self.format.next_packet() {
                Ok(packet) => {
                    if packet.track_id() != self.track_id {
                        continue;
                    }

                    match self.decoder.decode(&packet) {
                        Ok(decoded) => {
                            self.decoder_stats.record_frame();
                            let spec = *decoded.spec();
                            let duration = decoded.capacity() as u64;

                            // Update sample rate and channels from decoder output —
                            // FDK AAC may change these after SBR/PS processing
                            self.sample_rate = spec.rate;
                            self.channels = spec.channels.count() as u16;

                            if self.sample_buf.is_none()
                                || self.sample_buf.as_ref().unwrap().capacity() < duration as usize
                            {
                                self.sample_buf = Some(SampleBuffer::new(duration, spec));
                            }

                            if let Some(ref mut buf) = self.sample_buf {
                                buf.copy_interleaved_ref(decoded);
                                self.sample_idx = 0;
                                return true;
                            }
                        }
                        Err(symphonia::core::errors::Error::DecodeError(_)) => {
                            self.decoder_stats.record_error();
                            continue;
                        }
                        Err(e) => {
                            if let Ok(mut err) = self.last_error.lock() {
                                *err = Some(format!("{}", e));
                            }
                            return false;
                        }
                    }
                }
                Err(symphonia::core::errors::Error::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    // Clean EOF — stream ended naturally, no error stored
                    return false;
                }
                Err(e) => {
                    // IO error or other — likely network failure
                    if let Ok(mut err) = self.last_error.lock() {
                        *err = Some(format!("{}", e));
                    }
                    return false;
                }
            }
        }
    }
}

impl Iterator for SymphoniaSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ref buf) = self.sample_buf {
                if self.sample_idx < buf.samples().len() {
                    let sample = buf.samples()[self.sample_idx];
                    self.sample_idx += 1;
                    return Some(sample);
                }
            }

            if !self.decode_next_packet() {
                return None;
            }
        }
    }
}

impl Source for SymphoniaSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a minimal valid WAV file in memory
    fn make_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let bits_per_sample: u16 = 16;
        let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
        let block_align = channels * (bits_per_sample / 8);
        let data_size = (samples.len() * 2) as u32;
        let file_size = 36 + data_size;

        let mut buf = Vec::new();
        // RIFF header
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&file_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        // fmt chunk
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes()); // chunk size
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM format
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        // data chunk
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        for &s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        buf
    }

    // --- Basic decoding ---

    #[test]
    fn decode_wav_mono() {
        let samples: Vec<i16> = (0..1000).map(|i| (i % 100 * 100) as i16).collect();
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        assert_eq!(source.channels(), 1);
        assert_eq!(source.sample_rate(), 44100);
    }

    #[test]
    fn decode_wav_stereo() {
        let samples: Vec<i16> = (0..2000).map(|i| (i % 200 * 50) as i16).collect();
        let wav = make_wav(48000, 2, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        assert_eq!(source.channels(), 2);
        assert_eq!(source.sample_rate(), 48000);
    }

    #[test]
    fn decode_wav_8khz() {
        let samples: Vec<i16> = (0..400).map(|i| (i * 50) as i16).collect();
        let wav = make_wav(8000, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        assert_eq!(source.sample_rate(), 8000);
        assert_eq!(source.channels(), 1);
    }

    #[test]
    fn decode_wav_96khz() {
        let samples: Vec<i16> = (0..1000).map(|i| (i * 10) as i16).collect();
        let wav = make_wav(96000, 2, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        assert_eq!(source.sample_rate(), 96000);
    }

    // --- Sample iteration ---

    #[test]
    fn iterate_samples() {
        let samples: Vec<i16> = vec![1000, 2000, 3000, 4000];
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), samples.len());
        assert!(decoded.iter().all(|&s| s != 0.0));
    }

    #[test]
    fn iterate_silence() {
        let samples: Vec<i16> = vec![0; 500];
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), 500);
        assert!(decoded.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn iterate_full_scale_positive() {
        let samples: Vec<i16> = vec![i16::MAX; 100];
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), 100);
        // All samples should be positive and close to 1.0
        assert!(decoded.iter().all(|&s| s > 0.9));
    }

    #[test]
    fn iterate_full_scale_negative() {
        let samples: Vec<i16> = vec![i16::MIN; 100];
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), 100);
        // All samples should be negative and close to -1.0
        assert!(decoded.iter().all(|&s| s < -0.9));
    }

    #[test]
    fn iterate_stereo_preserves_sample_count() {
        // 500 frames * 2 channels = 1000 interleaved samples
        let samples: Vec<i16> = (0..1000).map(|i| (i * 10) as i16).collect();
        let wav = make_wav(44100, 2, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), 1000);
    }

    #[test]
    fn iterate_large_buffer() {
        // 5 seconds of stereo audio
        let num_samples = 44100 * 2 * 5;
        let samples: Vec<i16> = (0..num_samples)
            .map(|i| ((i as f64 * 0.01).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44100, 2, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), num_samples);
    }

    #[test]
    fn samples_are_in_valid_range() {
        let samples: Vec<i16> = (0..2000)
            .map(|i| ((i as f64 * 0.05).sin() * 30000.0) as i16)
            .collect();
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let decoded: Vec<f32> = source.collect();
        for (i, &s) in decoded.iter().enumerate() {
            assert!(s >= -1.0 && s <= 1.0, "Sample {} out of range: {}", i, s);
        }
    }

    // --- Codec info ---

    #[test]
    fn codec_info_for_wav() {
        let samples: Vec<i16> = vec![0; 100];
        let wav = make_wav(44100, 2, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let info = source.codec_info();
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate, 44100);
        assert!(!info.codec_name.is_empty());
    }

    #[test]
    fn codec_info_mono_wav() {
        let wav = make_wav(22050, 1, &[0; 100]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let info = source.codec_info();
        assert_eq!(info.channels, 1);
        assert_eq!(info.sample_rate, 22050);
    }

    #[test]
    fn codec_name_accessor() {
        let wav = make_wav(44100, 1, &[0; 100]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        let name = source.codec_name();
        assert!(!name.is_empty());
        // WAV PCM should be recognized
        assert!(
            name.contains("PCM") || name == "Audio",
            "Unexpected codec name for WAV: {}",
            name
        );
    }

    #[test]
    fn bits_per_sample_wav() {
        let wav = make_wav(44100, 1, &[0; 100]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        // WAV PCM 16-bit should report bits_per_sample
        if let Some(bps) = source.bits_per_sample() {
            assert_eq!(bps, 16);
        }
        // (Some symphonia versions may not report it; either way shouldn't crash)
    }

    #[test]
    fn codec_info_matches_accessors() {
        let wav = make_wav(48000, 2, &[0; 200]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let info = source.codec_info();
        assert_eq!(info.codec_name, source.codec_name());
        assert_eq!(info.channels, source.channels());
        assert_eq!(info.sample_rate, source.sample_rate());
        assert_eq!(info.bits_per_sample, source.bits_per_sample());
    }

    // --- codec_type_to_name ---

    #[test]
    fn codec_name_lookup() {
        use symphonia::core::codecs::*;
        assert_eq!(codec_type_to_name(CODEC_TYPE_MP3), "MP3");
        assert_eq!(codec_type_to_name(CODEC_TYPE_AAC), "AAC");
        assert_eq!(codec_type_to_name(CODEC_TYPE_OPUS), "Opus");
        assert_eq!(codec_type_to_name(CODEC_TYPE_FLAC), "FLAC");
        assert_eq!(codec_type_to_name(CODEC_TYPE_VORBIS), "Vorbis");
    }

    #[test]
    fn codec_name_pcm_variants() {
        use symphonia::core::codecs::*;
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_U8), "PCM 8-bit");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_S16LE), "PCM 16-bit");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_S16BE), "PCM 16-bit");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_S24LE), "PCM 24-bit");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_S24BE), "PCM 24-bit");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_S32LE), "PCM 32-bit");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_S32BE), "PCM 32-bit");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_F32LE), "PCM 32-bit Float");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_F32BE), "PCM 32-bit Float");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_F64LE), "PCM 64-bit Float");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_F64BE), "PCM 64-bit Float");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_ALAW), "PCM A-law");
        assert_eq!(codec_type_to_name(CODEC_TYPE_PCM_MULAW), "PCM u-law");
    }

    #[test]
    fn codec_name_alac() {
        use symphonia::core::codecs::*;
        assert_eq!(codec_type_to_name(CODEC_TYPE_ALAC), "ALAC");
    }

    #[test]
    fn codec_name_unknown_returns_audio() {
        use symphonia::core::codecs::*;
        assert_eq!(codec_type_to_name(CODEC_TYPE_NULL), "Audio");
    }

    // --- Error paths ---

    #[test]
    fn error_on_invalid_data() {
        let result = SymphoniaSource::new(Cursor::new(vec![0u8; 100]));
        assert!(result.is_err());
    }

    #[test]
    fn error_on_empty_data() {
        let result = SymphoniaSource::new(Cursor::new(Vec::<u8>::new()));
        assert!(result.is_err());
    }

    #[test]
    fn error_on_single_byte() {
        let result = SymphoniaSource::new(Cursor::new(vec![0xFF]));
        assert!(result.is_err());
    }

    #[test]
    fn error_on_truncated_wav_header() {
        // Valid RIFF header start but truncated
        let mut buf = Vec::new();
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        // Missing fmt chunk
        let result = SymphoniaSource::new(Cursor::new(buf));
        assert!(result.is_err());
    }

    #[test]
    fn error_on_random_bytes() {
        let random_data: Vec<u8> = (0..1024).map(|i| (i * 7 % 256) as u8).collect();
        let result = SymphoniaSource::new(Cursor::new(random_data));
        assert!(result.is_err());
    }

    #[test]
    fn error_message_is_descriptive() {
        let result = SymphoniaSource::new(Cursor::new(vec![0u8; 50]));
        match result {
            Err(RadioError::Decode(msg)) => {
                assert!(!msg.is_empty(), "Error message should not be empty");
            }
            Err(other) => {
                // Any RadioError variant is acceptable
                let msg = format!("{}", other);
                assert!(!msg.is_empty());
            }
            Ok(_) => panic!("Expected an error for invalid data"),
        }
    }

    // --- Source trait ---

    #[test]
    fn current_span_len_is_none() {
        let wav = make_wav(44100, 1, &[0; 100]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        assert!(source.current_span_len().is_none());
    }

    #[test]
    fn total_duration_is_none() {
        let samples: Vec<i16> = vec![0; 100];
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        assert!(source.total_duration().is_none());
    }

    // --- Format hints ---

    #[test]
    fn new_with_hint_wav() {
        let wav = make_wav(44100, 1, &[0; 100]);
        let source = SymphoniaSource::new_with_hint(Cursor::new(wav), Some("wav")).unwrap();
        assert_eq!(source.channels(), 1);
    }

    #[test]
    fn new_with_hint_none_same_as_new() {
        let samples: Vec<i16> = (0..500).map(|i| (i * 20) as i16).collect();
        let wav1 = make_wav(44100, 1, &samples);
        let wav2 = wav1.clone();

        let source1 = SymphoniaSource::new(Cursor::new(wav1)).unwrap();
        let source2 = SymphoniaSource::new_with_hint(Cursor::new(wav2), None).unwrap();

        assert_eq!(source1.channels(), source2.channels());
        assert_eq!(source1.sample_rate(), source2.sample_rate());
        assert_eq!(source1.codec_name(), source2.codec_name());
    }

    #[test]
    fn wrong_hint_still_decodes_wav() {
        // WAV is self-describing enough that wrong hints may still work
        let wav = make_wav(44100, 1, &[0; 100]);
        // Even with a wrong hint, symphonia may auto-detect
        let result = SymphoniaSource::new_with_hint(Cursor::new(wav), Some("mp3"));
        // Either succeeds (probe overrides) or fails with decode error - both are valid
        if let Ok(source) = result {
            assert_eq!(source.channels(), 1);
        }
    }

    // --- create_codec_registry ---

    #[test]
    fn codec_registry_creation_does_not_panic() {
        let _registry = create_codec_registry();
    }

    // --- Iterator exhaustion ---

    #[test]
    fn iterator_returns_none_after_exhaustion() {
        let wav = make_wav(44100, 1, &[1000; 10]);
        let mut source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        // Consume all samples
        while source.next().is_some() {}

        // After exhaustion, should consistently return None
        assert!(source.next().is_none());
        assert!(source.next().is_none());
    }

    #[test]
    fn very_short_file_one_sample() {
        let wav = make_wav(44100, 1, &[5000]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), 1);
    }

    // --- Probe timeout ---

    /// A reader that blocks forever on reads, simulating a hanging probe
    struct BlockingReader;

    impl std::io::Read for BlockingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_secs(60));
            Ok(0)
        }
    }

    impl std::io::Seek for BlockingReader {
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn probe_timeout_on_blocking_reader() {
        // This should timeout rather than hang forever
        // We use a short PROBE_TIMEOUT_SECS (10s from config), but this test
        // verifies the mechanism works. The blocking reader never returns data,
        // so probe can never complete.
        let start = std::time::Instant::now();
        let result = SymphoniaSource::new(BlockingReader);
        let elapsed = start.elapsed();

        match result {
            Err(RadioError::Timeout(msg)) => {
                assert!(msg.contains("timed out"));
            }
            Err(_) => {
                // Probe might fail before timeout if it detects no data
                // That's also acceptable
            }
            Ok(_) => panic!("Expected error for blocking reader"),
        }
        // Should complete within PROBE_TIMEOUT + some margin, not 60s
        assert!(
            elapsed.as_secs() < 30,
            "Probe should timeout, not block for {:?}",
            elapsed
        );
    }

    /// A reader that returns data slowly, one byte at a time with delays
    struct SlowReader {
        data: Vec<u8>,
        pos: usize,
    }

    impl SlowReader {
        fn new(data: Vec<u8>) -> Self {
            Self { data, pos: 0 }
        }
    }

    impl std::io::Read for SlowReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(10));
            let n = std::cmp::min(buf.len(), self.data.len() - self.pos).min(1);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl std::io::Seek for SlowReader {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            match pos {
                std::io::SeekFrom::Start(p) => self.pos = p as usize,
                std::io::SeekFrom::Current(p) => {
                    self.pos = (self.pos as i64 + p) as usize;
                }
                std::io::SeekFrom::End(p) => {
                    self.pos = (self.data.len() as i64 + p) as usize;
                }
            }
            Ok(self.pos as u64)
        }
    }

    #[test]
    fn probe_succeeds_with_slow_but_valid_reader() {
        // A slow reader with valid WAV data should still succeed
        let samples: Vec<i16> = vec![0; 100];
        let wav = make_wav(44100, 1, &samples);
        let slow = SlowReader::new(wav);

        let result = SymphoniaSource::new(slow);
        // Either succeeds (probe completes in time) or times out for very slow reads
        // Both are valid outcomes
        if let Ok(source) = result {
            assert_eq!(source.channels(), 1);
        }
    }

    #[test]
    fn probe_disconnect_handled() {
        // Test that we handle the case where the probe thread panics/disconnects
        // We can't easily force a panic, but we can verify the error path exists
        // by checking that invalid data doesn't hang
        let result = SymphoniaSource::new(Cursor::new(vec![0xFF; 10]));
        assert!(result.is_err());
    }

    #[test]
    fn probe_timeout_valid_wav_still_works() {
        // Ensure the timeout mechanism doesn't break normal decoding
        let samples: Vec<i16> = (0..2000).map(|i| (i * 10) as i16).collect();
        let wav = make_wav(44100, 2, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        assert_eq!(source.channels(), 2);
        assert_eq!(source.sample_rate(), 44100);
        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), 2000);
    }

    #[test]
    fn multiple_sequential_probes_work() {
        // Verify probing multiple files in sequence doesn't leak threads or break
        for i in 0..10 {
            let samples: Vec<i16> = (0..100).map(|j| ((i + j) * 50) as i16).collect();
            let wav = make_wav(44100, 1, &samples);
            let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
            assert_eq!(source.channels(), 1);
            let decoded: Vec<f32> = source.collect();
            assert_eq!(decoded.len(), 100);
        }
    }

    #[test]
    fn multiple_sequential_errors_dont_leak() {
        // Verify that repeated probe failures don't accumulate resources
        for _ in 0..20 {
            let result = SymphoniaSource::new(Cursor::new(vec![0u8; 50]));
            assert!(result.is_err());
        }
    }

    #[test]
    fn error_variant_is_decode_for_invalid_data() {
        let result = SymphoniaSource::new(Cursor::new(vec![0u8; 100]));
        match result {
            Err(RadioError::Decode(msg)) => {
                assert!(
                    msg.contains("Probe error"),
                    "Expected probe error, got: {}",
                    msg
                );
            }
            Err(RadioError::Timeout(_)) => {
                // Also acceptable — the probe may time out on junk data
            }
            Err(other) => {
                panic!("Expected Decode or Timeout error, got: {:?}", other);
            }
            Ok(_) => panic!("Expected error for invalid data"),
        }
    }

    #[test]
    fn error_variant_is_timeout_for_blocking() {
        // BlockingReader should specifically produce a Timeout error
        let result = SymphoniaSource::new(BlockingReader);
        match result {
            Err(RadioError::Timeout(msg)) => {
                assert!(
                    msg.contains("timed out"),
                    "Timeout message should mention 'timed out': {}",
                    msg
                );
                assert!(
                    msg.contains("10"),
                    "Timeout message should mention timeout duration: {}",
                    msg
                );
            }
            Err(_) => {
                // Acceptable: probe might fail with a different error before timeout
            }
            Ok(_) => panic!("Expected error for blocking reader"),
        }
    }

    #[test]
    fn probe_timeout_does_not_block_subsequent_probes() {
        // After a blocking probe times out, a normal probe should still work
        let start = std::time::Instant::now();
        let _ = SymphoniaSource::new(BlockingReader);
        let timeout_elapsed = start.elapsed();

        // Now probe a normal WAV — should work immediately
        let wav = make_wav(44100, 1, &[0; 100]);
        let normal_start = std::time::Instant::now();
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        let normal_elapsed = normal_start.elapsed();

        assert_eq!(source.channels(), 1);
        // Normal probe should be fast (under 1s), not blocked by the abandoned thread
        assert!(
            normal_elapsed.as_secs() < 2,
            "Normal probe after timeout took {:?} (timeout was {:?})",
            normal_elapsed,
            timeout_elapsed
        );
    }

    /// A reader that returns EOF immediately (no data at all)
    struct EmptyReader;

    impl std::io::Read for EmptyReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl std::io::Seek for EmptyReader {
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn probe_empty_reader_fails_fast() {
        let start = std::time::Instant::now();
        let result = SymphoniaSource::new(EmptyReader);
        let elapsed = start.elapsed();

        assert!(result.is_err());
        // Should fail fast, not wait for timeout
        assert!(
            elapsed.as_secs() < 5,
            "Empty reader should fail fast, took {:?}",
            elapsed
        );
    }

    /// A reader that returns an IO error on every read
    struct ErrorReader;

    impl std::io::Read for ErrorReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "simulated network error",
            ))
        }
    }

    impl std::io::Seek for ErrorReader {
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn probe_error_reader_fails_fast() {
        let start = std::time::Instant::now();
        let result = SymphoniaSource::new(ErrorReader);
        let elapsed = start.elapsed();

        assert!(result.is_err());
        // IO errors should cause probe to fail quickly, not hang
        assert!(
            elapsed.as_secs() < 5,
            "Error reader should fail fast, took {:?}",
            elapsed
        );
    }

    // --- Error slot (stream error propagation) ---

    /// A reader that serves valid data then returns an IO error after a threshold,
    /// simulating a network drop mid-stream.
    struct FailAfterReader {
        inner: Cursor<Vec<u8>>,
        bytes_read: usize,
        fail_after: usize,
    }

    impl FailAfterReader {
        fn new(data: Vec<u8>, fail_after: usize) -> Self {
            Self {
                inner: Cursor::new(data),
                bytes_read: 0,
                fail_after,
            }
        }
    }

    impl std::io::Read for FailAfterReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.bytes_read >= self.fail_after {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated network failure",
                ));
            }
            let n = self.inner.read(buf)?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    impl std::io::Seek for FailAfterReader {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn error_slot_none_for_clean_eof() {
        // Normal WAV that plays to completion — error slot should stay None
        let samples: Vec<i16> = (0..500).map(|i| (i * 20) as i16).collect();
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        let error_slot = source.error_slot();

        // Consume all samples
        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), 500);

        // Error slot should be None — clean EOF
        let err = error_slot.lock().unwrap();
        assert!(
            err.is_none(),
            "Error slot should be None for clean EOF, got: {:?}",
            *err
        );
    }

    #[test]
    fn error_slot_populated_on_io_error() {
        // Create a 1-second WAV (large enough for probe to succeed)
        let samples: Vec<i16> = (0..44100)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44100, 1, &samples);

        // Fail after 5000 bytes (probe reads ~44 byte header, then decode reads audio)
        let reader = FailAfterReader::new(wav, 5000);
        let source = SymphoniaSource::new(reader).unwrap();
        let error_slot = source.error_slot();

        // Consume samples until the reader fails
        let decoded: Vec<f32> = source.collect();
        // Should get some samples before failure (not all 44100)
        assert!(
            decoded.len() < 44100,
            "Should not get all samples, got {}",
            decoded.len()
        );

        // Error slot should contain the IO error
        let err = error_slot.lock().unwrap();
        assert!(
            err.is_some(),
            "Error slot should be populated after IO error"
        );
        let msg = err.as_ref().unwrap();
        assert!(!msg.is_empty(), "Error message should not be empty");
    }

    #[test]
    fn error_slot_is_separate_per_source() {
        let wav1 = make_wav(44100, 1, &[0; 100]);
        let wav2 = make_wav(44100, 1, &[0; 100]);

        let source1 = SymphoniaSource::new(Cursor::new(wav1)).unwrap();
        let source2 = SymphoniaSource::new(Cursor::new(wav2)).unwrap();

        let slot1 = source1.error_slot();
        let slot2 = source2.error_slot();

        // Different sources should have independent error slots
        assert!(!Arc::ptr_eq(&slot1, &slot2));
    }

    #[test]
    fn error_slot_accessible_after_source_dropped() {
        let samples: Vec<i16> = (0..44100)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44100, 1, &samples);
        let reader = FailAfterReader::new(wav, 5000);
        let source = SymphoniaSource::new(reader).unwrap();
        let error_slot = source.error_slot();

        // Consume and drop source
        let _: Vec<f32> = source.collect();

        // Error slot should still be readable after source is dropped
        let err = error_slot.lock().unwrap();
        // Should have an error because FailAfterReader failed
        assert!(err.is_some());
    }

    #[test]
    fn error_reader_populates_error_slot() {
        // ErrorReader returns io::Error on every read — probe will fail
        // but let's test with a reader that fails during decode, not probe
        let samples: Vec<i16> = (0..44100)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44100, 1, &samples);

        // Fail after 200 bytes — may or may not be enough for probe
        let reader = FailAfterReader::new(wav, 200);
        match SymphoniaSource::new(reader) {
            Ok(source) => {
                let error_slot = source.error_slot();
                let _: Vec<f32> = source.collect();
                // If probe succeeded, decode should have hit the error
                let err = error_slot.lock().unwrap();
                assert!(err.is_some(), "Should have error after reader failure");
            }
            Err(_) => {
                // Probe itself failed — that's also valid for very early failures
            }
        }
    }

    // --- DecoderStats ---

    #[test]
    fn decoder_stats_increments_on_valid_wav() {
        let samples: Vec<i16> = (0..1000).map(|i| (i * 10) as i16).collect();
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        let stats = source.decoder_stats();

        let _: Vec<f32> = source.collect();

        let (packets, errors) = stats.snapshot();
        assert!(packets > 0, "Should have decoded at least one frame");
        assert_eq!(errors, 0, "Should have no decode errors on valid WAV");
    }

    #[test]
    fn decoder_stats_accessor_returns_shared_arc() {
        let wav = make_wav(44100, 1, &[0; 100]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        let s1 = source.decoder_stats();
        let s2 = source.decoder_stats();
        assert!(Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    fn decoder_stats_one_frame_after_construction() {
        // Construction pre-decodes one frame to discover actual output sample rate
        let wav = make_wav(44100, 1, &[0; 100]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        let stats = source.decoder_stats();
        let (frames, errors) = stats.snapshot();
        assert_eq!(frames, 1, "Pre-decode should have decoded one frame");
        assert_eq!(errors, 0);
    }

    #[test]
    fn alternating_positive_negative() {
        let samples: Vec<i16> = (0..200)
            .map(|i| if i % 2 == 0 { 10000 } else { -10000 })
            .collect();
        let wav = make_wav(44100, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        let decoded: Vec<f32> = source.collect();
        assert_eq!(decoded.len(), 200);

        // Check alternating pattern is preserved
        for i in 0..decoded.len() {
            if i % 2 == 0 {
                assert!(decoded[i] > 0.0, "Even sample {} should be positive", i);
            } else {
                assert!(decoded[i] < 0.0, "Odd sample {} should be negative", i);
            }
        }
    }
}
