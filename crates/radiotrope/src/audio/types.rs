//! Shared audio types
//!
//! Pure data types used across the audio subsystem.

use std::fmt;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::config::audio::SPECTRUM_BANDS;

/// Current playback state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl fmt::Display for PlaybackState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaybackState::Stopped => write!(f, "Stopped"),
            PlaybackState::Playing => write!(f, "Playing"),
            PlaybackState::Paused => write!(f, "Paused"),
        }
    }
}

/// Codec information for the current stream
#[derive(Debug, Clone)]
pub struct CodecInfo {
    pub codec_name: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: Option<u32>,
    pub bitrate: Option<u32>,
}

impl fmt::Display for CodecInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let channel_str = if self.channels == 1 { "Mono" } else { "Stereo" };
        write!(f, "{}", self.codec_name)?;
        if let Some(br) = self.bitrate {
            write!(f, " · {} kbps", br)?;
        }
        write!(f, " · {} Hz", self.sample_rate)?;
        if let Some(bits) = self.bits_per_sample {
            write!(f, " · {}-bit", bits)?;
        }
        write!(f, " · {}", channel_str)
    }
}

/// Trait alias for a seekable, sendable reader
pub trait ReadSeek: std::io::Read + std::io::Seek + Send + Sync {}
impl<T: std::io::Read + std::io::Seek + Send + Sync> ReadSeek for T {}

/// Commands sent to the audio engine
pub enum AudioCommand {
    /// Start playing from the given reader
    Play {
        reader: Box<dyn ReadSeek>,
        format_hint: Option<String>,
        bitrate: Option<u32>,
        /// Atomic counter tracking bytes received from network (optional)
        bytes_received: Option<Arc<AtomicU64>>,
    },
    /// Stop playback
    Stop,
    /// Pause playback
    Pause,
    /// Resume playback
    Resume,
    /// Set volume (0.0..=2.0)
    SetVolume(f32),
    /// Shut down the engine thread
    Shutdown,
}

impl fmt::Debug for AudioCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioCommand::Play {
                format_hint,
                bitrate,
                bytes_received,
                ..
            } => f
                .debug_struct("Play")
                .field("format_hint", format_hint)
                .field("bitrate", bitrate)
                .field("has_bytes_received", &bytes_received.is_some())
                .finish(),
            AudioCommand::Stop => write!(f, "Stop"),
            AudioCommand::Pause => write!(f, "Pause"),
            AudioCommand::Resume => write!(f, "Resume"),
            AudioCommand::SetVolume(v) => write!(f, "SetVolume({})", v),
            AudioCommand::Shutdown => write!(f, "Shutdown"),
        }
    }
}

/// Events emitted by the audio engine
#[derive(Debug, Clone)]
pub enum AudioEvent {
    /// Playback started with codec info
    Playing(CodecInfo),
    /// Playback stopped
    Stopped,
    /// Playback paused
    Paused,
    /// Playback resumed
    Resumed,
    /// An error occurred
    Error(String),
    /// Metadata update from stream (ICY, etc.)
    MetadataUpdate { title: String, artist: String },
    /// Stream stalled — no audio data received for too long
    StreamStalled,
    /// Format probe timed out (e.g., MPEG-2 ADTS hang)
    ProbeTimeout,
    /// Probe succeeded but no audio samples were produced
    NoAudioTimeout,
    /// Buffer fill percentage (0–100); 100 means done buffering
    Buffering(u8),
}

/// Audio analysis data for visualization (VU meters + spectrum)
#[derive(Clone)]
pub struct AudioAnalysis {
    pub vu_left: f32,
    pub vu_right: f32,
    pub spectrum: [f32; SPECTRUM_BANDS],
    pub sample_count: u64,
}

impl Default for AudioAnalysis {
    fn default() -> Self {
        Self {
            vu_left: 0.0,
            vu_right: 0.0,
            spectrum: [0.0; SPECTRUM_BANDS],
            sample_count: 0,
        }
    }
}

