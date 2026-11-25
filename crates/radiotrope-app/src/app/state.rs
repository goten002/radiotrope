//! Shared application state and commands
//!
//! `AppCommand` is the unified command type sent by any frontend (GUI, MCP, tray).
//! `AppSnapshot` is the shared state read by MCP tool handlers.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use radiotrope::audio::{
    HissLevel, HumReadout, LevelReadout, LevelTarget, Listen, PlaybackState, RecordingFormat,
    SharedStats,
};
use radiotrope_app::data::schedule::{Action, Entry, LastRun};
use radiotrope_app::data::settings::Settings;

/// Told whether a schedule change was made, or why not
pub type ScheduleReply<T> = Option<tokio::sync::oneshot::Sender<Result<T, String>>>;

/// Commands sent by any frontend (GUI, MCP, tray)
pub enum AppCommand {
    // Playback
    Play {
        /// The station's default stream
        url: String,
        /// The station's other streams, tried in order when `url`
        /// doesn't connect
        backups: Vec<String>,
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
    /// Play a station stepped to with Next or Previous: it is the current
    /// one at once and is connected to a moment later, so stepping past
    /// stations starts only the last
    Step {
        url: String,
        backups: Vec<String>,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
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

    // Filters
    SetMatchVolume(bool),
    SetLevelTarget(LevelTarget),
    SetHissReducer(HissLevel),
    SetHumFilter(bool),
    SetRumbleFilter(bool),
    SetSmoothSwitching(bool),

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

    // Schedule
    /// Stop playback this many minutes from now; `None` turns it off
    SetSleepTimer {
        minutes: Option<u32>,
        /// Lower the volume over the last minute
        fade: bool,
    },
    /// Fade out at the end of the sleep timer, or not
    SetSleepFade(bool),
    /// Add an entry (id 0) or replace the one with its id. Told the id.
    SaveScheduleEntry {
        entry: Entry,
        reply: ScheduleReply<u64>,
    },
    RemoveScheduleEntry {
        id: u64,
        reply: ScheduleReply<()>,
    },
    SetScheduleEntryEnabled {
        id: u64,
        enabled: bool,
        reply: ScheduleReply<()>,
    },

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
    /// A preamp moved by hand on top of the preset is kept, and custom gains
    /// get the saved preamp.
    pub fn restore_eq(settings: &Settings) -> Vec<AppCommand> {
        let mut commands = vec![AppCommand::SetEqEnabled(settings.eq_enabled)];
        match &settings.eq_preset_name {
            Some(name) => {
                commands.push(AppCommand::SetEqPreset(name.clone()));
                if settings.eq_preamp_moved {
                    commands.push(AppCommand::SetEqPreamp(settings.eq_preamp));
                }
            }
            None => {
                commands.push(AppCommand::SetEqGains(settings.eq_gains));
                commands.push(AppCommand::SetEqPreamp(settings.eq_preamp));
            }
        }
        commands
    }

    /// The commands that set the filters as `settings` keep them,
    /// sent at startup. Unknown names get the defaults.
    pub fn restore_sound(settings: &Settings) -> Vec<AppCommand> {
        vec![
            AppCommand::SetMatchVolume(settings.match_volume),
            AppCommand::SetLevelTarget(
                LevelTarget::parse(&settings.match_level).unwrap_or_default(),
            ),
            AppCommand::SetHissReducer(
                HissLevel::parse(&settings.hiss_reducer).unwrap_or_default(),
            ),
            AppCommand::SetHumFilter(settings.hum_filter),
            AppCommand::SetRumbleFilter(settings.rumble_filter),
            AppCommand::SetSmoothSwitching(settings.smooth_switching),
        ]
    }
}

/// Snapshot of app state — shared between controller, GUI, and MCP
#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub playback: PlaybackState,
    pub station_name: Option<String>,
    /// The station's address: its default stream's, whichever plays
    pub station_url: Option<String>,
    /// The stream tried or playing: `station_url`, or one of the
    /// station's backups after the default didn't connect
    pub stream_url: Option<String>,
    /// Which of the station's streams that is, in the order they are
    /// tried (0: the default), and how many the station has
    pub stream_index: usize,
    pub stream_count: usize,
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
    /// True once the resolved stream is handed to the engine, until it
    /// plays, stops or fails. `playback` is still Stopped meanwhile.
    pub is_connecting: bool,
    /// An alarm's station didn't start and the alarm beeps instead.
    /// `playback` is Playing, so Stop silences it, but the station isn't.
    pub alarm_beep: bool,
    /// How loud the fades (an entry's fade in, a fading Stop, the Sleep
    /// Timer's fade out) let the station play now, from 0 to 1
    pub fade_gain: f32,
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
    /// The preamp was moved by hand since the preset was picked
    pub eq_preamp_moved: bool,
    pub eq_enabled: bool,
    pub eq_preset_name: Option<String>,

    // Filters
    pub match_volume: bool,
    pub level_target: LevelTarget,
    pub hiss: HissLevel,
    pub hum_filter: bool,
    pub rumble_filter: bool,
    pub smooth_switching: bool,
    /// What the leveler does to the station playing; `None` until the
    /// audio engine runs
    pub level: Option<Arc<LevelReadout>>,
    /// The hum the Hum Filter takes out of the station playing; `None`
    /// until the audio engine runs
    pub hum: Option<Arc<HumReadout>>,
    /// The engine's stream statistics, for phones (the window gets them
    /// its own way); `None` until the audio engine runs
    pub stats: Option<SharedStats>,

    // Recording
    /// Progress of the running recording, `None` when not recording
    pub recording: Option<RecordingProgress>,
    /// Last message for the window to show under the station: a recording
    /// saved or failed, or a scheduled entry missed or skipped
    pub recording_notice: Option<RecordingNotice>,
    /// The recording settings as the window has them, for an agent's
    /// start_recording
    pub recording_setup: RecordingSetup,

    // Schedule
    /// The scheduled entries, as saved
    pub schedule: Vec<Entry>,
    /// Counts the changes to `schedule`, so the window redraws its list
    /// only when it changed
    pub schedule_seq: u64,
    /// How each entry's last run went, for its row. A change counts in
    /// `schedule_seq` too.
    pub schedule_results: Vec<LastRun>,
    /// The running sleep timer
    pub sleep: Option<SleepTimerInfo>,
    /// The entry playing or recording now
    pub scheduled: Option<ScheduledNow>,
    /// The next entry to come round
    pub next_run: Option<NextRun>,

    // Listening on phones
    /// Phones listening to the station; `None` until the audio engine runs
    pub listen: Option<Listen>,
}

/// What the player is doing, worked out from the state's flags: the
/// window, the tray and phones ask this instead of reading the status
/// line, whose wording is free to change
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayerPhase {
    /// Nothing was played yet
    #[default]
    Idle,
    /// The station's stream is being found
    Connecting,
    /// The engine has the stream and fills its buffer; nothing is heard yet
    Buffering,
    Playing,
    /// The station went away and is tried again (a recording waits for it)
    Reconnecting,
    Paused,
    Stopped,
    /// The station didn't start, or ended with an error
    Failed,
}

impl PlayerPhase {
    /// On its way to playing: the status chip pulses and Play reads Stop
    pub fn is_loading(self) -> bool {
        matches!(
            self,
            PlayerPhase::Connecting | PlayerPhase::Buffering | PlayerPhase::Reconnecting
        )
    }

