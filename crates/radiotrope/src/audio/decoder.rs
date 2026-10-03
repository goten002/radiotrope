//! Audio decoder using Symphonia
//!
//! Provides `SymphoniaSource` which decodes audio streams into f32 samples,
//! supporting Opus (via libopus), MP3, AAC, FLAC, Vorbis, and more.

use std::io::{Read, Seek};
use std::num::NonZero;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use rodio::Source;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions,
};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{BitReaderLtr, MediaSourceStream, ReadBitsLtr, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia_adapter_fdk_aac::AacDecoder as LibAacDecoder;
use symphonia_adapter_libopus::OpusDecoder as LibOpusDecoder;

use crate::config::timeouts::PROBE_TIMEOUT_SECS;
use crate::error::RadioError;

use super::stats::DecoderStats;
use super::types::CodecInfo;

/// The format reader produced by a successful probe
pub type ProbedFormat = Box<dyn FormatReader>;

/// Unreadable stretches the demuxer may report in a row before the stream
/// counts as broken. A live stream can carry garbage: an ICY reconnect
/// splices two streams mid-frame, and a false sync word in the join then
/// fails to parse. The demuxer resyncs on the next call.
const MAX_DEMUX_ERRORS: u32 = 100;

/// Convert a symphonia codec ID to a human-readable name
pub fn codec_type_to_name(codec: AudioCodecId) -> String {
    use symphonia::core::codecs::audio::well_known::*;
    match codec {
        CODEC_ID_AAC => "AAC".to_string(),
        CODEC_ID_FLAC => "FLAC".to_string(),
        CODEC_ID_MP3 => "MP3".to_string(),
        CODEC_ID_OPUS => "Opus".to_string(),
        CODEC_ID_VORBIS => "Vorbis".to_string(),
        CODEC_ID_PCM_U8 => "PCM 8-bit".to_string(),
        CODEC_ID_PCM_S16LE | CODEC_ID_PCM_S16BE => "PCM 16-bit".to_string(),
        CODEC_ID_PCM_S24LE | CODEC_ID_PCM_S24BE => "PCM 24-bit".to_string(),
        CODEC_ID_PCM_S32LE | CODEC_ID_PCM_S32BE => "PCM 32-bit".to_string(),
        CODEC_ID_PCM_F32LE | CODEC_ID_PCM_F32BE => "PCM 32-bit Float".to_string(),
        CODEC_ID_PCM_F64LE | CODEC_ID_PCM_F64BE => "PCM 64-bit Float".to_string(),
        CODEC_ID_PCM_ALAW => "PCM A-law".to_string(),
        CODEC_ID_PCM_MULAW => "PCM u-law".to_string(),
        CODEC_ID_ALAC => "ALAC".to_string(),
        _ => "Audio".to_string(),
    }
}

/// Audio object types that name SBR outright: HE-AAC and HE-AAC v2
const AOT_SBR: u32 = 5;
const AOT_PS: u32 = 29;
/// USAC (xHE-AAC). It has an SBR of its own, but it is not HE-AAC: it
/// shows as plain AAC (only "AAC" and "AAC+" are shown)
const AOT_USAC: u32 = 42;

/// Sampling rates by `samplingFrequencyIndex` (ISO/IEC 14496-3, 1.6.3.3)
const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Whether an AAC stream is AAC+ (HE-AAC v1 or v2), given the rate its
/// first frame decoded at.
///
/// AAC+ codes the audio at half the rate, and SBR rebuilds the top half,
/// so FDK puts out twice the rate the stream declares. ADTS (ICY, HLS TS)
/// declares that core rate in its header; MP4 gives it in the
/// AudioSpecificConfig, or names SBR there outright. The rare single-rate
/// SBR, which keeps the rate, reads as plain AAC, and so does USAC.
fn is_aac_plus(params: &AudioCodecParameters, decoded_rate: u32) -> bool {
    let core_rate = match params.extra_data.as_deref().and_then(audio_specific_config) {
        Some((AOT_SBR | AOT_PS, _)) => return true,
        Some((AOT_USAC, _)) => return false,
        Some((_, rate)) => Some(rate),
        None => params.sample_rate,
    };
    core_rate.is_some_and(|core| decoded_rate > core)
}

/// The object type and sampling rate an AudioSpecificConfig starts with
fn audio_specific_config(config: &[u8]) -> Option<(u32, u32)> {
    let mut bits = BitReaderLtr::new(config);
    let object_type = match bits.read_bits_leq32(5).ok()? {
        31 => 32 + bits.read_bits_leq32(6).ok()?,
        object_type => object_type,
    };
    let rate = match bits.read_bits_leq32(4).ok()? {
        15 => bits.read_bits_leq32(24).ok()?,
        index => *AAC_SAMPLE_RATES.get(index as usize)?,
    };
    Some((object_type, rate))
}

/// Create a codec registry with Opus support via libopus
pub fn create_codec_registry() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    // Register built-in codecs first (includes symphonia's AAC)
    symphonia::default::register_enabled_codecs(&mut registry);
    // Override AAC with FDK AAC (supports HE-AAC v1/v2 with SBR)
    registry.register_audio_decoder::<LibAacDecoder>();
    registry.register_audio_decoder::<LibOpusDecoder>();
    registry
}