impl AudioAnalysis {
    /// Reset all analysis values to zero
    pub fn reset(&mut self) {
        self.vu_left = 0.0;
        self.vu_right = 0.0;
        self.spectrum = [0.0; SPECTRUM_BANDS];
        self.sample_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // --- PlaybackState ---

    #[test]
    fn playback_state_default_is_stopped() {
        assert_eq!(PlaybackState::default(), PlaybackState::Stopped);
    }

    #[test]
    fn playback_state_display() {
        assert_eq!(PlaybackState::Stopped.to_string(), "Stopped");
        assert_eq!(PlaybackState::Playing.to_string(), "Playing");
        assert_eq!(PlaybackState::Paused.to_string(), "Paused");
    }

    #[test]
    fn playback_state_equality() {
        assert_eq!(PlaybackState::Playing, PlaybackState::Playing);
        assert_ne!(PlaybackState::Playing, PlaybackState::Stopped);
        assert_ne!(PlaybackState::Playing, PlaybackState::Paused);
        assert_ne!(PlaybackState::Stopped, PlaybackState::Paused);
    }

    #[test]
    fn playback_state_clone() {
        let state = PlaybackState::Playing;
        let cloned = state;
        assert_eq!(state, cloned);
    }

    #[test]
    fn playback_state_debug() {
        assert_eq!(format!("{:?}", PlaybackState::Stopped), "Stopped");
        assert_eq!(format!("{:?}", PlaybackState::Playing), "Playing");
        assert_eq!(format!("{:?}", PlaybackState::Paused), "Paused");
    }

    // --- CodecInfo ---

    #[test]
    fn codec_info_display_without_bits() {
        let info = CodecInfo {
            codec_name: "MP3".to_string(),
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: None,
            bitrate: None,
        };
        assert_eq!(info.to_string(), "MP3 · 44100 Hz · Stereo");
    }

    #[test]
    fn codec_info_display_with_bits() {
        let info = CodecInfo {
            codec_name: "FLAC".to_string(),
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: Some(24),
            bitrate: None,
        };
        assert_eq!(info.to_string(), "FLAC · 48000 Hz · 24-bit · Mono");
    }

    #[test]
    fn codec_info_display_with_bitrate() {
        let info = CodecInfo {
            codec_name: "MP3".to_string(),
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: None,
            bitrate: Some(192),
        };
        assert_eq!(info.to_string(), "MP3 · 192 kbps · 44100 Hz · Stereo");
    }

    #[test]
    fn codec_info_display_with_bits_and_bitrate() {
        let info = CodecInfo {
            codec_name: "FLAC".to_string(),
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: Some(24),
            bitrate: Some(320),
        };
        assert_eq!(
            info.to_string(),
            "FLAC · 320 kbps · 48000 Hz · 24-bit · Stereo"
        );
    }

    #[test]
    fn codec_info_display_mono_shows_mono() {
        let info = CodecInfo {
            codec_name: "AAC".to_string(),
            channels: 1,
            sample_rate: 22050,
            bits_per_sample: None,
            bitrate: None,
        };
        assert!(info.to_string().contains("Mono"));
    }

    #[test]
    fn codec_info_display_multichannel_shows_stereo() {
        // Any channel count > 1 displays as "Stereo" (current behavior)
        for ch in [2, 4, 6, 8] {
            let info = CodecInfo {
                codec_name: "PCM".to_string(),
                channels: ch,
                sample_rate: 44100,
                bits_per_sample: None,
                bitrate: None,
            };
            assert!(
                info.to_string().contains("Stereo"),
                "channels={} should display as Stereo",
                ch
            );
        }
    }

    #[test]
    fn codec_info_display_high_sample_rate() {
        let info = CodecInfo {
            codec_name: "FLAC".to_string(),
            channels: 2,
            sample_rate: 192000,
            bits_per_sample: Some(32),
            bitrate: None,
        };
        assert!(info.to_string().contains("192000"));
    }

    #[test]
    fn codec_info_display_zero_sample_rate() {
        let info = CodecInfo {
            codec_name: "Unknown".to_string(),
            channels: 2,
            sample_rate: 0,
            bits_per_sample: None,
            bitrate: None,
        };
        // Should not panic, just display 0
        assert!(info.to_string().contains("0 Hz"));
    }

    #[test]
    fn codec_info_clone() {
        let info = CodecInfo {
            codec_name: "Opus".to_string(),
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: Some(16),
            bitrate: None,
        };
        let cloned = info.clone();
        assert_eq!(cloned.codec_name, "Opus");
        assert_eq!(cloned.channels, 2);
        assert_eq!(cloned.sample_rate, 48000);
        assert_eq!(cloned.bits_per_sample, Some(16));
    }

    #[test]
    fn codec_info_debug() {
        let info = CodecInfo {
            codec_name: "MP3".to_string(),
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: None,
            bitrate: None,
        };
        let debug = format!("{:?}", info);
        assert!(debug.contains("MP3"));
        assert!(debug.contains("44100"));
    }

    #[test]
    fn codec_info_empty_codec_name() {
        let info = CodecInfo {
            codec_name: String::new(),
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: None,
            bitrate: None,
        };
        // Should not panic
        let display = info.to_string();
        assert!(display.contains("44100"));
    }

    // --- AudioCommand ---

    #[test]
    fn audio_command_debug() {
        let cmd = AudioCommand::Stop;
        assert_eq!(format!("{:?}", cmd), "Stop");

        let cmd = AudioCommand::Pause;
        assert_eq!(format!("{:?}", cmd), "Pause");

        let cmd = AudioCommand::Resume;
        assert_eq!(format!("{:?}", cmd), "Resume");

        let cmd = AudioCommand::SetVolume(0.5);
        assert_eq!(format!("{:?}", cmd), "SetVolume(0.5)");

        let cmd = AudioCommand::Shutdown;
        assert_eq!(format!("{:?}", cmd), "Shutdown");
    }

    #[test]
    fn audio_command_play_debug_with_hint() {
        let cmd = AudioCommand::Play {
            reader: Box::new(Cursor::new(vec![0u8; 10])),
            format_hint: Some("aac".to_string()),
            bitrate: None,
            bytes_received: None,
        };
        let debug = format!("{:?}", cmd);
        assert!(debug.contains("Play"));
        assert!(debug.contains("aac"));
    }

    #[test]
    fn audio_command_play_debug_without_hint() {
        let cmd = AudioCommand::Play {
            reader: Box::new(Cursor::new(vec![0u8; 10])),
            format_hint: None,
            bitrate: None,
            bytes_received: None,
        };
        let debug = format!("{:?}", cmd);
        assert!(debug.contains("Play"));
        assert!(debug.contains("None"));
    }

    #[test]
    fn audio_command_set_volume_boundary_values() {
        // These should not panic
        let _ = format!("{:?}", AudioCommand::SetVolume(0.0));
        let _ = format!("{:?}", AudioCommand::SetVolume(1.0));
        let _ = format!("{:?}", AudioCommand::SetVolume(2.0));
        let _ = format!("{:?}", AudioCommand::SetVolume(-1.0));
        let _ = format!("{:?}", AudioCommand::SetVolume(f32::INFINITY));
        let _ = format!("{:?}", AudioCommand::SetVolume(f32::NAN));
    }

    // --- AudioEvent ---

    #[test]
    fn audio_event_debug() {
        let evt = AudioEvent::Stopped;
        assert_eq!(format!("{:?}", evt), "Stopped");

        let evt = AudioEvent::Paused;
        assert_eq!(format!("{:?}", evt), "Paused");

        let evt = AudioEvent::Resumed;
        assert_eq!(format!("{:?}", evt), "Resumed");

        let evt = AudioEvent::Error("test error".to_string());
        let debug = format!("{:?}", evt);
        assert!(debug.contains("test error"));
    }

    #[test]
    fn audio_event_playing_debug() {
        let info = CodecInfo {
            codec_name: "MP3".to_string(),
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: None,
            bitrate: None,
        };
        let evt = AudioEvent::Playing(info);
        let debug = format!("{:?}", evt);
        assert!(debug.contains("Playing"));
        assert!(debug.contains("MP3"));
    }

    #[test]
    fn audio_event_metadata_update() {
        let evt = AudioEvent::MetadataUpdate {
            title: "Song Title".to_string(),
            artist: "Artist Name".to_string(),
        };
        let debug = format!("{:?}", evt);
        assert!(debug.contains("Song Title"));
        assert!(debug.contains("Artist Name"));
    }

    #[test]
    fn audio_event_clone() {
        let evt = AudioEvent::Error("cloned error".to_string());
        let cloned = evt.clone();
        if let AudioEvent::Error(msg) = cloned {
            assert_eq!(msg, "cloned error");
        } else {
            panic!("Expected Error variant after clone");
        }
    }

    #[test]
    fn audio_event_paused_clone() {
        let evt = AudioEvent::Paused;
        let cloned = evt.clone();
        assert!(matches!(cloned, AudioEvent::Paused));
    }

    #[test]
    fn audio_event_resumed_clone() {
        let evt = AudioEvent::Resumed;
        let cloned = evt.clone();
        assert!(matches!(cloned, AudioEvent::Resumed));
    }

    #[test]
    fn audio_event_metadata_empty_strings() {
        let evt = AudioEvent::MetadataUpdate {
            title: String::new(),
            artist: String::new(),
        };
        // Should not panic
        let _ = format!("{:?}", evt);
    }

    // --- AudioAnalysis ---

    #[test]
    fn audio_analysis_default_is_zero() {
        let analysis = AudioAnalysis::default();
        assert_eq!(analysis.vu_left, 0.0);
        assert_eq!(analysis.vu_right, 0.0);
        assert!(analysis.spectrum.iter().all(|&v| v == 0.0));
        assert_eq!(analysis.sample_count, 0);
    }

    #[test]
    fn audio_analysis_reset() {
        let mut analysis = AudioAnalysis {
            vu_left: 0.5,
            vu_right: 0.8,
            spectrum: [0.3; SPECTRUM_BANDS],
            sample_count: 42,
        };
        analysis.reset();
        assert_eq!(analysis.vu_left, 0.0);
        assert_eq!(analysis.vu_right, 0.0);
        assert!(analysis.spectrum.iter().all(|&v| v == 0.0));
        assert_eq!(analysis.sample_count, 0);
    }

    #[test]
    fn audio_analysis_reset_from_extreme_values() {
        let mut analysis = AudioAnalysis {
            vu_left: f32::MAX,
            vu_right: f32::MIN,
            spectrum: [f32::MAX; SPECTRUM_BANDS],
            sample_count: u64::MAX,
        };
        analysis.reset();
        assert_eq!(analysis.vu_left, 0.0);
        assert_eq!(analysis.vu_right, 0.0);
        assert!(analysis.spectrum.iter().all(|&v| v == 0.0));
        assert_eq!(analysis.sample_count, 0);
    }

    #[test]
    fn audio_analysis_clone() {
        let analysis = AudioAnalysis {
            vu_left: 0.42,
            vu_right: 0.58,
            spectrum: [0.1; SPECTRUM_BANDS],
            sample_count: 999,
        };
        let cloned = analysis.clone();
        assert_eq!(cloned.vu_left, 0.42);
        assert_eq!(cloned.vu_right, 0.58);
        assert!(cloned
            .spectrum
            .iter()
            .all(|&v| (v - 0.1).abs() < f32::EPSILON));
        assert_eq!(cloned.sample_count, 999);
    }

    #[test]
    fn audio_analysis_spectrum_has_correct_length() {
        let analysis = AudioAnalysis::default();
        assert_eq!(analysis.spectrum.len(), SPECTRUM_BANDS);
    }

    #[test]
    fn audio_analysis_individual_spectrum_bands() {
        let mut analysis = AudioAnalysis::default();
        for i in 0..SPECTRUM_BANDS {
            analysis.spectrum[i] = i as f32 / SPECTRUM_BANDS as f32;
        }
        for i in 0..SPECTRUM_BANDS {
            let expected = i as f32 / SPECTRUM_BANDS as f32;
            assert!(
                (analysis.spectrum[i] - expected).abs() < f32::EPSILON,
                "Band {} mismatch",
                i
            );
        }
        analysis.reset();
        assert!(analysis.spectrum.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn audio_analysis_double_reset() {
        let mut analysis = AudioAnalysis {
            vu_left: 1.0,
            vu_right: 1.0,
            spectrum: [1.0; SPECTRUM_BANDS],
            sample_count: 100,
        };
        analysis.reset();
        analysis.reset();
        assert_eq!(analysis.vu_left, 0.0);
        assert_eq!(analysis.vu_right, 0.0);
    }

    // --- ReadSeek trait ---

    #[test]
    fn cursor_implements_read_seek() {
        let cursor = Cursor::new(vec![1u8, 2, 3]);
        let _boxed: Box<dyn ReadSeek> = Box::new(cursor);
    }

    #[test]
    fn read_seek_trait_with_empty_cursor() {
        let cursor = Cursor::new(Vec::<u8>::new());
        let mut boxed: Box<dyn ReadSeek> = Box::new(cursor);
        let mut buf = [0u8; 10];
        let n = std::io::Read::read(&mut *boxed, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    // --- New resilience events ---

    #[test]
    fn audio_event_stream_stalled() {
        let evt = AudioEvent::StreamStalled;
        let debug = format!("{:?}", evt);
        assert!(debug.contains("StreamStalled"));
        let cloned = evt.clone();
        assert!(matches!(cloned, AudioEvent::StreamStalled));
    }

    #[test]
    fn audio_event_probe_timeout() {
        let evt = AudioEvent::ProbeTimeout;
        let debug = format!("{:?}", evt);
        assert!(debug.contains("ProbeTimeout"));
        let cloned = evt.clone();
        assert!(matches!(cloned, AudioEvent::ProbeTimeout));
    }

    #[test]
    fn audio_event_no_audio_timeout() {
        let evt = AudioEvent::NoAudioTimeout;
        let debug = format!("{:?}", evt);
        assert!(debug.contains("NoAudioTimeout"));
        let cloned = evt.clone();
        assert!(matches!(cloned, AudioEvent::NoAudioTimeout));
    }
}