    /// Playing or on its way to: a Stop has something to stop
    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    pub fn is_active(self) -> bool {
        self == PlayerPhase::Playing || self.is_loading()
    }
}

impl AppSnapshot {
    /// What the player is doing now
    pub fn phase(&self) -> PlayerPhase {
        if self.playback == PlaybackState::Playing {
            return PlayerPhase::Playing;
        }
        if self.playback == PlaybackState::Paused {
            return PlayerPhase::Paused;
        }
        if self.waits_to_reconnect() {
            return PlayerPhase::Reconnecting;
        }
        if self.is_resolving {
            return PlayerPhase::Connecting;
        }
        if self.is_connecting {
            return PlayerPhase::Buffering;
        }
        if self.is_error {
            return PlayerPhase::Failed;
        }
        if self.play_seq == 0 {
            PlayerPhase::Idle
        } else {
            PlayerPhase::Stopped
        }
    }

    /// The station isn't heard and is tried again. The controller has no
    /// flag for this: a recording that goes on while nothing plays is one
    /// waiting for its station, and otherwise the status is the one constant
    /// the controller sets while it retries (compared as that constant, never
    /// as its wording).
    fn waits_to_reconnect(&self) -> bool {
        self.playback == PlaybackState::Stopped
            && !self.is_error
            && (self.recording.is_some()
                || self.status_text == radiotrope_app::text::status::RECONNECTING)
    }
}

/// The running sleep timer
#[derive(Clone, Debug, PartialEq)]
pub struct SleepTimerInfo {
    pub until: DateTime<Local>,
    pub fade: bool,
    /// The length it was started with, for Restart
    pub minutes: u32,
}

/// The entry playing or recording now
#[derive(Clone, Debug, PartialEq)]
pub struct ScheduledNow {
    pub id: u64,
    /// "Play Jazz FM"
    pub title: String,
    pub until: Option<DateTime<Local>>,
    pub record: bool,
}

/// The next entry to come round
#[derive(Clone, Debug, PartialEq)]
pub struct NextRun {
    pub id: u64,
    pub at: DateTime<Local>,
    /// "Play Jazz FM"
    pub title: String,
    pub action: Action,
    /// Its station's name ("" for Stop)
    pub station: String,
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

/// Recording settings a phone changed; what is `None` stays as it was.
/// The folder isn't here: it is only set from the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordingSettingsChange {
    pub format: Option<radiotrope_app::data::settings::RecordingFormat>,
    /// `Some(None)` is Auto
    pub bitrate: Option<Option<u32>>,
    pub with_eq: Option<bool>,
}

impl RecordingSettingsChange {
    /// Make the change in settings that are about to be saved
    pub fn apply(&self, settings: &mut Settings) {
        if let Some(format) = self.format {
            settings.recording_format = format;
        }
        if let Some(bitrate) = self.bitrate {
            settings.recording_bitrate = bitrate;
        }
        if let Some(on) = self.with_eq {
            settings.record_with_eq = on;
        }
    }

