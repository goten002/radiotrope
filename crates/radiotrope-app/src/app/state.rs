//! Shared application state and commands
//!
//! `AppCommand` is the unified command type sent by any frontend (GUI, MCP, tray).
//! `AppSnapshot` is the shared state read by MCP tool handlers.

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::Duration;

use radiotrope::audio::{PlaybackState, RecordingFormat};

/// Commands sent by any frontend (GUI, MCP, tray)
pub enum AppCommand {
    // Playback
    Play {
        url: String,
        name: Option<String>,
    },
    Stop,
    #[allow(dead_code)] // planned: pause/resume from MCP
    Pause,
    #[allow(dead_code)] // planned: pause/resume from MCP
    Resume,
    SetVolume(f32),
    Mute,
    Unmute,

    // State query (MCP reads shared_state directly)
    #[allow(dead_code)]
    GetState,

    // Shutdown the app
    Shutdown,

    // Equalizer
    SetEqBand {
        band: usize,
        gain_db: f32,
    },
    SetEqPreset(String),
    SetEqGains([f32; 10]),
    SetEqPreamp(f32),
    SetEqEnabled(bool),

    // Recording
    /// Record the playing station to `<folder>/<Station> - <time>.<ext>`
    StartRecording {
        folder: PathBuf,
        format: RecordingFormat,
        /// MP3/Opus bitrate in kbps; `None` uses the station's bitrate
        bitrate: Option<u32>,
        /// Record the sound after the equalizer instead of before it
        with_eq: bool,
        /// Station logo as PNG, for the file's cover art
        cover: Option<Vec<u8>>,
    },
    StopRecording,

    // Internal: stream resolved on worker thread (not sent by frontends)
    InternalStreamResolved {
        generation: u64,
        result: Result<radiotrope::stream::ResolvedStream, String>,
    },
}

/// Snapshot of app state — shared between controller, GUI, and MCP
#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub playback: PlaybackState,
    pub station_name: Option<String>,
    pub station_url: Option<String>,
    pub title: String,
    pub artist: String,
    pub volume: f32,
    pub is_muted: bool,
    /// Last error from stream resolution or engine
    pub last_error: Option<String>,
    /// True while a stream is being resolved (not yet playing or failed)
    pub is_resolving: bool,
    /// Counts the stations started, so a caller can tell when its own Play
    /// has been taken up
    pub play_seq: u64,

    // Codec / stream info for the playback display
    pub codec_name: String,
    pub stream_type: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate: Option<u32>,
    pub status_text: Cow<'static, str>,
    /// True when status_text represents an error/warning state (for red UI text)
    pub is_error: bool,

    // Accent color
    pub accent_color: Option<String>,

    // Equalizer
    pub eq_gains: [f32; 10],
    pub eq_preamp: f32,
    pub eq_enabled: bool,
    pub eq_preset_name: Option<String>,

    // Recording
    /// Progress of the running recording, `None` when not recording
    pub recording: Option<RecordingProgress>,
    /// Last "saved" or error message about a recording
    pub recording_notice: Option<RecordingNotice>,
}

/// Progress of the running recording
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingProgress {
    pub path: PathBuf,
    pub duration: Duration,
    pub bytes: u64,
}

/// A message about a recording for the UI to show briefly
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingNotice {
    /// Increases with every notice, so the UI can tell a new one apart
    pub seq: u64,
    pub text: String,
    pub is_error: bool,
}

impl Default for AppSnapshot {
    fn default() -> Self {
        Self {
            playback: PlaybackState::default(),
            station_name: None,
            station_url: None,
            title: String::new(),
            artist: String::new(),
            volume: 1.0,
            is_muted: false,
            last_error: None,
            is_resolving: false,
            play_seq: 0,
            codec_name: String::new(),
            stream_type: String::new(),
            sample_rate: 0,
            channels: 0,
            bitrate: None,
            status_text: Cow::Borrowed("Ready"),
            is_error: false,
            accent_color: None,
            eq_gains: [0.0; 10],
            eq_preamp: 0.0,
            eq_enabled: false,
            eq_preset_name: Some("Flat".to_string()),
            recording: None,
            recording_notice: None,
        }
    }
}