/// Spawn a probe thread and return the receiver immediately (non-blocking).
///
/// The probe runs on a background `"symphonia-probe"` thread. The caller can
/// poll the returned `Receiver` with `try_recv()` or block with `recv_timeout()`.
pub fn start_probe<R: Read + Seek + Send + Sync + 'static>(
    reader: R,
    format_hint: Option<String>,
) -> Result<Receiver<Result<ProbedFormat, RadioError>>, RadioError> {
    spawn_probe(reader, format_hint, Ok)
}

/// Like [`start_probe`], but also builds the [`SymphoniaSource`] on the probe
/// thread.
///
/// Building the source decodes the first packet, which reads from the
/// stream and can block for as long as the station takes to send it. Doing
/// that here keeps the caller (the engine's command loop) responsive.
pub fn start_open<R: Read + Seek + Send + Sync + 'static>(
    reader: R,
    format_hint: Option<String>,
) -> Result<Receiver<Result<SymphoniaSource, RadioError>>, RadioError> {
    spawn_probe(reader, format_hint, SymphoniaSource::from_probed)
}

fn spawn_probe<R, T, F>(
    reader: R,
    format_hint: Option<String>,
    finish: F,
) -> Result<Receiver<Result<T, RadioError>>, RadioError>
where
    R: Read + Seek + Send + Sync + 'static,
    T: Send + 'static,
    F: FnOnce(ProbedFormat) -> Result<T, RadioError> + Send + 'static,
{
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
            let result = probe
                .probe(&hint, mss, format_opts, metadata_opts)
                .map_err(|e| RadioError::Decode(format!("Probe error: {}", e)))
                .and_then(finish);
            let _ = tx.send(result);
        })
        .map_err(|e| RadioError::Audio(format!("Failed to spawn probe thread: {}", e)))?;

    Ok(rx)
}

/// Find the first audio track of `format` and make a decoder for it
fn open_audio_track(
    format: &dyn FormatReader,
) -> Result<(u32, AudioCodecParameters, Box<dyn AudioDecoder>), RadioError> {
    let (track_id, codec_params) = format
        .tracks()
        .iter()
        .find_map(|t| match &t.codec_params {
            Some(CodecParameters::Audio(params))
                if params.codec != symphonia::core::codecs::audio::CODEC_ID_NULL_AUDIO =>
            {
                Some((t.id, params.clone()))
            }
            _ => None,
        })
        .ok_or_else(|| RadioError::Decode("No audio track found".to_string()))?;

    let codec_name = codec_type_to_name(codec_params.codec);
    let decoder = create_codec_registry()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .map_err(|_| RadioError::Decode(format!("Unsupported codec: {codec_name}")))?;
    Ok((track_id, codec_params, decoder))
}

/// A symphonia-based audio source that supports Opus and other formats
pub struct SymphoniaSource {
    decoder: Box<dyn AudioDecoder>,
    format: ProbedFormat,
    track_id: u32,
    sample_buf: Option<Vec<f32>>,
    sample_idx: usize,
    channels: u16,
    sample_rate: u32,
    codec_name: String,
    /// The track being decoded, for its codec name
    codec_params: AudioCodecParameters,
    /// The output rate `codec_name` was worked out for (0: not yet). AAC
    /// and AAC+ only tell apart by the rate they decode at, which can
    /// change in mid-stream (an HLS stream moving between them).
    named_at_rate: u32,
    /// `codec_name`, shared with whoever shows it ([`Self::codec_label`])
    label: Arc<Mutex<String>>,
    bits_per_sample: Option<u32>,
    /// Stores the last non-EOF error for the engine to check after stream ends
    last_error: Arc<Mutex<Option<String>>>,
    /// Atomic decode counters (frames decoded / decode errors)
    decoder_stats: Arc<DecoderStats>,
    /// Demuxer errors since the last packet that decoded
    demux_errors: u32,
    /// Microseconds of audio decoded, for the stream buffer's byte rate
    decoded_time: Option<Arc<AtomicU64>>,
}

