//! Shared application state and commands
//!
//! `AppCommand` is the unified command type sent by any frontend (GUI, MCP, tray).
//! `AppSnapshot` is the shared state read by MCP tool handlers.

use std::borrow::Cow;

use radiotrope::audio::PlaybackState;

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

    // Favorites (planned)
    #[allow(dead_code)]
    AddFavorite {
        name: String,
        url: String,
    },
    #[allow(dead_code)]
    RemoveFavorite(String),

    // Search (planned)
    #[allow(dead_code)]
    Search(String),

    // State query (MCP reads shared_state directly)
    #[allow(dead_code)]
    GetState,

    // Shutdown the app
    Shutdown,

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

    // Codec / stream info for the playback display
    pub codec_name: String,
    pub stream_type: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate: Option<u32>,
    pub status_text: Cow<'static, str>,
    /// True when status_text represents an error/warning state (for red UI text)
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
            codec_name: String::new(),
            stream_type: String::new(),
            sample_rate: 0,
            channels: 0,
            bitrate: None,
            status_text: Cow::Borrowed("Ready"),
            is_error: false,
        }
    }
}