    /// Make the change in the settings recordings start with
    pub fn apply_to_setup(&self, setup: &mut RecordingSetup) {
        if let Some(format) = self.format {
            setup.format = format.into();
        }
        if let Some(bitrate) = self.bitrate {
            setup.bitrate = bitrate;
        }
        if let Some(on) = self.with_eq {
            setup.with_eq = on;
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

/// What a notice is about. It decides the notice's icon, what a click
/// on it does, and whether it stays until it is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// A recording was saved: a click opens the recordings folder
    Saved,
    /// The Scheduler's news (an entry missed or skipped): a click opens
    /// the Scheduler
    Scheduler,
    /// The Sleep Timer's news: a click opens the Sleep Timer
    SleepTimer,
    /// Something went wrong
    Error,
}

impl NoticeKind {
    /// Stays on screen until closed. What fails or is missed mostly
    /// happens with nobody watching; "Saved" answers a press and passes.
    pub fn stays(self) -> bool {
        matches!(self, NoticeKind::Scheduler | NoticeKind::Error)
    }

    /// The kind as the window's `recording-notice-kind` takes it
    pub fn id(self) -> &'static str {
        match self {
            NoticeKind::Saved => "saved",
            NoticeKind::Scheduler => "scheduler",
            NoticeKind::SleepTimer => "sleep",
            NoticeKind::Error => "error",
        }
    }
}

/// A message for the UI to show under the station: about a recording,
/// or from the Scheduler or the Sleep Timer
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingNotice {
    /// Increases with every notice, so the UI can tell a new one apart
    pub seq: u64,
    pub text: String,
    pub kind: NoticeKind,
    /// `kind` is [`NoticeKind::Error`], for those who only ask that
    /// (agents and phones)
    pub is_error: bool,
}

impl RecordingNotice {
    pub fn new(seq: u64, text: impl Into<String>, kind: NoticeKind) -> Self {
        Self {
            seq,
            text: text.into(),
            kind,
            is_error: kind == NoticeKind::Error,
        }
    }
}

impl Default for AppSnapshot {
    fn default() -> Self {
        Self {
            playback: PlaybackState::default(),
            station_name: None,
            station_url: None,
            stream_url: None,
            stream_index: 0,
            stream_count: 0,
            station_logo_url: None,
            station_country: None,
            title: String::new(),
            artist: String::new(),
            volume: 1.0,
            is_muted: false,
            last_error: None,
            is_resolving: false,
            is_connecting: false,
            alarm_beep: false,
            fade_gain: 1.0,
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
            eq_preamp_moved: false,
            eq_enabled: false,
            eq_preset_name: Some("Flat".to_string()),
            match_volume: true,
            level_target: LevelTarget::default(),
            hiss: HissLevel::Off,
            hum_filter: true,
            rumble_filter: true,
            smooth_switching: true,
            level: None,
            hum: None,
            stats: None,
            recording: None,
            recording_notice: None,
            recording_setup: RecordingSetup::default(),
            schedule: Vec::new(),
            schedule_seq: 0,
            schedule_results: Vec::new(),
            sleep: None,
            scheduled: None,
            next_run: None,
            listen: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radiotrope_app::text::status;

    /// One state per phase, as the controller leaves them
    fn states() -> Vec<(PlayerPhase, AppSnapshot)> {
        let played = || AppSnapshot {
            play_seq: 1,
            station_url: Some("http://jazz.test/stream".into()),
            ..Default::default()
        };
        vec![
            (PlayerPhase::Idle, AppSnapshot::default()),
            (
                PlayerPhase::Connecting,
                AppSnapshot {
                    is_resolving: true,
                    status_text: status::CONNECTING.into(),
                    ..played()
                },
            ),
            (
                PlayerPhase::Buffering,
                AppSnapshot {
                    is_connecting: true,
                    status_text: "Buffering 40%".into(),
                    ..played()
                },
            ),
            (
                PlayerPhase::Playing,
                AppSnapshot {
                    playback: PlaybackState::Playing,
                    status_text: status::PLAYING.into(),
                    ..played()
                },
            ),
            (
                PlayerPhase::Paused,
                AppSnapshot {
                    playback: PlaybackState::Paused,
                    status_text: status::PAUSED.into(),
                    ..played()
                },
            ),
            (
                PlayerPhase::Stopped,
                AppSnapshot {
                    status_text: status::STOPPED.into(),
                    ..played()
                },
            ),
            (
                PlayerPhase::Failed,
                AppSnapshot {
                    is_error: true,
                    status_text: "Can't connect".into(),
                    ..played()
                },
            ),
            // A recording waits for its station, between two tries
            (
                PlayerPhase::Reconnecting,
                AppSnapshot {
                    recording: Some(RecordingProgress {
                        path: PathBuf::from("jazz.mp3"),
                        duration: Duration::from_secs(90),
                        bytes: 1,
                    }),
                    status_text: status::RECONNECTING.into(),
                    ..played()
                },
            ),
        ]
    }

    #[test]
    fn the_phase_follows_the_flags() {
        for (phase, state) in states() {
            assert_eq!(state.phase(), phase, "{}", state.status_text);
        }
    }

    #[test]
    fn rewording_the_status_keeps_the_phase() {
        // Every status as some other wording: the phase is the same.
        // Reconnecting is told by its waiting recording here.
        for (phase, mut state) in states() {
            state.status_text = format!("{} (reworded)", state.status_text).into();
            assert_eq!(state.phase(), phase);
        }
        // Without a recording, a retry is known by the constant the
        // controller sets, whatever it reads
        let retrying = AppSnapshot {
            play_seq: 1,
            is_resolving: true,
            status_text: status::RECONNECTING.into(),
            ..Default::default()
        };
        assert_eq!(retrying.phase(), PlayerPhase::Reconnecting);
    }

    #[test]
    fn loading_is_connecting_buffering_and_reconnecting() {
        use PlayerPhase::*;
        for phase in [Connecting, Buffering, Reconnecting] {
            assert!(phase.is_loading() && phase.is_active(), "{phase:?}");
        }
        for phase in [Idle, Paused, Stopped, Failed] {
            assert!(!phase.is_loading() && !phase.is_active(), "{phase:?}");
        }
        assert!(Playing.is_active() && !Playing.is_loading());
    }

    #[test]
    fn a_stall_while_playing_is_still_playing() {
        // "Waiting for data…" is shown as an error, with the station on
        let stalled = AppSnapshot {
            playback: PlaybackState::Playing,
            is_error: true,
            status_text: status::WAITING_FOR_DATA.into(),
            ..Default::default()
        };
        assert_eq!(stalled.phase(), PlayerPhase::Playing);
    }
}
