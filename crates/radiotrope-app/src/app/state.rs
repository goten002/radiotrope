//! Shared application state and commands
//!
//! `AppCommand` is the unified command type sent by any frontend (GUI, MCP, tray).
//! `AppSnapshot` is the shared state read by MCP tool handlers.

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::Duration;

use radiotrope::audio::{PlaybackState, RecordingFormat};
use radiotrope_app::data::settings::Settings;

/// Commands sent by any frontend (GUI, MCP, tray)
pub enum AppCommand {
    // Playback
    Play {
        url: String,
        name: Option<String>,
        /// The station's logo, for the header while it plays
        logo_url: Option<String>,
        /// The station's country, shown with it and kept if it is starred
        country: Option<String>,
        /// Told the `play_seq` the station gets once the controller takes
        /// the command up, so an agent's play follows its own station and
        /// not one started just before or after it
        taken: Option<tokio::sync::oneshot::Sender<u64>>,
    },
    Stop,
    SetVolume(f32),
    Mute,
    Unmute,

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

impl AppCommand {
    /// The commands that set the equalizer as `settings` keep it, sent at
    /// startup. A saved preset brings its own preamp, as when it is picked:
    /// the saved one can be from a build where the preset's gains differed.
    /// Custom gains get the saved preamp.
    pub fn restore_eq(settings: &Settings) -> Vec<AppCommand> {
        let mut commands = vec![AppCommand::SetEqEnabled(settings.eq_enabled)];
        match &settings.eq_preset_name {
            Some(name) => commands.push(AppCommand::SetEqPreset(name.clone())),
            None => {
                commands.push(AppCommand::SetEqGains(settings.eq_gains));
                commands.push(AppCommand::SetEqPreamp(settings.eq_preamp));
            }
        }
        commands
    }
}

/// Snapshot of app state — shared between controller, GUI, and MCP
#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub playback: PlaybackState,
    pub station_name: Option<String>,
    pub station_url: Option<String>,
    /// Logo of the station being played, as given with its Play: the GUI
    /// shows it whoever started the station (the UI or an agent)
    pub station_logo_url: Option<String>,
    /// Country of the station being played, as given with its Play
    pub station_country: Option<String>,
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
    /// The recording settings as the window has them, for an agent's
    /// start_recording
    pub recording_setup: RecordingSetup,
}

/// The choices of the Recording Settings dialog. Kept in memory, so an
/// agent's recording uses what the window shows without reading the
/// settings file while the window may be saving it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordingSetup {
    /// `None` is the default folder
    pub dir: Option<PathBuf>,
    pub format: RecordingFormat,
    /// MP3/Opus bitrate in kbps; `None` uses the station's bitrate
    pub bitrate: Option<u32>,
    /// Record after the equalizer (only counts while the EQ is on)
    pub with_eq: bool,
}

impl RecordingSetup {
    pub fn from_settings(settings: &radiotrope_app::data::settings::Settings) -> Self {
        Self {
            dir: settings.recording_dir.clone(),
            format: settings.recording_format.into(),
            bitrate: settings.recording_bitrate,
            with_eq: settings.record_with_eq,
        }
    }
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
            station_logo_url: None,
            station_country: None,
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
            recording_setup: RecordingSetup::default(),
        }
    }
}