impl SymphoniaSource {
    /// Create a new source from a reader, auto-detecting the format
    pub fn new<R: Read + Seek + Send + Sync + 'static>(reader: R) -> Result<Self, RadioError> {
        Self::new_with_hint(reader, None)
    }

    /// Create a new source with an optional format hint (e.g., "aac", "mp4")
    ///
    /// Blocks for up to `PROBE_TIMEOUT_SECS` while probing the format and
    /// decoding the first packet, which happen on a thread of their own.
    /// After a timeout that thread is left behind, still holding `reader`:
    /// it ends once a read of `reader` returns, so a reader that can block
    /// for good (a stalled network stream) keeps it waiting for good. Give
    /// such a reader a way to end, as the engine does with a cancellable
    /// stream and [`start_open`].
    pub fn new_with_hint<R: Read + Seek + Send + Sync + 'static>(
        reader: R,
        format_hint: Option<&str>,
    ) -> Result<Self, RadioError> {
        let rx = start_open(reader, format_hint.map(|s| s.to_string()))?;

        match rx.recv_timeout(Duration::from_secs(PROBE_TIMEOUT_SECS)) {
            Ok(opened) => opened,
            Err(RecvTimeoutError::Timeout) => Err(RadioError::Timeout(format!(
                "Format probe timed out after {}s",
                PROBE_TIMEOUT_SECS
            ))),
            Err(RecvTimeoutError::Disconnected) => {
                Err(RadioError::Decode("Probe thread panicked".to_string()))
            }
        }
    }

    /// Create a `SymphoniaSource` from a completed probe.
    ///
    /// Decodes the first packet, so it reads from the stream and may block
    /// until the station sends data. [`start_open`] runs it off-thread.
    pub fn from_probed(format: ProbedFormat) -> Result<Self, RadioError> {
        let (track_id, codec_params, decoder) = open_audio_track(format.as_ref())?;

        let codec_name = codec_type_to_name(codec_params.codec);
        let channels = codec_params
            .channels
            .as_ref()
            .map(|c| c.count() as u16)
            .unwrap_or(2);
        let sample_rate = codec_params.sample_rate.unwrap_or(44100);
        let bits_per_sample = codec_params.bits_per_sample;
        if channels == 0 || sample_rate == 0 {
            return Err(RadioError::Decode(format!(
                "Invalid audio format: {sample_rate} Hz, {channels} channels"
            )));
        }

        let mut source = Self {
            decoder,
            format,
            track_id,
            sample_buf: None,
            sample_idx: 0,
            channels,
            sample_rate,
            label: Arc::new(Mutex::new(codec_name.clone())),
            codec_name,
            codec_params,
            named_at_rate: 0,
            bits_per_sample,
            last_error: Arc::new(Mutex::new(None)),
            decoder_stats: Arc::new(DecoderStats::new()),
            demux_errors: 0,
            decoded_time: None,
        };

        // Pre-decode the first frame to discover the actual output sample rate.
        // This is critical for HE-AAC where FDK AAC applies SBR, doubling the
        // sample rate (e.g., 24kHz→48kHz). Without this, rodio would configure
        // its resampler using the core rate from the ADTS header before any
        // frames are decoded, causing low-pitch playback. The same doubled
        // rate tells AAC+ from plain AAC (`name_codec`).
        source.decode_next_packet();

        Ok(source)
    }

    /// Name the codec for the rate the last packet decoded at: AAC+ is
    /// AAC that decodes at twice the rate its track declares. Done again
    /// whenever that rate changes, and after a new track.
    fn name_codec(&mut self) {
        if self.named_at_rate == self.sample_rate {
            return;
        }
        self.named_at_rate = self.sample_rate;
        let mut name = codec_type_to_name(self.codec_params.codec);
        if self.codec_params.codec == CODEC_ID_AAC
            && is_aac_plus(&self.codec_params, self.sample_rate)
        {
            name = "AAC+".to_string();
        }
        if name != self.codec_name {
            self.codec_name = name;
            *self.label.lock().unwrap_or_else(|e| e.into_inner()) = self.codec_name.clone();
        }
    }

    /// Get the codec name (e.g., "MP3", "Opus", "AAC")
    pub fn codec_name(&self) -> &str {
        &self.codec_name
    }

    /// The codec name, kept up to date while the source is decoded on
    /// another thread: a chained Ogg stream can change codec, and an AAC
    /// stream can move between AAC and AAC+
    pub fn codec_label(&self) -> Arc<Mutex<String>> {
        self.label.clone()
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

    /// Add the length of each decoded packet to `micros`, from here on
    /// (see `StreamBufferReader::decoded_time`)
    pub fn count_decoded_time(&mut self, micros: Arc<AtomicU64>) {
        self.decoded_time = Some(micros);
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

    /// Record why the stream ended, for the engine to report
    fn fail(&self, reason: String) -> bool {
        if let Ok(mut err) = self.last_error.lock() {
            *err = Some(reason);
        }
        false
    }

    /// Switch to the stream's new track after `ResetRequired`: a chained
    /// Ogg stream (Icecast Vorbis/Opus stations often start a new one for
    /// every song) has a new serial number and codec setup.
    fn reset_track(&mut self) -> Result<(), RadioError> {
        let (track_id, codec_params, decoder) = open_audio_track(self.format.as_ref())?;
        self.track_id = track_id;
        self.decoder = decoder;
        self.bits_per_sample = codec_params.bits_per_sample;
        self.codec_params = codec_params;
        // Named again once its first packet decodes: an AAC track's name
        // depends on the rate it decodes at
        self.named_at_rate = 0;
        Ok(())
    }

    fn decode_next_packet(&mut self) -> bool {
        use symphonia::core::errors::Error;

        loop {
            match self.format.next_packet() {
                Ok(None) => {
                    // Clean EOF — stream ended naturally, no error stored
                    return false;
                }
                Ok(Some(packet)) => {
                    if packet.track_id != self.track_id {
                        continue;
                    }

                    match self.decoder.decode(&packet) {
                        Ok(decoded) => {
                            let spec = decoded.spec();
                            let rate = spec.rate();
                            let channels = spec.channels().count() as u16;
                            if rate == 0 || channels == 0 {
                                return self.fail(format!(
                                    "Invalid audio format: {rate} Hz, {channels} channels"
                                ));
                            }
                            self.decoder_stats.record_frame();
                            self.demux_errors = 0;
                            if let Some(time) = &self.decoded_time {
                                let micros = decoded.frames() as u64 * 1_000_000 / rate as u64;
                                time.fetch_add(micros, Ordering::Relaxed);
                            }

                            // Update sample rate and channels from decoder output —
                            // FDK AAC may change these after SBR/PS processing
                            self.sample_rate = rate;
                            self.channels = channels;

                            let buf = self.sample_buf.get_or_insert_with(Vec::new);
                            decoded.copy_to_vec_interleaved(buf);
                            self.sample_idx = 0;
                            self.name_codec();
                            return true;
                        }
                        Err(Error::DecodeError(_)) => {
                            self.decoder_stats.record_error();
                            continue;
                        }
                        Err(Error::ResetRequired) => {
                            self.decoder.reset();
                            continue;
                        }
                        Err(e) => return self.fail(format!("{}", e)),
                    }
                }
                Err(Error::ResetRequired) => {
                    if let Err(e) = self.reset_track() {
                        return self.fail(e.to_string());
                    }
                }
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Clean EOF — stream ended naturally, no error stored
                    return false;
                }
                // Garbage in the stream: skip it, unless nothing but garbage
                // comes. A false sync word can also parse as a header the
                // demuxer doesn't support (an ADTS header for several AAC
                // frames): that is garbage too, not the station's format.
                Err(Error::DecodeError(msg)) | Err(Error::Unsupported(msg)) => {
                    self.decoder_stats.record_error();
                    self.demux_errors += 1;
                    if self.demux_errors >= MAX_DEMUX_ERRORS {
                        return self.fail(format!(
                            "{msg} ({} unreadable frames in a row)",
                            self.demux_errors
                        ));
                    }
                }
                Err(e) => {
                    // IO error or other — likely network failure
                    return self.fail(format!("{}", e));
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
                if self.sample_idx < buf.len() {
                    let sample = buf[self.sample_idx];
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
        // Return the remaining samples in the current decoded packet.
        // This creates span boundaries between packets, allowing rodio to
        // re-query sample_rate()/channels() and reconfigure its resampler.
        // Critical for codecs like HE-AAC where FDK AAC may change the
        // output sample rate after SBR processing kicks in.
        self.sample_buf
            .as_ref()
            .map(|buf| buf.len().saturating_sub(self.sample_idx))
    }

    fn channels(&self) -> NonZero<u16> {
        NonZero::new(self.channels).expect("channels must be non-zero")
    }

    fn sample_rate(&self) -> NonZero<u32> {
        NonZero::new(self.sample_rate).expect("sample_rate must be non-zero")
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

        assert_eq!(source.channels().get(), 1);
        assert_eq!(source.sample_rate().get(), 44100);
    }

    #[test]
    fn decode_wav_stereo() {
        let samples: Vec<i16> = (0..2000).map(|i| (i % 200 * 50) as i16).collect();
        let wav = make_wav(48000, 2, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();

        assert_eq!(source.channels().get(), 2);
        assert_eq!(source.sample_rate().get(), 48000);
    }

    #[test]
    fn decode_wav_8khz() {
        let samples: Vec<i16> = (0..400).map(|i| (i * 50) as i16).collect();
        let wav = make_wav(8000, 1, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        assert_eq!(source.sample_rate().get(), 8000);
        assert_eq!(source.channels().get(), 1);
    }

    #[test]
    fn decode_wav_96khz() {
        let samples: Vec<i16> = (0..1000).map(|i| (i * 10) as i16).collect();
        let wav = make_wav(96000, 2, &samples);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        assert_eq!(source.sample_rate().get(), 96000);
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
            assert!(
                (-1.0..=1.0).contains(&s),
                "Sample {} out of range: {}",
                i,
                s
            );
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
        assert_eq!(info.channels, source.channels().get());
        assert_eq!(info.sample_rate, source.sample_rate().get());
        assert_eq!(info.bits_per_sample, source.bits_per_sample());
    }

    // --- codec_type_to_name ---

    #[test]
    fn codec_name_lookup() {
        use symphonia::core::codecs::audio::well_known::*;
        assert_eq!(codec_type_to_name(CODEC_ID_MP3), "MP3");
        assert_eq!(codec_type_to_name(CODEC_ID_AAC), "AAC");
        assert_eq!(codec_type_to_name(CODEC_ID_OPUS), "Opus");
        assert_eq!(codec_type_to_name(CODEC_ID_FLAC), "FLAC");
        assert_eq!(codec_type_to_name(CODEC_ID_VORBIS), "Vorbis");
    }

    #[test]
    fn codec_name_pcm_variants() {
        use symphonia::core::codecs::audio::well_known::*;
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_U8), "PCM 8-bit");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_S16LE), "PCM 16-bit");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_S16BE), "PCM 16-bit");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_S24LE), "PCM 24-bit");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_S24BE), "PCM 24-bit");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_S32LE), "PCM 32-bit");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_S32BE), "PCM 32-bit");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_F32LE), "PCM 32-bit Float");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_F32BE), "PCM 32-bit Float");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_F64LE), "PCM 64-bit Float");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_F64BE), "PCM 64-bit Float");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_ALAW), "PCM A-law");
        assert_eq!(codec_type_to_name(CODEC_ID_PCM_MULAW), "PCM u-law");
    }

    #[test]
    fn codec_name_alac() {
        use symphonia::core::codecs::audio::well_known::*;
        assert_eq!(codec_type_to_name(CODEC_ID_ALAC), "ALAC");
    }

    #[test]
    fn codec_name_unknown_returns_audio() {
        use symphonia::core::codecs::audio::CODEC_ID_NULL_AUDIO;
        assert_eq!(codec_type_to_name(CODEC_ID_NULL_AUDIO), "Audio");
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
    fn current_span_len_returns_remaining_samples() {
        let wav = make_wav(44100, 1, &[0; 100]);
        let source = SymphoniaSource::new(Cursor::new(wav)).unwrap();
        // After pre-decode, span length is the remaining samples in the decoded buffer
        let span = source.current_span_len();
        assert!(span.is_some(), "Should have a span after pre-decode");
        assert!(span.unwrap() > 0, "Span should be non-empty");
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
        assert_eq!(source.channels().get(), 1);
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
            assert_eq!(source.channels().get(), 1);
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
            assert_eq!(source.channels().get(), 1);
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

        assert_eq!(source.channels().get(), 2);
        assert_eq!(source.sample_rate().get(), 44100);
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
            assert_eq!(source.channels().get(), 1);
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

        assert_eq!(source.channels().get(), 1);
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
        for (i, &sample) in decoded.iter().enumerate() {
            if i % 2 == 0 {
                assert!(sample > 0.0, "Even sample {} should be positive", i);
            } else {
                assert!(sample < 0.0, "Odd sample {} should be negative", i);
            }
        }
    }

    // --- Stream changes mid-playback ---

    /// `secs` of a 440 Hz stereo tone at 48 kHz, as Ogg Opus
    fn ogg_opus_tone(secs: f32) -> Vec<u8> {
        let frames = (48_000.0 * secs) as usize;
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3;
                [v, v]
            })
            .collect();
        crate::audio::recording::encode_ogg_opus(48_000, 2, &samples)
    }

    #[test]
    fn chained_ogg_plays_through_the_song_change() {
        // Icecast Ogg stations start a new logical stream (new serial,
        // new headers) at each song; symphonia reports ResetRequired there
        // (each encode picks its own serial number)
        let chained = [ogg_opus_tone(0.5), ogg_opus_tone(0.5)].concat();

        let source = SymphoniaSource::new_with_hint(Cursor::new(chained), Some("ogg")).unwrap();
        let error_slot = source.error_slot();
        let samples = source.count();
        let secs = samples as f32 / 2.0 / 48_000.0;
        assert!(
            error_slot.lock().unwrap().is_none(),
            "stream ended with an error: {:?}",
            error_slot.lock().unwrap()
        );
        assert!(
            secs > 0.9,
            "only {secs:.2} s decoded: stopped at the song change"
        );
    }

    #[test]
    fn zero_sample_rate_is_an_error_not_a_panic() {
        // rodio needs a non-zero rate and channel count; the equalizer
        // panics on zero when the engine builds the chain
        let wav = make_wav(0, 1, &[0i16; 1000]);
        let result = SymphoniaSource::new(Cursor::new(wav));
        assert!(result.is_err());
    }

    // --- AAC and AAC+ ---

    mod aac_plus {
        use super::*;
        use fdk_aac::enc::{
            AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport,
        };
        use symphonia::core::codecs::audio::AudioCodecParameters;

        /// Stereo test audio: tones across the band, different left and right
        fn music(rate: u32, secs: f32) -> Vec<i16> {
            (0..(rate as f32 * secs) as usize)
                .flat_map(|i| {
                    let t = i as f32 / rate as f32;
                    let tone = |f: f32| (2.0 * std::f32::consts::PI * f * t).sin();
                    let band = (tone(220.0) + tone(1760.0) + tone(5000.0) + tone(9000.0)) / 6.0;
                    [band + tone(330.0) / 5.0, band + tone(660.0) / 5.0]
                })
                .map(|s| (s * 20000.0) as i16)
                .collect()
        }

        /// Test audio encoded by FDK as ADTS, the way ICY stations and HLS
        /// TS segments send AAC. Like them, it signals SBR and PS only
        /// inside the frames: every ADTS header says AAC-LC.
        fn adts(object_type: AudioObjectType, bit_rate: u32, rate: u32, stereo: bool) -> Vec<u8> {
            let encoder = Encoder::new(EncoderParams {
                bit_rate: BitRate::Cbr(bit_rate),
                sample_rate: rate,
                transport: Transport::Adts,
                channels: if stereo {
                    ChannelMode::Stereo
                } else {
                    ChannelMode::Mono
                },
                audio_object_type: object_type,
            })
            .unwrap();
            let mut pcm = music(rate, 1.5);
            if !stereo {
                pcm = pcm.into_iter().step_by(2).collect();
            }
            let mut stream = Vec::new();
            let mut out = [0u8; 8192];
            let mut at = 0;
            while at < pcm.len() {
                let end = (at + 4096).min(pcm.len());
                let encoded = encoder.encode(&pcm[at..end], &mut out).unwrap();
                stream.extend_from_slice(&out[..encoded.output_size]);
                at += encoded.input_consumed.max(1);
            }
            stream
        }

        /// `stream` from its `n`-th ADTS frame on, as when tuning in to a
        /// station that is already playing
        fn from_frame(stream: &[u8], n: usize) -> Vec<u8> {
            let mut at = 0;
            for _ in 0..n {
                let header = &stream[at..at + 7];
                at += ((header[3] as usize & 0x03) << 11)
                    | ((header[4] as usize) << 3)
                    | (header[5] as usize >> 5);
            }
            stream[at..].to_vec()
        }

        fn open(stream: Vec<u8>) -> CodecInfo {
            SymphoniaSource::new_with_hint(Cursor::new(stream), Some("aac"))
                .unwrap()
                .codec_info()
        }

        #[test]
        fn a_reconnect_into_the_middle_of_a_frame_keeps_playing() {
            let stream = adts(AudioObjectType::Mpeg4LowComplexity, 128_000, 44_100, true);
            let whole = SymphoniaSource::new_with_hint(Cursor::new(stream.clone()), Some("aac"))
                .unwrap()
                .count();
            // The connection dropped, and the reconnect lands in the middle
            // of a frame, on bytes that look like an ADTS header for two AAC
            // frames (which the demuxer doesn't read)
            let cut = from_frame(&stream, 10).len();
            let mut spliced = stream[..stream.len() - cut].to_vec();
            spliced.extend_from_slice(&[0xFF, 0xF1, 0x50, 0x80, 0x10, 0x00, 0x01, 0x23, 0x45]);
            spliced.extend_from_slice(&from_frame(&stream, 20));

            let source = SymphoniaSource::new_with_hint(Cursor::new(spliced), Some("aac")).unwrap();
            let slot = source.error_slot();
            let played = source.count();
            assert_eq!(*slot.lock().unwrap(), None, "the stream ended on an error");
            assert!(played > whole / 2, "played {played} of {whole}");
        }

        #[test]
        fn plain_aac_is_called_aac() {
            let info = open(adts(
                AudioObjectType::Mpeg4LowComplexity,
                128_000,
                44_100,
                true,
            ));
            assert_eq!(info.codec_name, "AAC");
            assert_eq!((info.sample_rate, info.channels), (44_100, 2));
        }

        #[test]
        fn plain_aac_at_a_low_rate_is_still_aac() {
            // A low rate alone doesn't make AAC+: the decoder must double it
            let info = open(adts(
                AudioObjectType::Mpeg4LowComplexity,
                32_000,
                22_050,
                false,
            ));
            assert_eq!(info.codec_name, "AAC");
            assert_eq!((info.sample_rate, info.channels), (22_050, 1));
        }

        #[test]
        fn he_aac_is_called_aac_plus() {
            let info = open(adts(AudioObjectType::Mpeg4HeAac, 64_000, 44_100, true));
            assert_eq!(info.codec_name, "AAC+");
            assert_eq!((info.sample_rate, info.channels), (44_100, 2));
        }

        #[test]
        fn he_aac_at_48_khz_is_called_aac_plus() {
            let info = open(adts(AudioObjectType::Mpeg4HeAac, 48_000, 48_000, true));
            assert_eq!(info.codec_name, "AAC+");
            assert_eq!(info.sample_rate, 48_000);
        }

        #[test]
        fn mono_he_aac_is_called_aac_plus() {
            let info = open(adts(AudioObjectType::Mpeg4HeAac, 32_000, 44_100, false));
            assert_eq!(info.codec_name, "AAC+");
        }

        #[test]
        fn he_aac_v2_is_called_aac_plus() {
            let info = open(adts(AudioObjectType::Mpeg4HeAacV2, 32_000, 44_100, true));
            assert_eq!(info.codec_name, "AAC+");
            assert_eq!((info.sample_rate, info.channels), (44_100, 2));
        }

        #[test]
        fn tuning_in_mid_stream_still_finds_aac_plus() {
            let v1 = adts(AudioObjectType::Mpeg4HeAac, 64_000, 44_100, true);
            let v2 = adts(AudioObjectType::Mpeg4HeAacV2, 32_000, 44_100, true);
            for n in [1, 7, 20] {
                assert_eq!(
                    open(from_frame(&v1, n)).codec_name,
                    "AAC+",
                    "HE-AAC from frame {n}"
                );
                assert_eq!(
                    open(from_frame(&v2, n)).codec_name,
                    "AAC+",
                    "HE-AAC v2 from frame {n}"
                );
            }
        }

        #[test]
        fn tuning_in_mid_frame_still_tells_them_apart() {
            // A station's first bytes can land anywhere in a frame
            let plain = from_frame(
                &adts(AudioObjectType::Mpeg4LowComplexity, 128_000, 44_100, true),
                7,
            );
            let plus = from_frame(&adts(AudioObjectType::Mpeg4HeAac, 64_000, 44_100, true), 7);
            for cut in [1, 100, 250] {
                assert_eq!(
                    open(plain[cut..].to_vec()).codec_name,
                    "AAC",
                    "AAC cut at {cut}"
                );
                assert_eq!(
                    open(plus[cut..].to_vec()).codec_name,
                    "AAC+",
                    "AAC+ cut at {cut}"
                );
            }
        }

        /// An MP4 track's AudioSpecificConfig: object type, rate index and
        /// stereo, as MP4 and fMP4 (HLS) carry it
        fn config(object_type: u16, rate_index: u16) -> Box<[u8]> {
            Box::new(((object_type << 11) | (rate_index << 7) | (2 << 3)).to_be_bytes())
        }

        fn mp4_track(
            sample_rate: Option<u32>,
            extra_data: Option<Box<[u8]>>,
        ) -> AudioCodecParameters {
            let mut params = AudioCodecParameters::new();
            params.for_codec(CODEC_ID_AAC);
            if let Some(rate) = sample_rate {
                params.with_sample_rate(rate);
            }
            if let Some(config) = extra_data {
                params.with_extra_data(config);
            }
            params
        }

        #[test]
        fn an_mp4_config_that_names_sbr_is_aac_plus() {
            // Explicit signalling: HE-AAC (5) or HE-AAC v2 (29), whatever the
            // rates say. The sample entry often gives the output rate.
            for object_type in [5, 29] {
                let track = mp4_track(Some(44_100), Some(config(object_type, 7)));
                assert!(is_aac_plus(&track, 44_100), "object type {object_type}");
            }
        }

        #[test]
        fn an_mp4_config_gives_the_core_rate() {
            // Implicit signalling: the config has AAC-LC at the core rate,
            // while the sample entry may already say 44100
            let he_aac = mp4_track(Some(44_100), Some(config(2, 7)));
            assert!(is_aac_plus(&he_aac, 44_100));
            let plain = mp4_track(Some(44_100), Some(config(2, 4)));
            assert!(!is_aac_plus(&plain, 44_100));
        }

        #[test]
        fn a_config_with_escape_codes_is_read() {
            // Object type 42 (USAC) after the escape 31, then an explicit
            // 24-bit rate of 24000 Hz after the rate index 15
            let bits: u64 = (31 << 43) | (10 << 37) | (15 << 33) | (24_000 << 9);
            let config = &bits.to_be_bytes()[2..];
            assert_eq!(audio_specific_config(config), Some((42, 24_000)));
            // USAC (xHE-AAC) shows as plain AAC, whatever rate it plays at
            let track = mp4_track(Some(48_000), Some(Box::from(config)));
            assert!(!is_aac_plus(&track, 48_000));
        }

        #[test]
        fn a_new_track_keeps_the_aac_plus_name() {
            let stream = adts(AudioObjectType::Mpeg4HeAac, 64_000, 44_100, true);
            let mut source =
                SymphoniaSource::new_with_hint(Cursor::new(stream), Some("aac")).unwrap();
            let label = source.codec_label();
            assert_eq!(source.codec_name(), "AAC+");
            // As after the demuxer's ResetRequired: the same stream, a new
            // decoder, named again from its first packet
            source.reset_track().unwrap();
            assert!(source.decode_next_packet());
            assert_eq!(source.codec_name(), "AAC+");
            assert_eq!(*label.lock().unwrap(), "AAC+");
        }

        #[test]
        fn the_name_follows_the_rate_the_stream_decodes_at() {
            let stream = adts(AudioObjectType::Mpeg4LowComplexity, 128_000, 44_100, true);
            let mut source =
                SymphoniaSource::new_with_hint(Cursor::new(stream), Some("aac")).unwrap();
            let label = source.codec_label();
            assert_eq!(*label.lock().unwrap(), "AAC");
            // An HLS stream moving to HE-AAC on the same track: the track
            // still declares 22.05 kHz, and SBR doubles it
            source.codec_params.sample_rate = Some(22_050);
            source.sample_rate = 44_100;
            source.named_at_rate = 22_050;
            source.name_codec();
            assert_eq!(*label.lock().unwrap(), "AAC+");
            // And back to plain AAC at the rate it declares
            source.sample_rate = 22_050;
            source.name_codec();
            assert_eq!(source.codec_name(), "AAC");
            assert_eq!(*label.lock().unwrap(), "AAC");
        }

        #[test]
        fn a_broken_config_falls_back_to_the_track_rate() {
            let track = mp4_track(Some(22_050), Some(Box::new([0x12])));
            assert!(is_aac_plus(&track, 44_100));
            assert!(!is_aac_plus(&track, 22_050));
            let unknown = mp4_track(None, None);
            assert!(!is_aac_plus(&unknown, 44_100));
        }
    }

    // --- Garbage from the demuxer ---

    mod demux_errors {
        use super::*;
        use symphonia::core::errors::{Error, Result};
        use symphonia::core::formats::{FormatInfo, MediaInfo, SeekMode, SeekTo, SeekedTo, Track};
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::Metadata;
        use symphonia::core::packet::Packet;

        /// A demuxer that reports `errors` unreadable frames after its
        /// `after`-th packet
        struct Garbled {
            inner: ProbedFormat,
            after: usize,
            errors: u32,
            packets: usize,
        }

        impl FormatReader for Garbled {
            fn format_info(&self) -> &FormatInfo {
                self.inner.format_info()
            }
            fn media_info(&self) -> &MediaInfo {
                self.inner.media_info()
            }
            fn metadata(&mut self) -> Metadata<'_> {
                self.inner.metadata()
            }
            fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
                self.inner.seek(mode, to)
            }
            fn tracks(&self) -> &[Track] {
                self.inner.tracks()
            }
            fn next_packet(&mut self) -> Result<Option<Packet>> {
                if self.packets == self.after && self.errors > 0 {
                    self.errors -= 1;
                    return Err(Error::DecodeError("adts: invalid sample rate"));
                }
                self.packets += 1;
                self.inner.next_packet()
            }
            fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
            where
                Self: 's,
            {
                self.inner.into_inner()
            }
        }

        /// One second of a 44.1 kHz mono WAV whose demuxer reports
        /// `errors` unreadable frames after its third packet
        fn garbled_source(errors: u32) -> SymphoniaSource {
            let samples: Vec<i16> = (0..44100).map(|i| (i % 1000) as i16).collect();
            let wav = make_wav(44100, 1, &samples);
            let probed = start_probe(Cursor::new(wav), Some("wav".into()))
                .unwrap()
                .recv()
                .unwrap()
                .unwrap();
            let garbled = Garbled {
                inner: probed,
                after: 3,
                errors,
                packets: 0,
            };
            SymphoniaSource::from_probed(Box::new(garbled)).unwrap()
        }

        #[test]
        fn a_stretch_of_garbage_is_skipped() {
            let source = garbled_source(20);
            let slot = source.error_slot();
            let stats = source.decoder_stats();
            assert_eq!(source.count(), 44100, "every sample still plays");
            assert!(slot.lock().unwrap().is_none(), "no error");
            assert_eq!(
                stats
                    .decode_errors
                    .load(std::sync::atomic::Ordering::SeqCst),
                20
            );
        }

        #[test]
        fn nothing_but_garbage_ends_the_stream_with_the_reason() {
            let source = garbled_source(u32::MAX);
            let slot = source.error_slot();
            let played = source.count();
            assert!(played > 0 && played < 44100, "played {played}");
            let err = slot.lock().unwrap().clone().expect("an error");
            assert!(err.contains("invalid sample rate"), "{err}");
            assert!(err.contains("100 unreadable frames in a row"), "{err}");
        }
    }
}
