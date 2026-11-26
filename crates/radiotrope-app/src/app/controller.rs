//! Application controller
//!
//! Owns the audio engine, shared state, and processes commands from all
//! frontends (GUI, MCP, tray) through a single crossbeam channel.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crossbeam_channel::{at, never, select, tick, Receiver, Sender};

use radiotrope::audio::{
    AudioAnalysis, AudioEngine, AudioEvent, EngineEvent, PlaybackState, RecordingFormat,
    RecordingOptions, RecordingStatus, RecordingTags, SharedStats, StreamId, TapPoint,
};
use radiotrope::config::timeouts::RESOLVE_TIMEOUT_SECS;
use radiotrope::stream::metadata::StreamMetadata;
use radiotrope::stream::{StreamCancel, StreamResolver, StreamType};
use radiotrope_app::data::levels::Levels;
use radiotrope_app::data::recordings;
use radiotrope_app::text::{self, status};

use super::state::{AppCommand, AppSnapshot, NoticeKind, RecordingNotice, RecordingProgress};

mod alarm_tone;
pub mod fallback;
pub mod play_report;
mod resume;
mod scheduling;

pub use scheduling::{clash_message, CoverSource};

/// How often the schedule's clock is read while something is scheduled
const SCHEDULE_POLL: Duration = Duration::from_secs(1);

/// How often a fade steps the volume
const FADE_STEP: Duration = Duration::from_millis(100);

/// Timeout for stream resolution — if the server doesn't respond within this
/// duration the resolve attempt is abandoned. The engine ends a resolve by
/// its own deadline with the reason; this is a little longer, as a backstop.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(RESOLVE_TIMEOUT_SECS + 3);

/// How often a recording's progress is read while one runs (the UI shows
/// it about as often)
const RECORDING_POLL: Duration = Duration::from_millis(200);

/// How long a station stepped to (Next, Previous) is shown before it is
/// connected to: stepping on within this time starts only the last one
const STEP_SETTLE: Duration = Duration::from_millis(300);

/// The status while the output device's driver is stuck in an open. Nothing
/// plays until it returns, and if it never does only a restart helps.
pub const OUTPUT_NOT_RESPONDING: &str =
    "Audio device not responding. Restart Radiotrope if this continues.";

/// The status of a station that plays nothing
const NO_AUDIO: &str = "The station sends no audio";

/// The error of a station whose threads the system didn't start
const CANT_START: &str = "Can't start the station";

pub struct AppController {
    cmd_rx: Receiver<AppCommand>,
    cmd_tx: Sender<AppCommand>,
    shared_state: Arc<Mutex<AppSnapshot>>,
    engine: Option<AudioEngine>,
    metadata_rx: Option<crossbeam_channel::Receiver<StreamMetadata>>,
    /// Monotonically increasing counter to discard stale resolve results
    resolve_generation: u64,
    /// Stops the station being resolved or played: its network threads end
    /// at once instead of finishing their requests for nobody
    stream_cancel: Option<StreamCancel>,
    /// The station the engine plays for us. Events about any other are an
    /// earlier station's last words.
    stream_id: Option<StreamId>,
    /// One-shot channel to send the engine's analysis Arc to the UI thread
    analysis_tx: Option<Sender<Arc<Mutex<AudioAnalysis>>>>,
    /// One-shot channel to send the engine's SharedStats to the UI thread
    stats_tx: Option<Sender<SharedStats>>,
    /// Saved volume level before mute (for restoring on unmute)
    volume_before_mute: f32,
    /// Sequence number of the last recording notice
    notice_seq: u64,
    /// The playing stream failed: the engine's Stopped that follows keeps the
    /// error showing instead of a plain "Stopped"
    stream_failed: bool,
    /// While the output device hangs: the status it covers, shown again
    /// once the device answers
    output_hang: Option<(std::borrow::Cow<'static, str>, bool)>,
    /// The scheduled entries
    schedule: Vec<radiotrope_app::data::schedule::Entry>,
    /// Where the entries are saved; `None` keeps them in memory (tests)
    schedule_path: Option<std::path::PathBuf>,
    /// How each entry's last run went, saved with the entries
    schedule_results: Vec<radiotrope_app::data::schedule::LastRun>,
    /// The scheduled run the running recording was started for
    recording_for: Option<scheduling::RunId>,
    /// The entry the schedule started, while it plays
    active: Option<scheduling::ActiveEntry>,
    sleep: Option<scheduling::SleepTimer>,
    fade_in: Option<scheduling::FadeIn>,
    /// A fading Stop whose fade the user ended by moving the volume
    stop_fade_skipped: Option<chrono::DateTime<chrono::Local>>,
    /// Gives a scheduled recording its cover art
    cover_source: Option<CoverSource>,
    /// The volume last given to the engine
    engine_volume: Option<f32>,
    /// A recording kept open while its station, which failed, is started
    /// again
    resume: Option<resume::Resume>,
    /// The station that was playing when its reader gave up (the network
    /// was gone for too long), while it is started again and again
    outage: Option<resume::Outage>,
    /// The station stepped to and when it is connected to, while it is
    /// only shown (see [`STEP_SETTLE`])
    settling: Option<(std::time::Instant, String)>,
    /// The time the outage's retries and a step's wait go by, when a test
    /// sets it
    #[cfg(test)]
    fake_now: Option<std::time::Instant>,
    /// How loud the stations played were, for volume matching
    levels: Levels,
    /// The station whose loudness the engine is measuring
    level_url: Option<String>,
    /// Counts a play in the directory (see [`play_report`])
    play_reporter: Option<play_report::PlayReporter>,
    /// The station started last, until it first plays: its play is
    /// counted then, once, and not again for a reconnect or retry
    unreported: Option<String>,
    /// The current station's streams: the one tried or playing, and the
    /// ones to fall back to (see [`fallback`])
    streams: fallback::StreamList,
    /// The backups that came with a Play or Step, until its station is
    /// made the current one
    new_backups: Option<Vec<String>>,
    /// The backups of a station started by its address alone (a scheduled
    /// entry): see [`fallback::favorite_backups`]
    backups_of: Option<fallback::BackupsOf>,
    /// Told the state after every change the controller makes (see
    /// [`AppController::with_observer`])
    observer: Option<StateObserver>,
}

/// Something that follows the player's state: called on the controller's
/// thread, with the state locked, so it only copies what it needs
pub type StateObserver = Box<dyn FnMut(&AppSnapshot) + Send>;

impl AppController {
    pub fn new(
        cmd_rx: Receiver<AppCommand>,
        cmd_tx: Sender<AppCommand>,
        shared_state: Arc<Mutex<AppSnapshot>>,
        analysis_tx: Sender<Arc<Mutex<AudioAnalysis>>>,
        stats_tx: Sender<SharedStats>,
    ) -> Self {
        Self {
            cmd_rx,
            cmd_tx,
            shared_state,
            engine: None,
            metadata_rx: None,
            resolve_generation: 0,
            stream_cancel: None,
            stream_id: None,
            analysis_tx: Some(analysis_tx),
            stats_tx: Some(stats_tx),
            volume_before_mute: 1.0,
            notice_seq: 0,
            stream_failed: false,
            output_hang: None,
            schedule: Vec::new(),
            schedule_path: None,
            schedule_results: Vec::new(),
            recording_for: None,
            active: None,
            sleep: None,
            fade_in: None,
            stop_fade_skipped: None,
            cover_source: None,
            engine_volume: None,
            resume: None,
            outage: None,
            settling: None,
            #[cfg(test)]
            fake_now: None,
            levels: Levels::default(),
            level_url: None,
            play_reporter: None,
            unreported: None,
            streams: fallback::StreamList::default(),
            new_backups: None,
            backups_of: None,
            observer: None,
        }
    }

    /// Tell `observer` the state once the controller starts and again each
    /// time it has handled something (a command, an engine event, song
    /// info, a tick of its own): every change the controller makes is
    /// followed by a call, with no timer of the observer's own
    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    pub fn with_observer(mut self, observer: StateObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Show the observer the state as it is now
    fn tell_observer(&mut self) {
        if let Some(observer) = self.observer.as_mut() {
            let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            observer(&state);
        }
    }

    /// Run the controller event loop (blocking, call from a dedicated thread)
    pub fn run(&mut self) {
        // Initialize the audio engine
        match AudioEngine::new() {
            Ok(engine) => {
                // Send analysis Arc to UI thread before storing engine
                if let Some(tx) = self.analysis_tx.take() {
                    let _ = tx.send(engine.analysis());
                }
                // Send shared stats Arc to UI thread
                if let Some(tx) = self.stats_tx.take() {
                    let _ = tx.send(engine.shared_stats());
                }
                // Phones reach the listening and the stream statistics
                // through the shared state, and the window and phones what
                // volume matching does
                {
                    let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    state.listen = Some(engine.listen());
                    state.level = Some(engine.level_readout());
                    state.hum = Some(engine.hum_readout());
                    state.stats = Some(engine.shared_stats());
                }
                self.engine = Some(engine);
                self.levels = Levels::load(unix_now());
            }
            Err(e) => {
                // Not a missing device (the engine waits for one): say why
                // nothing will play
                eprintln!("Failed to initialize audio engine: {e}");
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.status_text = format!("Can't start audio: {e}").into();
                state.is_error = true;
                return;
            }
        }

        // Sleep until a command, an engine event or song info comes; while
        // recording, also wake to read its progress
        let commands = self.cmd_rx.clone();
        let mut engine_events = match &self.engine {
            Some(engine) => engine.event_receiver().clone(),
            None => never(),
        };
        let recording_tick = tick(RECORDING_POLL);
        let schedule_tick = tick(SCHEDULE_POLL);
        let fade_tick = tick(FADE_STEP);
        let no_tick = never();
        let mut recording = false;
        let mut scheduled = self.schedule_armed();
        let mut fading = false;
        self.tell_observer();
        loop {
            let metadata = self.metadata_rx.clone().unwrap_or_else(never);
            // Wakes when the station an outage stopped is due again
            let outage_due = self.outage_due().map(at).unwrap_or_else(never);
            // Wakes when the station stepped to is to be connected to
            let step_due = self.step_due().map(at).unwrap_or_else(never);
            select! {
                recv(commands) -> cmd => match cmd {
                    Ok(cmd) => {
                        if self.handle_command(cmd) {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                recv(engine_events) -> event => match event {
                    Ok(event) => self.handle_engine_event(event),
                    // The engine's thread is gone
                    Err(_) => engine_events = never(),
                },
                recv(metadata) -> meta => match meta {
                    Ok(meta) => self.show_metadata(meta),
                    // The station's reader is gone
                    Err(_) => self.metadata_rx = None,
                },
                recv(if recording { &recording_tick } else { &no_tick }) -> _ => {}
                recv(if scheduled { &schedule_tick } else { &no_tick }) -> _ => {}
                recv(if fading { &fade_tick } else { &no_tick }) -> _ => {}
                recv(outage_due) -> _ => {}
                recv(step_due) -> _ => {}
            }
            self.connect_settled();
            self.retry_outage();
            if scheduled || self.schedule_armed() {
                let now = chrono::Local::now();
                self.run_schedule(now);
                fading = self.fading(&now);
            } else {
                fading = false;
            }
            scheduled = self.schedule_armed();
            recording = self.poll_recording();
            if recording {
                self.retry_resume();
            }
            self.keep_hang_showing();
            self.tell_observer();
        }

        // Finish any recording before the engine goes away
        self.stop_recording();
        self.remember_level();

        // Shutdown engine
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
        }
    }

    /// While the output device hangs, its message stays up: a Play or a
    /// station's end would otherwise cover it with a status nothing acts on
    fn keep_hang_showing(&mut self) {
        let Some(under) = self.output_hang.as_mut() else {
            return;
        };
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        if state.status_text != OUTPUT_NOT_RESPONDING {
            *under = (state.status_text.clone(), state.is_error);
            state.status_text = OUTPUT_NOT_RESPONDING.into();
            state.is_error = true;
        }
        // An agent waiting for its station hears why it doesn't start
        if state.last_error.is_none() {
            state.last_error = Some(OUTPUT_NOT_RESPONDING.to_string());
        }
    }

    /// Handle a single command. Returns true if the loop should exit.
    fn handle_command(&mut self, cmd: AppCommand) -> bool {
        match cmd {
            AppCommand::Shutdown => return true,

            AppCommand::Play {
                url,
                backups,
                name,
                logo_url,
                country,
                taken,
            } => {
                // The schedule's entry is over: the user picked a station
                self.release_schedule();
                self.new_backups = Some(backups);
                let seq = self.start_new(&url, name, logo_url, country);
                if let Some(taken) = taken {
                    // An agent that stopped waiting has dropped its end
                    let _ = taken.send(seq);
                }
            }

            AppCommand::Step {
                url,
                backups,
                name,
                logo_url,
                country,
            } => {
                // The schedule's entry is over: the user picked a station
                self.release_schedule();
                self.new_backups = Some(backups);
                self.count_start(&url);
                self.step_to(&url, name, logo_url, country);
            }
            AppCommand::Stop => {
                self.unreported = None;
                self.stopped_by_user();
                self.stop_playback();
            }
            AppCommand::SetSleepTimer { minutes, fade } => self.set_sleep_timer(minutes, fade),
            AppCommand::SetSleepFade(fade) => self.set_sleep_fade(fade),
            AppCommand::SaveScheduleEntry { entry, reply } => {
                self.save_schedule_entry(entry, reply)
            }
            AppCommand::RemoveScheduleEntry { id, reply } => self.remove_schedule_entry(id, reply),
            AppCommand::SetScheduleEntryEnabled { id, enabled, reply } => {
                self.set_schedule_entry_enabled(id, enabled, reply)
            }
            AppCommand::SetVolume(vol) => {
                // Keep NaN out of the shared state and the saved settings
                if !vol.is_finite() {
                    return false;
                }
                let vol = vol.clamp(0.0, 1.0);
                {
                    let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    state.volume = vol;
                    // Auto-unmute when volume is changed to a non-zero value
                    if state.is_muted && vol > 0.0 {
                        state.is_muted = false;
                    }
                }
                self.volume_set_by_user();
                self.apply_volume(&chrono::Local::now());
            }
            AppCommand::Mute => {
                {
                    let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    self.volume_before_mute = state.volume;
                    state.is_muted = true;
                }
                self.volume_set_by_user();
                self.apply_volume(&chrono::Local::now());
            }
            AppCommand::Unmute => {
                {
                    let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    state.is_muted = false;
                    state.volume = self.volume_before_mute;
                }
                self.volume_set_by_user();
                self.apply_volume(&chrono::Local::now());
            }
            AppCommand::SetEqBand { band, gain_db } => {
                if let Some(engine) = &self.engine {
                    engine.set_eq_band(band, gain_db);
                }
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                if band < 10 {
                    state.eq_gains[band] = gain_db;
                }
                state.eq_preset_name = None;
            }
            AppCommand::SetEqPreset(ref name) => {
                if let Some(preset) = radiotrope::audio::find_preset(name) {
                    // Each preset brings its own preamp, so it doesn't
                    // play louder than Flat
                    let preamp = preset.preamp_db();
                    if let Some(engine) = &self.engine {
                        engine.set_eq_gains(preset.gains, Some(preset.name.to_string()));
                        engine.set_eq_preamp(preamp);
                    }
                    let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    state.eq_gains = preset.gains;
                    state.eq_preset_name = Some(preset.name.to_string());
                    state.eq_preamp = preamp;
                    state.eq_preamp_moved = false;
                }
            }
            AppCommand::SetEqGains(gains) => {
                if let Some(engine) = &self.engine {
                    engine.set_eq_gains(gains, None);
                }
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.eq_gains = gains;
                state.eq_preset_name = None;
            }
            AppCommand::SetEqPreamp(db) => {
                if let Some(engine) = &self.engine {
                    engine.set_eq_preamp(db);
                }
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.eq_preamp = db;
                state.eq_preamp_moved = true;
            }
            AppCommand::SetEqEnabled(on) => {
                if let Some(engine) = &self.engine {
                    engine.set_eq_enabled(on);
                }
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.eq_enabled = on;
            }
            AppCommand::SetMatchVolume(on) => {
                if let Some(engine) = &self.engine {
                    engine.set_match_volume(on);
                }
                self.shared_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .match_volume = on;
            }
            AppCommand::SetLevelTarget(target) => {
                if let Some(engine) = &self.engine {
                    engine.set_level_target(target);
                }
                self.shared_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .level_target = target;
            }
            AppCommand::SetHissReducer(level) => {
                if let Some(engine) = &self.engine {
                    engine.set_hiss(level);
                }
                self.shared_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .hiss = level;
            }
            AppCommand::SetHumFilter(on) => {
                if let Some(engine) = &self.engine {
                    engine.set_hum_filter(on);
                }
                self.shared_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .hum_filter = on;
            }
            AppCommand::SetRumbleFilter(on) => {
                if let Some(engine) = &self.engine {
                    engine.set_rumble_filter(on);
                }
                self.shared_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rumble_filter = on;
            }
            AppCommand::SetSmoothSwitching(on) => {
                if let Some(engine) = &self.engine {
                    engine.set_smooth_switching(on);
                }
                self.shared_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .smooth_switching = on;
            }
            AppCommand::StartRecording {
                folder,
                format,
                bitrate,
                with_eq,
                cover,
            } => {
                self.start_recording(&folder, format, bitrate, with_eq, cover);
            }
            AppCommand::StopRecording => {
                // A scheduled recording stopped by hand is over
                if self.active.is_some() {
                    self.release_schedule();
                }
                self.stop_recording();
            }
            AppCommand::InternalStreamResolved { generation, result } => {
                self.handle_stream_resolved(generation, result);
            }
        }
        false
    }

    /// Keep what the station playing measured as for next time
    fn remember_level(&mut self) {
        let Some(url) = self.level_url.take() else {
            return;
        };
        let Some(lufs) = self
            .engine
            .as_ref()
            .and_then(|engine| engine.level_readout().loudness_lufs())
        else {
            return;
        };
        self.levels.remember(&url, lufs, unix_now());
    }

    /// Stop the station, and the recording of it
    fn stop_playback(&mut self) {
        // Nothing starts it again
        self.outage = None;
        // Before the engine stops measuring it
        self.remember_level();
        self.stop_recording();
        self.cancel_stream();
        if let Some(engine) = &self.engine {
            engine.stop();
        }
        self.metadata_rx = None;
        self.stream_failed = false;
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        // A station still resolving doesn't start after the stop
        state.is_resolving = false;
        state.is_connecting = false;
        let was_beeping = std::mem::take(&mut state.alarm_beep);
        state.playback = PlaybackState::Stopped;
        state.status_text = status::STOPPED.into();
        state.is_error = false;
        state.title.clear();
        state.artist.clear();
        drop(state);
        // The beep's volume floor is gone
        if was_beeping {
            self.apply_volume(&chrono::Local::now());
        }
    }

    /// Find the backups of a station started by its address alone (see
    /// [`fallback::favorite_backups`])
    pub fn with_backups_of(mut self, backups_of: fallback::BackupsOf) -> Self {
        self.backups_of = Some(backups_of);
        self
    }

    /// The backups of the station at `url`, as far as is known
    pub(super) fn backups_of(&self, url: &str) -> Vec<String> {
        self.backups_of
            .as_ref()
            .map(|backups_of| backups_of(url))
            .unwrap_or_default()
    }

    /// Count a play in the directory with `reporter` (see [`play_report`])
    pub fn with_play_reporter(mut self, reporter: play_report::PlayReporter) -> Self {
        self.play_reporter = Some(reporter);
        self
    }

    /// A station is started anew (not reconnected, retried or kept for a
    /// recording waiting for it): its play is counted once it plays
    pub(super) fn count_start(&mut self, url: &str) {
        self.unreported = (!self.resumes(url)).then(|| url.to_string());
    }

    /// [`Self::start_stream`] for a station started anew: its play is
    /// counted once it plays
    pub(super) fn start_new(
        &mut self,
        url: &str,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
    ) -> u64 {
        self.count_start(url);
        self.start_stream(url, name, logo_url, country)
    }

    /// The station started last first plays: its play is counted
    fn report_play(&mut self, url: Option<String>) {
        let Some(url) = url.filter(|url| self.unreported.as_deref() == Some(url.as_str())) else {
            return;
        };
        self.unreported = None;
        if let Some(reporter) = &self.play_reporter {
            reporter(&url);
        }
    }

    /// Play a station, ending the recording of the one before.
    /// Returns the station's `play_seq`.
    fn start_stream(
        &mut self,
        url: &str,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
    ) -> u64 {
        // A station picked takes over from one an outage stopped
        self.outage = None;
        // Playing the station a recording waits for keeps the recording
        if self.resumes(url) {
            return self.resume_station_now();
        }
        // Switching station ends the recording of the old one
        self.stop_recording();
        self.open_stream(url, name, logo_url, country)
    }

    /// Step to a station (Next, Previous): it is the current one at once,
    /// with the one before stopped, and is connected to [`STEP_SETTLE`]
    /// later. A step, a Play or a Stop in between takes its place, so
    /// stepping past stations connects to none of them.
    /// Returns the station's `play_seq`.
    fn step_to(
        &mut self,
        url: &str,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
    ) -> u64 {
        self.outage = None;
        // A recording waits for this very station: nothing to step past
        if self.resumes(url) {
            return self.resume_station_now();
        }
        self.stop_recording();
        let seq = self.show_stream(url, name, logo_url, country);
        self.settling = Some((self.now() + STEP_SETTLE, url.to_string()));
        seq
    }

    /// When the station stepped to is connected to
    fn step_due(&self) -> Option<std::time::Instant> {
        self.settling.as_ref().map(|(due, _)| *due)
    }

    /// Connect to the station stepped to, once nothing took its place for
    /// [`STEP_SETTLE`]
    fn connect_settled(&mut self) {
        let now = self.now();
        if !self.step_due().is_some_and(|due| now >= due) {
            return;
        }
        if self.settling.take().is_some() {
            self.connect_current();
        }
    }

    /// Make a station the current one and connect to it.
    /// Returns the station's `play_seq`. A running recording carries on
    /// with the new stream's audio.
    fn open_stream(
        &mut self,
        url: &str,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
    ) -> u64 {
        let seq = self.show_stream(url, name, logo_url, country);
        self.connect_current();
        seq
    }

    /// Make a station the current one, as connecting, with the one before
    /// stopped: [`Self::connect`] then connects to it.
    /// Returns the station's `play_seq`.
    ///
    /// Each call increments `resolve_generation`; stale results from earlier
    /// calls are discarded in `handle_stream_resolved`.
    fn show_stream(
        &mut self,
        url: &str,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
    ) -> u64 {
        // Stop any current playback first, and any station still resolving
        self.remember_level();
        self.cancel_stream();
        if let Some(engine) = &self.engine {
            engine.stop();
        }
        self.metadata_rx = None;

        self.stream_failed = false;

        // A station picked comes with its streams, and starts with its
        // default. One started again (a reconnect, a retry) keeps the
        // streams it has and starts with the one that last played.
        match self.new_backups.take() {
            Some(backups) => self.streams = fallback::StreamList::new(url, backups),
            None if self.streams.is_station(url) => self.streams.restart(),
            None => self.streams = fallback::StreamList::new(url, Vec::new()),
        }

        {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            state.play_seq += 1;
            state.station_url = Some(url.to_string());
            fallback::show(&self.streams, &mut state);
            state.station_name = name;
            state.station_logo_url = logo_url.filter(|logo| !logo.is_empty());
            state.station_country = country.filter(|country| !country.is_empty());
            state.title.clear();
            state.artist.clear();
            state.last_error = None;
            state.is_resolving = true;
            state.is_connecting = false;
            state.alarm_beep = false;
            state.codec_name.clear();
            state.stream_type.clear();
            state.sample_rate = 0;
            state.channels = 0;
            state.bitrate = None;
            state.status_text = status::CONNECTING.into();
            state.is_error = false;
            // The old station was stopped above. Its own Stopped is ignored
            // like the rest of its events.
            state.playback = PlaybackState::Stopped;
            state.play_seq
        }
    }

    /// Connect to the current station's stream: its default, or the
    /// backup whose turn it is
    fn connect_current(&mut self) {
        let url = self.streams.current().to_string();
        self.connect(&url);
    }

    /// The stream being tried didn't connect: try the station's next one.
    /// False when there is none (or the stream had played already), and
    /// the failure is the station's.
    fn try_next_stream(&mut self, error: &str) -> bool {
        let failed = self.streams.current().to_string();
        let Some(next) = self.streams.next_stream().map(str::to_string) else {
            return false;
        };
        eprintln!("Stream {failed} didn't connect ({error}); trying {next}");
        // The failed stream's last words (its Stopped) change nothing
        let handed_over = self.stream_id.is_some();
        let settling = self.settling.take();
        self.cancel_stream();
        self.settling = settling;
        // The engine lets go of a stream it was given
        if let Some(engine) = self.engine.as_ref().filter(|_| handed_over) {
            engine.stop();
        }
        self.metadata_rx = None;
        self.stream_failed = false;
        {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            fallback::show(&self.streams, &mut state);
            state.is_resolving = true;
            state.is_connecting = false;
            state.codec_name.clear();
            state.stream_type.clear();
            state.sample_rate = 0;
            state.channels = 0;
            state.bitrate = None;
            // Started again after an outage, it is still reconnecting
            state.status_text = if self.outage.is_some() {
                resume::RECONNECTING.into()
            } else {
                status::CONNECTING.into()
            };
            state.is_error = false;
        }
        self.connect(&next);
        true
    }

    /// Resolve a stream of the current station on a worker thread, which
    /// sends the result back
    fn connect(&mut self, url: &str) {
        let generation = self.resolve_generation;
        let cancel = StreamCancel::new();
        self.stream_cancel = Some(cancel.clone());

        let url: Arc<str> = Arc::from(url);
        let cmd_tx = self.cmd_tx.clone();

        let spawned = std::thread::Builder::new()
            .name("stream-resolve".into())
            .spawn(move || {
                // Run the actual resolve on a nested thread so we can enforce a timeout
                let (tx, rx) = crossbeam_channel::bounded(1);
                let url_inner = Arc::clone(&url);
                let cancel_inner = cancel.clone();
                let inner = std::thread::Builder::new()
                    .name("stream-resolve-inner".into())
                    .spawn(move || {
                        let result = StreamResolver::resolve_cancellable(&url_inner, &cancel_inner)
                            .map_err(|e| e.to_string());
                        let _ = tx.send(result);
                    });

                let result = match inner {
                    Ok(_) => match rx.recv_timeout(RESOLVE_TIMEOUT) {
                        Ok(r) => r,
                        Err(_) => {
                            // Stop the resolve, which would otherwise carry on
                            cancel.cancel();
                            Err(format!(
                                "Stream resolution timed out after {}s for: {url}",
                                RESOLVE_TIMEOUT.as_secs()
                            ))
                        }
                    },
                    // The system is out of threads: the station fails, the
                    // player carries on
                    Err(e) => {
                        eprintln!("Can't start the stream-resolve-inner thread: {e}");
                        Err(CANT_START.to_string())
                    }
                };

                let _ = cmd_tx.send(AppCommand::InternalStreamResolved { generation, result });
            });
        if let Err(e) = spawned {
            eprintln!("Can't start the stream-resolve thread: {e}");
            self.handle_stream_resolved(generation, Err(CANT_START.to_string()));
        }
    }

    /// Stop the station being resolved or played, and make any resolve
    /// result still on its way stale, and any event about the station
    fn cancel_stream(&mut self) {
        if let Some(cancel) = self.stream_cancel.take() {
            cancel.cancel();
        }
        self.resolve_generation += 1;
        self.stream_id = None;
        // A station stepped to and not connected to yet never is
        self.settling = None;
    }

    /// Handle the resolved stream — start playback (or store error).
    ///
    /// Results with a stale `generation` are silently discarded.
    fn handle_stream_resolved(
        &mut self,
        generation: u64,
        result: Result<radiotrope::stream::ResolvedStream, String>,
    ) {
        if generation != self.resolve_generation {
            // A newer Play was issued while this resolve was in flight — discard.
            return;
        }

        // This resolve is current — clear the resolving flag regardless of outcome.
        match result {
            Ok(resolved) => {
                let url = {
                    let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    // Keep API name from Play command; fall back to stream-provided name
                    if state.station_name.is_none() {
                        state.station_name = resolved.info.station_name.clone();
                    }
                    state.is_resolving = false;
                    state.stream_type = match resolved.info.stream_type {
                        StreamType::Direct => "ICY".to_string(),
                        StreamType::Hls => "HLS".to_string(),
                    };
                    state.bitrate = resolved.info.bitrate;
                    // Started again after an outage, it is still reconnecting
                    state.status_text = if self.outage.is_some() {
                        resume::RECONNECTING.into()
                    } else {
                        status::CONNECTING.into()
                    };
                    state.is_error = false;
                    state.is_connecting = true;
                    state.station_url.clone().unwrap_or_default()
                };

                // Store metadata receiver for polling
                let mut resolved = resolved;
                self.metadata_rx = resolved.metadata_rx.take();

                // Start playback. The engine cancels the stream when it stops.
                if let Some(engine) = &self.engine {
                    // A station heard before starts at its volume
                    engine.set_start_loudness(self.levels.get(&url));
                    self.stream_id = Some(engine.play_stream(resolved));
                    self.level_url = Some(url);
                    // The stream is the engine's now: cancelling it here on
                    // a stop or a switch would cut the station off halfway
                    // through Smooth Switching's fade-out
                    self.stream_cancel = None;
                }
            }
            Err(e) => {
                eprintln!("Stream resolution failed: {e}");
                // The station's next stream may still connect
                if self.try_next_stream(&e) {
                    return;
                }
                {
                    let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    state.playback = PlaybackState::Stopped;
                    // The station stays the current one, showing its error: the
                    // window shows the station it has, so clearing it left the
                    // error on the one played before (a favorite, say)
                    state.last_error = Some(e.clone());
                    state.is_resolving = false;
                    state.status_text = text::playback_error(&e).into();
                    state.is_error = true;
                }
                // A recording waiting for the station tries again
                self.resume_failed(&e);
                // And so does a station an outage stopped
                self.outage_try_failed();
            }
        }
    }

    fn handle_engine_event(&mut self, event: EngineEvent) {
        // An earlier station's last words: it was stopped when the station
        // changed, and nothing it says applies to the one playing now.
        // Events about no station (a device lost while stopped) apply.
        if event.stream.is_some() && event.stream != self.stream_id {
            return;
        }
        let about_station = event.stream.is_some();
        let event = event.event;

        // A stream that fails before it plays gives way to the station's
        // next one; the failure counts only when the last has failed too
        match &event {
            AudioEvent::Error(e) if about_station && self.try_next_stream(e) => return,
            AudioEvent::NoAudioTimeout if about_station && self.try_next_stream(NO_AUDIO) => return,
            _ => {}
        }

        // The stream ended or failed: a recording waits for the station to
        // be started again, or keeps what was recorded
        self.recording_station_event(&event);
        // The station's reader gave up: it is started again until it plays
        self.outage_station_event(&event);

        // The output was gone so long that the station would play minutes
        // behind live: start it again, to catch up
        if matches!(event, AudioEvent::FellBehind) {
            let station = {
                let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.station_url.clone().map(|url| {
                    (
                        url,
                        state.station_name.clone(),
                        state.station_logo_url.clone(),
                        state.station_country.clone(),
                    )
                })
            };
            if let Some((url, name, logo_url, country)) = station {
                self.start_stream(&url, name.clone(), logo_url.clone(), country.clone());
                // With the network still away (a laptop just woken), the
                // start fails: it is tried again like after an outage
                self.expect_outage(url, name, logo_url, country);
            }
            return;
        }

        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        // While a new station resolves, news of the device waits: the
        // engine repeats a lost device once the station plays. A hung
        // device isn't repeated, so it shows at once.
        if state.is_resolving
            && !matches!(
                event,
                AudioEvent::OutputNotResponding | AudioEvent::OutputResponding
            )
        {
            return;
        }
        match event {
            AudioEvent::Playing(codec_info) => {
                state.playback = PlaybackState::Playing;
                state.is_connecting = false;
                state.codec_name = codec_info.codec_name;
                state.channels = codec_info.channels;
                state.sample_rate = codec_info.sample_rate;
                // Prefer ICY/HLS bitrate (already stored), fall back to codec-detected
                if state.bitrate.is_none() {
                    state.bitrate = codec_info.bitrate;
                }
                state.status_text = status::PLAYING.into();
                state.is_error = false;
                self.stream_failed = false;
                // This is the stream a reconnect starts with
                self.streams.played();
                let url = state.station_url.clone();
                drop(state);
                self.report_play(url);
                // An alarm's fade starts once it is heard
                self.entry_station_plays();
                self.apply_volume(&chrono::Local::now());
            }
            // AAC and AAC+, or a chained Ogg stream's next codec
            AudioEvent::CodecChanged(codec_info) => {
                state.codec_name = codec_info.codec_name;
            }
            AudioEvent::Stopped => {
                state.playback = PlaybackState::Stopped;
                state.is_connecting = false;
                // A stream that failed stops with its error still showing
                if !std::mem::take(&mut self.stream_failed) {
                    state.status_text = status::STOPPED.into();
                    state.is_error = false;
                }
                // A recording that waits for the station says so, and a
                // station that is started again after an outage
                if self.resume.is_some() || self.outage.is_some() {
                    state.status_text = resume::RECONNECTING.into();
                    state.is_error = false;
                }
                // Not heard yet, and on its way: a phone shows it starting
                if self.outage.is_some() {
                    state.is_connecting = true;
                }
            }
            AudioEvent::Paused => {
                state.playback = PlaybackState::Paused;
                state.is_connecting = false;
                state.status_text = status::PAUSED.into();
                state.is_error = false;
                self.stream_failed = false;
            }
            AudioEvent::Resumed => {
                state.playback = PlaybackState::Playing;
                state.status_text = status::PLAYING.into();
                state.is_error = false;
                self.stream_failed = false;
            }
            AudioEvent::Error(ref e) => {
                eprintln!("Engine error: {e}");
                state.last_error = Some(e.clone());
                state.status_text = text::playback_error(e).into();
                state.is_error = true;
                state.is_connecting = false;
                self.stream_failed = true;
                // The error is for agents and the log: the station is
                // started again
                if self.outage.is_some() {
                    state.status_text = resume::RECONNECTING.into();
                    state.is_error = false;
                    state.is_connecting = true;
                }
            }
            AudioEvent::Buffering(pct) => {
                state.status_text = if pct < 100 {
                    format!("Buffering {}%", pct).into()
                } else {
                    status::PLAYING.into()
                };
                state.is_error = false;
            }
            AudioEvent::StreamStalled => {
                state.status_text = status::WAITING_FOR_DATA.into();
                state.is_error = true;
            }
            AudioEvent::StreamRecovered => {
                state.status_text = status::PLAYING.into();
                state.is_error = false;
            }
            AudioEvent::OutputLost => {
                // The station stays loaded, and so does a recording
                state.status_text = "Audio output lost. Waiting for a device\u{2026}".into();
                state.is_error = true;
            }
            AudioEvent::OutputNotResponding => {
                self.output_hang = Some((state.status_text.clone(), state.is_error));
                state.status_text = OUTPUT_NOT_RESPONDING.into();
                state.is_error = true;
                state.last_error = Some(OUTPUT_NOT_RESPONDING.to_string());
            }
            AudioEvent::OutputResponding => {
                if let Some((status, is_error)) = self.output_hang.take() {
                    if state.status_text == OUTPUT_NOT_RESPONDING {
                        state.status_text = status;
                        state.is_error = is_error;
                    }
                    if state.last_error.as_deref() == Some(OUTPUT_NOT_RESPONDING) {
                        state.last_error = None;
                    }
                }
            }
            AudioEvent::OutputRestored => {
                state.status_text = match state.playback {
                    PlaybackState::Playing => status::PLAYING,
                    PlaybackState::Paused => status::PAUSED,
                    PlaybackState::Stopped => status::STOPPED,
                }
                .into();
                state.is_error = false;
            }
            AudioEvent::ProbeTimeout => {
                state.status_text = "Station not responding".into();
                state.is_error = true;
            }
            AudioEvent::NoAudioTimeout => {
                state.status_text = NO_AUDIO.into();
                state.is_error = true;
                state.is_connecting = false;
                self.stream_failed = true;
                if self.outage.is_some() {
                    state.status_text = resume::RECONNECTING.into();
                    state.is_error = false;
                    state.is_connecting = true;
                }
            }
            // Handled above
            AudioEvent::FellBehind => {}
        }
    }

    /// Start recording the playing station into `folder`, and say so
    /// when it can't be
    fn start_recording(
        &mut self,
        folder: &Path,
        format: RecordingFormat,
        bitrate: Option<u32>,
        with_eq: bool,
        cover: Option<Vec<u8>>,
    ) {
        if let Err(e) = self.try_start_recording(folder, format, bitrate, with_eq, cover) {
            self.notify(&e, NoticeKind::Error);
        }
    }

    /// Start recording the playing station into `folder`, or say why not
    /// (a sentence for the user). Nothing to do is fine: no audio engine
    /// yet, or a recording already running.
    fn try_start_recording(
        &mut self,
        folder: &Path,
        format: RecordingFormat,
        bitrate: Option<u32>,
        with_eq: bool,
        cover: Option<Vec<u8>>,
    ) -> Result<(), String> {
        let Some(engine) = &self.engine else {
            return Ok(());
        };
        let recorder = engine.recorder();
        if recorder.is_recording() {
            return Ok(());
        }

        let (station, url, playing, station_kbps) = {
            let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.station_name.clone().unwrap_or_default(),
                state.station_url.clone().unwrap_or_default(),
                state.playback == PlaybackState::Playing,
                state.bitrate,
            )
        };
        if !playing {
            return Err("Start a station before recording".into());
        }
        recordings::prepare_dir(folder)?;

        let now = chrono::Local::now();
        let path = recordings::new_file_path(folder, &station, now, format.extension());
        let station_name = if station.is_empty() {
            "Radio".to_string()
        } else {
            station
        };
        let options = RecordingOptions {
            path: path.clone(),
            format,
            bitrate_kbps: recording_kbps(bitrate, station_kbps),
            tap: if with_eq {
                TapPoint::AfterEq
            } else {
                TapPoint::BeforeEq
            },
            tags: RecordingTags {
                title: format!("{station_name}, {}", now.format("%Y-%m-%d %H:%M")),
                artist: station_name,
                album: "Radiotrope recordings".to_string(),
                comment: url,
                cover,
            },
        };

        match recorder.start(options) {
            Ok(()) => {
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.recording = Some(RecordingProgress {
                    path,
                    duration: Duration::ZERO,
                    bytes: 0,
                });
                Ok(())
            }
            Err(e) => Err(format!("Can't start recording: {e}")),
        }
    }

    /// Stop the running recording, if any, and say where it was saved.
    fn stop_recording(&mut self) {
        self.stop_recording_with(None);
    }

    /// Stop the running recording, if any, and say where it was saved, and
    /// why it stopped when that is `reason` rather than the user
    fn stop_recording_with(&mut self, reason: Option<String>) {
        // Nothing waits for the station any more
        self.end_resume();
        let Some(engine) = &self.engine else { return };
        let Some(mut status) = engine.recorder().stop() else {
            return;
        };
        if status.error.is_none() {
            status.error = reason;
        }
        self.shared_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recording = None;
        self.report_finished(&status);
    }

    /// Mirror the recording's progress into the shared state, and stop it
    /// if writing failed (e.g. the disk is full). Returns whether one is
    /// still running.
    fn poll_recording(&mut self) -> bool {
        let Some(engine) = &self.engine else {
            return false;
        };
        let Some(status) = engine.recorder().status() else {
            return false;
        };
        if status.error.is_some() {
            self.stop_recording();
            return false;
        }
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.recording = Some(RecordingProgress {
            path: status.path,
            duration: status.duration,
            bytes: status.bytes_written,
        });
        true
    }

    fn report_finished(&mut self, status: &RecordingStatus) {
        let name = status
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match &status.error {
            Some(e) => self.notify(
                &format!("Recording stopped: {e}. Saved {name}"),
                NoticeKind::Error,
            ),
            None => self.notify(&format!("Saved {name}"), NoticeKind::Saved),
        }
        // A scheduled recording: its entry's row says how it went
        self.scheduled_recording_finished(status);
    }

    /// Tell the user `text` under the station. The kind says what it is
    /// about: its icon, what a click does, and whether it stays until closed.
    fn notify(&mut self, text: &str, kind: NoticeKind) {
        self.notice_seq += 1;
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.recording_notice = Some(RecordingNotice::new(self.notice_seq, text, kind));
    }

    /// Show the station's song info: `meta`, or a newer one already waiting
    fn show_metadata(&mut self, mut meta: StreamMetadata) {
        if let Some(rx) = &self.metadata_rx {
            while let Ok(newer) = rx.try_recv() {
                meta = newer;
            }
        }
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(title) = meta.title {
            state.title = title;
        }
        if let Some(artist) = meta.artist {
            state.artist = artist;
        }
    }
}
/// Bitrate used when Auto can't find the station's bitrate, in kbps.
const AUTO_FALLBACK_KBPS: u32 = 256;

/// Seconds since 1970, for the station levels' dates
fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The bitrate to record at: the chosen one, or for Auto the station's own
/// (256 kbps when unknown), kept within 32–320 kbps.
fn recording_kbps(chosen: Option<u32>, station: Option<u32>) -> u32 {
    chosen
        .or(station.filter(|k| *k > 0))
        .unwrap_or(AUTO_FALLBACK_KBPS)
        .clamp(32, 320)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller_and_state() -> (AppController, Arc<Mutex<AppSnapshot>>) {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (analysis_tx, _) = crossbeam_channel::unbounded();
        let (stats_tx, _) = crossbeam_channel::unbounded();
        let state = Arc::new(Mutex::new(AppSnapshot::default()));
        let controller = AppController::new(cmd_rx, cmd_tx, state.clone(), analysis_tx, stats_tx);
        (controller, state)
    }

    mod equalizer_at_startup {
        use super::*;
        use radiotrope_app::data::settings::Settings;

        /// The equalizer once the startup commands for `settings` are handled
        fn restored(settings: &Settings) -> AppSnapshot {
            let (mut controller, state) = controller_and_state();
            for command in AppCommand::restore_eq(settings) {
                controller.handle_command(command);
            }
            let state = state.lock().unwrap();
            state.clone()
        }

        #[test]
        fn a_saved_preset_gets_its_own_preamp() {
            let rock = radiotrope::audio::find_preset("Rock").unwrap();
            assert!(rock.preamp_db() < 0.0);
            // Kept by a build where Rock had other gains and no preamp
            let settings = Settings {
                eq_enabled: true,
                eq_preset_name: Some("Rock".into()),
                eq_gains: [3.0; 10],
                eq_preamp: 0.0,
                ..Default::default()
            };
            let state = restored(&settings);
            assert!(state.eq_enabled);
            assert_eq!(state.eq_preset_name.as_deref(), Some("Rock"));
            assert_eq!(state.eq_gains, rock.gains);
            assert_eq!(state.eq_preamp, rock.preamp_db());
        }

        #[test]
        fn a_preamp_moved_on_a_preset_is_kept() {
            let (mut controller, state) = controller_and_state();
            controller.handle_command(AppCommand::SetEqPreset("Rock".into()));
            assert!(!state.lock().unwrap().eq_preamp_moved);
            controller.handle_command(AppCommand::SetEqPreamp(-1.5));
            // What the window saves on the way out
            let s = state.lock().unwrap().clone();
            assert!(s.eq_preamp_moved);
            let settings = Settings {
                eq_enabled: true,
                eq_preset_name: s.eq_preset_name,
                eq_gains: s.eq_gains,
                eq_preamp: s.eq_preamp,
                eq_preamp_moved: s.eq_preamp_moved,
                ..Default::default()
            };

            let state = restored(&settings);
            assert_eq!(state.eq_preset_name.as_deref(), Some("Rock"));
            assert_eq!(state.eq_preamp, -1.5);
            assert!(state.eq_preamp_moved);

            // Picking the preset again brings its own preamp back
            let (mut controller, state) = controller_and_state();
            for command in AppCommand::restore_eq(&settings) {
                controller.handle_command(command);
            }
            controller.handle_command(AppCommand::SetEqPreset("Rock".into()));
            let state = state.lock().unwrap();
            let rock = radiotrope::audio::find_preset("Rock").unwrap();
            assert_eq!(state.eq_preamp, rock.preamp_db());
            assert!(!state.eq_preamp_moved);
        }

        #[test]
        fn custom_gains_get_the_saved_preamp() {
            let gains = [2.0, 1.0, 0.0, -1.0, 0.0, 0.0, 1.5, 0.0, 0.0, -2.0];
            let settings = Settings {
                eq_preset_name: None,
                eq_gains: gains,
                eq_preamp: -4.5,
                ..Default::default()
            };
            let state = restored(&settings);
            assert!(!state.eq_enabled);
            assert_eq!(state.eq_preset_name, None);
            assert_eq!(state.eq_gains, gains);
            assert_eq!(state.eq_preamp, -4.5);
        }
    }

    mod output_device {
        use super::*;

        #[test]
        fn a_lost_output_shows_until_a_device_is_back() {
            let (mut controller, state) = controller_and_state();
            controller.stream_id = Some(StreamId(1));
            controller.handle_engine_event(on(1, playing()));
            controller.handle_engine_event(on(1, AudioEvent::OutputLost));
            {
                let state = state.lock().unwrap();
                assert!(state.is_error);
                assert!(state.status_text.contains("output lost"));
                // The station is still loaded
                assert_eq!(state.playback, PlaybackState::Playing);
            }

            controller.handle_engine_event(on(1, AudioEvent::OutputRestored));
            let state = state.lock().unwrap();
            assert!(!state.is_error);
            assert_eq!(state.status_text, "Playing");
        }

        #[test]
        fn a_device_back_while_paused_shows_paused() {
            let (mut controller, state) = controller_and_state();
            controller.stream_id = Some(StreamId(1));
            controller.handle_engine_event(on(1, playing()));
            controller.handle_engine_event(on(1, AudioEvent::OutputLost));
            controller.handle_engine_event(on(1, AudioEvent::Paused));
            controller.handle_engine_event(on(1, AudioEvent::OutputRestored));

            let state = state.lock().unwrap();
            assert!(!state.is_error);
            assert_eq!(state.status_text, "Paused");
        }
    }

    mod stream_cancel {
        use super::*;
        use radiotrope_app::data::settings::Settings;
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        /// A station that answers and then sends no audio
        fn silent_station() -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/live", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    std::thread::spawn(move || {
                        let mut request = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        while request.read_line(&mut line).is_ok_and(|n| n > 2) {
                            line.clear();
                        }
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\n\r\n");
                        std::thread::sleep(Duration::from_secs(60));
                    });
                }
            });
            url
        }

        /// The result of the resolve that was cancelled. Unless the cancel
        /// reaches it, a silent station's resolve runs to the 15 s timeout.
        fn cancelled_resolve(controller: &AppController) -> AppCommand {
            let cmd = controller
                .cmd_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("the cancelled resolve must end at once");
            assert!(matches!(cmd, AppCommand::InternalStreamResolved { .. }));
            cmd
        }

        #[test]
        fn a_station_that_fails_to_resolve_stays_the_current_one() {
            let (mut controller, state) = controller();
            // Nothing listens there, so the resolve fails at once
            let url = "http://127.0.0.1:1/dead.mp3";
            controller.start_stream(url, Some("Dead FM".into()), None, None);
            let resolved = controller
                .cmd_rx
                .recv_timeout(Duration::from_secs(20))
                .expect("the resolve must fail");
            assert!(matches!(
                resolved,
                AppCommand::InternalStreamResolved { .. }
            ));
            controller.handle_command(resolved);

            let state = state.lock().unwrap();
            assert!(state.is_error, "{}", state.status_text);
            assert_eq!(state.playback, PlaybackState::Stopped);
            assert_eq!(state.station_url.as_deref(), Some(url));
            assert_eq!(state.station_name.as_deref(), Some("Dead FM"));
        }

        #[test]
        fn a_nan_volume_is_ignored() {
            let (mut controller, state) = controller();
            controller.handle_command(AppCommand::SetVolume(0.4));
            controller.handle_command(AppCommand::SetVolume(f32::NAN));
            assert_eq!(state.lock().unwrap().volume, 0.4);
        }

        #[test]
        fn the_filter_settings_are_restored() {
            let (mut controller, state) = controller();
            let settings = Settings {
                match_volume: false,
                match_level: "loud".into(),
                hiss_reducer: "strong".into(),
                hum_filter: false,
                rumble_filter: false,
                smooth_switching: false,
                ..Settings::default()
            };
            for command in AppCommand::restore_sound(&settings) {
                controller.handle_command(command);
            }
            let s = state.lock().unwrap();
            assert!(!s.match_volume);
            assert_eq!(s.level_target, radiotrope::audio::LevelTarget::Loud);
            assert_eq!(s.hiss, radiotrope::audio::HissLevel::Strong);
            assert!(!s.hum_filter && !s.rumble_filter && !s.smooth_switching);
        }

        #[test]
        fn an_unknown_level_gets_the_default() {
            let (mut controller, state) = controller();
            let settings = Settings {
                match_level: "deafening".into(),
                ..Settings::default()
            };
            for command in AppCommand::restore_sound(&settings) {
                controller.handle_command(command);
            }
            let s = state.lock().unwrap();
            assert!(s.match_volume);
            assert_eq!(s.level_target, radiotrope::audio::LevelTarget::Normal);
        }

        #[test]
        fn a_stop_while_resolving_ends_the_resolve_and_stays_stopped() {
            let (mut controller, state) = controller();
            controller.start_stream(&silent_station(), Some("Silent FM".into()), None, None);
            let cancel = controller.stream_cancel.clone().unwrap();
            assert!(state.lock().unwrap().is_resolving);

            controller.handle_command(AppCommand::Stop);
            assert!(cancel.is_cancelled());
            assert!(!state.lock().unwrap().is_resolving);

            // The resolve's result comes after the stop and changes nothing
            let resolved = cancelled_resolve(&controller);
            controller.handle_command(resolved);
            let state = state.lock().unwrap();
            assert_eq!(state.playback, PlaybackState::Stopped);
            assert!(!state.is_resolving);
            assert!(!state.is_error, "{}", state.status_text);
            assert_eq!(state.status_text, "Stopped");
        }

        #[test]
        fn a_play_tells_its_sender_which_station_it_became() {
            let (mut controller, state) = controller();
            // The window starts one first: it asks for nothing back
            controller.handle_command(AppCommand::Play {
                url: silent_station(),
                backups: Vec::new(),
                name: None,
                logo_url: None,
                country: None,
                taken: None,
            });
            let (taken, mut seq) = tokio::sync::oneshot::channel();
            controller.handle_command(AppCommand::Play {
                url: silent_station(),
                backups: Vec::new(),
                name: Some("Agent FM".into()),
                logo_url: None,
                country: None,
                taken: Some(taken),
            });
            let state = state.lock().unwrap();
            assert_eq!(seq.try_recv().unwrap(), state.play_seq);
            assert_eq!(state.play_seq, 2);
            assert_eq!(state.station_name.as_deref(), Some("Agent FM"));
        }

        #[test]
        fn a_play_carries_its_logo_and_country() {
            let (mut controller, state) = controller();
            controller.handle_command(AppCommand::Play {
                url: silent_station(),
                backups: Vec::new(),
                name: Some("Silent FM".into()),
                logo_url: Some("http://silent.test/logo.png".into()),
                country: Some("Greece".into()),
                taken: None,
            });
            {
                let state = state.lock().unwrap();
                assert_eq!(
                    state.station_logo_url.as_deref(),
                    Some("http://silent.test/logo.png")
                );
                assert_eq!(state.station_country.as_deref(), Some("Greece"));
            }
            // The next station without one doesn't keep it
            controller.handle_command(AppCommand::Play {
                url: silent_station(),
                backups: Vec::new(),
                name: None,
                logo_url: None,
                country: Some(String::new()),
                taken: None,
            });
            {
                let state = state.lock().unwrap();
                assert_eq!(state.station_logo_url, None);
                assert_eq!(state.station_country, None);
            }
            controller.handle_command(AppCommand::Stop);
        }

        #[test]
        fn switching_station_stops_the_old_one_at_once() {
            let (mut controller, state) = controller();
            controller.stream_id = Some(StreamId(1));
            controller.handle_engine_event(on(1, playing()));
            controller.start_stream(&silent_station(), Some("Next".into()), None, None);
            assert_eq!(controller.stream_id, None);
            {
                let state = state.lock().unwrap();
                assert_eq!(state.playback, PlaybackState::Stopped);
                assert!(state.is_resolving);
            }
            controller.handle_command(AppCommand::Stop);
        }

        /// A station that answers with MP3 frames (of silence) for a minute
        fn mp3_station() -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/live.mp3", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    std::thread::spawn(move || {
                        let mut request = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        while request.read_line(&mut line).is_ok_and(|n| n > 2) {
                            line.clear();
                        }
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\n\r\n");
                        // MPEG-1 Layer III, 128 kbps, 44.1 kHz: 417-byte frames
                        let mut frame = vec![0u8; 417];
                        frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
                        for _ in 0..2_000 {
                            if stream.write_all(&frame).is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(26));
                        }
                    });
                }
            });
            url
        }

        #[test]
        fn a_stop_leaves_a_playing_stream_to_the_engine() {
            let (mut controller, _state) = controller();
            // No device needed: the engine waits for one
            controller.engine = Some(AudioEngine::new().unwrap());
            controller.start_stream(&mp3_station(), Some("MP3 FM".into()), None, None);
            let cancel = controller.stream_cancel.clone().unwrap();
            let resolved = controller
                .cmd_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the station must resolve");
            controller.handle_command(resolved);
            assert!(controller.stream_id.is_some(), "not handed to the engine");

            // The engine ends it after its fade-out, not the stop
            controller.cancel_stream();
            assert!(!cancel.is_cancelled());
            controller.handle_command(AppCommand::Stop);
        }

        /// A step to `url`, as the N and P keys send it
        fn step(controller: &mut AppController, url: &str, name: &str) {
            controller.handle_command(AppCommand::Step {
                url: url.into(),
                backups: Vec::new(),
                name: Some(name.into()),
                logo_url: None,
                country: None,
            });
        }

        /// A controller whose clock a test moves, and that clock
        fn stepping() -> (AppController, Arc<Mutex<AppSnapshot>>, std::time::Instant) {
            let (mut controller, state) = controller();
            let now = std::time::Instant::now();
            controller.fake_now = Some(now);
            (controller, state, now)
        }

        /// `ms` later: what is due by then is done
        fn later(controller: &mut AppController, ms: u64) {
            controller.fake_now = Some(controller.fake_now.unwrap() + Duration::from_millis(ms));
            controller.connect_settled();
        }

        #[test]
        fn stepping_three_times_starts_only_the_last_station() {
            let (mut controller, state, _) = stepping();
            controller.stream_id = Some(StreamId(1));
            controller.handle_engine_event(on(1, playing()));
            let station = silent_station();

            step(&mut controller, "http://first.test/live", "First");
            // It is the current station at once, with the old one stopped,
            // and nothing is connected to yet
            {
                let state = state.lock().unwrap();
                assert_eq!(state.station_name.as_deref(), Some("First"));
                assert_eq!(state.playback, PlaybackState::Stopped);
                assert!(state.is_resolving);
                assert_eq!(state.status_text, "Connecting…");
            }
            assert_eq!(controller.stream_id, None);
            assert!(controller.stream_cancel.is_none());

            later(&mut controller, 100);
            step(&mut controller, "http://second.test/live", "Second");
            later(&mut controller, 100);
            step(&mut controller, &station, "Third");
            // Each step waits its own time: the first two are long past
            later(&mut controller, 299);
            assert!(controller.stream_cancel.is_none());
            assert!(controller.cmd_rx.is_empty(), "a skipped station started");

            later(&mut controller, 1);
            assert!(controller.stream_cancel.is_some(), "the last one starts");
            assert_eq!(controller.step_due(), None);
            {
                let state = state.lock().unwrap();
                assert_eq!(state.station_name.as_deref(), Some("Third"));
                assert_eq!(state.station_url.as_deref(), Some(station.as_str()));
                assert!(state.is_resolving);
                // Three steps, three stations: the window counts on it
                assert_eq!(state.play_seq, 3);
            }
            // Only the third was resolved: nothing else comes back
            controller.handle_command(AppCommand::Stop);
            let resolved = cancelled_resolve(&controller);
            assert!(controller.cmd_rx.is_empty());
            controller.handle_command(resolved);
            assert_eq!(state.lock().unwrap().status_text, "Stopped");
        }

        #[test]
        fn a_stop_before_a_step_connects_starts_nothing() {
            let (mut controller, state, _) = stepping();
            step(&mut controller, &silent_station(), "Silent FM");
            assert!(controller.step_due().is_some());

            controller.handle_command(AppCommand::Stop);
            assert_eq!(controller.step_due(), None);
            later(&mut controller, 1_000);
            assert!(controller.stream_cancel.is_none());
            assert!(controller.cmd_rx.is_empty(), "the station started");
            let state = state.lock().unwrap();
            assert!(!state.is_resolving);
            assert_eq!(state.status_text, "Stopped");
            assert_eq!(state.station_name.as_deref(), Some("Silent FM"));
        }

        #[test]
        fn a_play_takes_the_place_of_a_step_still_waiting() {
            let (mut controller, state, _) = stepping();
            step(&mut controller, "http://skipped.test/live", "Skipped");
            // A station clicked starts at once
            controller.start_stream(&silent_station(), Some("Clicked".into()), None, None);
            assert_eq!(controller.step_due(), None);
            let clicked = controller.stream_cancel.clone().unwrap();
            later(&mut controller, 1_000);
            assert!(!clicked.is_cancelled());
            assert_eq!(
                state.lock().unwrap().station_name.as_deref(),
                Some("Clicked")
            );
            controller.handle_command(AppCommand::Stop);
        }

        #[test]
        fn a_step_cancels_the_station_still_connecting() {
            let (mut controller, _state, _) = stepping();
            controller.start_stream(&silent_station(), Some("First".into()), None, None);
            let first = controller.stream_cancel.clone().unwrap();
            step(&mut controller, "http://second.test/live", "Second");
            assert!(first.is_cancelled());
            controller.handle_command(AppCommand::Stop);
        }

        #[test]
        fn switching_station_cancels_the_one_resolving() {
            let (mut controller, state) = controller();
            let station = silent_station();
            controller.start_stream(&station, Some("First".into()), None, None);
            let first = controller.stream_cancel.clone().unwrap();
            controller.start_stream(&station, Some("Second".into()), None, None);
            let second = controller.stream_cancel.clone().unwrap();
            assert!(first.is_cancelled());
            assert!(!second.is_cancelled());

            // The first station's result is stale: the second still resolves
            let resolved = cancelled_resolve(&controller);
            controller.handle_command(resolved);
            {
                let state = state.lock().unwrap();
                assert!(state.is_resolving);
                assert!(!state.is_error, "{}", state.status_text);
                assert_eq!(state.station_name.as_deref(), Some("Second"));
            }

            controller.handle_command(AppCommand::Stop);
            assert!(second.is_cancelled());
        }
    }

    #[test]
    fn recording_bitrate_choice() {
        assert_eq!(recording_kbps(Some(128), Some(64)), 128);
        assert_eq!(recording_kbps(Some(320), Some(128)), 320);
        assert_eq!(recording_kbps(None, Some(320)), 320);
        assert_eq!(recording_kbps(None, Some(64)), 64);
        assert_eq!(recording_kbps(None, None), 256);
        assert_eq!(recording_kbps(None, Some(0)), 256);
        assert_eq!(recording_kbps(None, Some(16)), 32);
        assert_eq!(recording_kbps(None, Some(1411)), 320);
    }

    fn controller() -> (AppController, Arc<Mutex<AppSnapshot>>) {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (analysis_tx, _) = crossbeam_channel::unbounded();
        let (stats_tx, _) = crossbeam_channel::unbounded();
        let state = Arc::new(Mutex::new(AppSnapshot::default()));
        let controller = AppController::new(cmd_rx, cmd_tx, state.clone(), analysis_tx, stats_tx);
        (controller, state)
    }

    fn playing() -> AudioEvent {
        AudioEvent::Playing(radiotrope::audio::CodecInfo {
            codec_name: "MP3".into(),
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: None,
            bitrate: Some(128),
        })
    }

    /// An event about station `id`
    fn on(id: u64, event: AudioEvent) -> EngineEvent {
        EngineEvent {
            stream: Some(StreamId(id)),
            event,
        }
    }

    /// An event about no station
    fn device(event: AudioEvent) -> EngineEvent {
        EngineEvent {
            stream: None,
            event,
        }
    }

    #[test]
    fn a_hung_output_device_shows_until_it_answers() {
        let (mut controller, state) = controller();
        controller.handle_engine_event(device(AudioEvent::OutputNotResponding));
        {
            let state = state.lock().unwrap();
            assert!(state.is_error);
            assert_eq!(state.status_text, OUTPUT_NOT_RESPONDING);
            assert_eq!(state.last_error.as_deref(), Some(OUTPUT_NOT_RESPONDING));
        }

        // A station picked meanwhile doesn't cover it, and an agent waiting
        // for that station is told why it doesn't start
        resolving(&mut controller, &state);
        state.lock().unwrap().last_error = None;
        controller.keep_hang_showing();
        {
            let state = state.lock().unwrap();
            assert_eq!(state.status_text, OUTPUT_NOT_RESPONDING);
            assert_eq!(state.last_error.as_deref(), Some(OUTPUT_NOT_RESPONDING));
        }

        // The device answers: the station's status shows again
        controller.handle_engine_event(device(AudioEvent::OutputResponding));
        let state = state.lock().unwrap();
        assert!(!state.is_error);
        assert_eq!(state.status_text, status::CONNECTING);
        assert_eq!(state.last_error, None);
    }

    /// A controller playing station 1
    fn controller_playing() -> (AppController, Arc<Mutex<AppSnapshot>>) {
        let (mut controller, state) = controller();
        connecting(&mut controller, &state, 1);
        controller.handle_engine_event(on(1, playing()));
        (controller, state)
    }

    #[test]
    fn a_failed_stream_stops_with_its_error_showing() {
        let (mut controller, state) = controller_playing();
        // What the engine sends when a station's audio can't be read
        controller.handle_engine_event(on(
            1,
            AudioEvent::Error("Stream error: HTTP 404 Not Found".into()),
        ));
        controller.handle_engine_event(on(1, AudioEvent::Stopped));

        let state = state.lock().unwrap();
        assert_eq!(state.playback, PlaybackState::Stopped);
        assert!(state.is_error);
        assert!(
            state.status_text.contains("HTTP 404"),
            "{}",
            state.status_text
        );
    }

    #[test]
    fn a_codec_change_renames_the_codec_and_keeps_playing() {
        let (mut controller, state) = controller_playing();
        let info = radiotrope::audio::CodecInfo {
            codec_name: "AAC+".into(),
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: None,
            bitrate: None,
        };
        controller.handle_engine_event(on(1, AudioEvent::CodecChanged(info)));

        let state = state.lock().unwrap();
        assert_eq!(state.codec_name, "AAC+");
        assert_eq!(state.playback, PlaybackState::Playing);
        assert_eq!(state.status_text, "Playing");
    }

    #[test]
    fn no_audio_stays_showing_after_the_stop() {
        let (mut controller, state) = controller_playing();
        controller.handle_engine_event(on(1, AudioEvent::NoAudioTimeout));
        controller.handle_engine_event(on(1, AudioEvent::Stopped));

        let state = state.lock().unwrap();
        assert_eq!(state.playback, PlaybackState::Stopped);
        assert!(state.is_error);
        assert_eq!(state.status_text, NO_AUDIO);
    }

    /// What `start_stream` does while the next station resolves
    fn resolving(controller: &mut AppController, state: &Arc<Mutex<AppSnapshot>>) {
        controller.cancel_stream();
        let mut state = state.lock().unwrap();
        state.is_resolving = true;
        state.is_connecting = false;
        state.status_text = status::CONNECTING.into();
        state.is_error = false;
        state.playback = PlaybackState::Stopped;
    }

    /// What `handle_stream_resolved` does once it hands station `id` over
    fn connecting(controller: &mut AppController, state: &Arc<Mutex<AppSnapshot>>, id: u64) {
        controller.stream_id = Some(StreamId(id));
        let mut state = state.lock().unwrap();
        state.is_resolving = false;
        state.is_connecting = true;
        state.status_text = status::CONNECTING.into();
    }

    mod stream_fallback {
        use super::*;

        // Nothing listens at these, so a resolve fails at once
        const DEFAULT: &str = "http://127.0.0.1:1/default.mp3";
        const SECOND: &str = "http://127.0.0.1:1/second.mp3";
        const THIRD: &str = "http://127.0.0.1:1/third.mp3";

        /// Play a station with three streams
        fn play(controller: &mut AppController) {
            controller.handle_command(AppCommand::Play {
                url: DEFAULT.into(),
                backups: vec![SECOND.into(), THIRD.into()],
                name: Some("Three Streams FM".into()),
                logo_url: None,
                country: None,
                taken: None,
            });
        }

        /// Take up the result of the resolve under way, which fails
        fn resolve_fails(controller: &mut AppController) {
            let resolved = controller
                .cmd_rx
                .recv_timeout(Duration::from_secs(20))
                .expect("the resolve must fail");
            assert!(matches!(
                resolved,
                AppCommand::InternalStreamResolved { result: Err(_), .. }
            ));
            controller.handle_command(resolved);
        }

        /// (the stream tried, its place, how many, resolving, failed)
        fn tried(state: &Arc<Mutex<AppSnapshot>>) -> (String, usize, usize, bool, bool) {
            let state = state.lock().unwrap();
            assert_eq!(
                state.station_url.as_deref(),
                Some(DEFAULT),
                "the station is known by its default stream, whichever is tried"
            );
            (
                state.stream_url.clone().unwrap_or_default(),
                state.stream_index,
                state.stream_count,
                state.is_resolving,
                state.is_error,
            )
        }

        #[test]
        fn streams_that_dont_resolve_are_tried_in_order_before_the_error_shows() {
            let (mut controller, state) = controller();
            play(&mut controller);
            let seq = state.lock().unwrap().play_seq;
            assert_eq!(tried(&state), (DEFAULT.into(), 0, 3, true, false));

            resolve_fails(&mut controller);
            assert_eq!(tried(&state), (SECOND.into(), 1, 3, true, false));
            assert_eq!(state.lock().unwrap().status_text, status::CONNECTING);
            assert_eq!(state.lock().unwrap().last_error, None);

            resolve_fails(&mut controller);
            assert_eq!(tried(&state), (THIRD.into(), 2, 3, true, false));

            // The last one fails too: now the station has failed
            resolve_fails(&mut controller);
            assert_eq!(tried(&state), (THIRD.into(), 2, 3, false, true));
            let state = state.lock().unwrap();
            assert!(state.last_error.is_some());
            assert_eq!(state.playback, PlaybackState::Stopped);
            assert_eq!(state.play_seq, seq, "one station, started once");
            assert_eq!(state.station_name.as_deref(), Some("Three Streams FM"));
        }

        #[test]
        fn a_stream_the_engine_cant_play_gives_way_to_the_next() {
            let (mut controller, state) = controller();
            play(&mut controller);
            // The default resolved and was handed to the engine, which
            // finds nothing it can play
            connecting(&mut controller, &state, 1);
            controller.handle_engine_event(on(1, AudioEvent::Error("Probe failed".into())));
            assert_eq!(tried(&state), (SECOND.into(), 1, 3, true, false));
            // Its Stopped is an earlier stream's last word
            controller.handle_engine_event(on(1, AudioEvent::Stopped));
            assert_eq!(tried(&state), (SECOND.into(), 1, 3, true, false));
            assert_eq!(state.lock().unwrap().status_text, status::CONNECTING);

            // The second sends no audio
            connecting(&mut controller, &state, 2);
            controller.handle_engine_event(on(2, AudioEvent::NoAudioTimeout));
            assert_eq!(tried(&state), (THIRD.into(), 2, 3, true, false));

            // The third plays
            connecting(&mut controller, &state, 3);
            controller.handle_engine_event(on(3, playing()));
            let state = state.lock().unwrap();
            assert_eq!(state.playback, PlaybackState::Playing);
            assert_eq!(state.stream_url.as_deref(), Some(THIRD));
            assert_eq!((state.stream_index, state.stream_count), (2, 3));
            assert!(!state.is_error);
        }

        #[test]
        fn a_stream_that_played_fails_as_the_station_not_to_the_next_stream() {
            let (mut controller, state) = controller();
            play(&mut controller);
            connecting(&mut controller, &state, 1);
            controller.handle_engine_event(on(1, playing()));

            controller.handle_engine_event(on(
                1,
                AudioEvent::Error("Stream error: HTTP 404 Not Found".into()),
            ));
            controller.handle_engine_event(on(1, AudioEvent::Stopped));
            assert_eq!(tried(&state), (DEFAULT.into(), 0, 3, false, true));
        }

        #[test]
        fn a_device_error_isnt_the_streams_failure() {
            let (mut controller, state) = controller();
            play(&mut controller);
            connecting(&mut controller, &state, 1);
            controller.handle_engine_event(device(AudioEvent::Error("No device".into())));
            assert_eq!(state.lock().unwrap().stream_url.as_deref(), Some(DEFAULT));
        }

        #[test]
        fn a_station_started_again_begins_with_the_stream_that_last_played() {
            let (mut controller, state) = controller();
            play(&mut controller);
            resolve_fails(&mut controller);
            // The second stream plays (its resolve here fails; the test
            // stands in for the engine)
            let _ = controller.cmd_rx.recv_timeout(Duration::from_secs(20));
            connecting(&mut controller, &state, 1);
            controller.handle_engine_event(on(1, playing()));
            assert_eq!(state.lock().unwrap().stream_url.as_deref(), Some(SECOND));

            // A reconnect (an outage's retry, a recording that waits)
            controller.start_stream(DEFAULT, None, None, None);
            assert_eq!(tried(&state), (SECOND.into(), 1, 3, true, false));
            // And from there the others get their turn again
            resolve_fails(&mut controller);
            assert_eq!(tried(&state), (THIRD.into(), 2, 3, true, false));
            resolve_fails(&mut controller);
            assert_eq!(tried(&state), (DEFAULT.into(), 0, 3, true, false));
            resolve_fails(&mut controller);
            assert_eq!(tried(&state), (DEFAULT.into(), 0, 3, false, true));

            // Picked again by the user: from its default
            play(&mut controller);
            assert_eq!(tried(&state), (DEFAULT.into(), 0, 3, true, false));
            controller.handle_command(AppCommand::Stop);
        }

        #[test]
        fn another_station_started_without_streams_has_only_its_own() {
            let (mut controller, state) = controller();
            play(&mut controller);
            // The schedule starts a station: just its address
            let other = "http://127.0.0.1:1/other.mp3";
            controller.start_new(other, None, None, None);
            {
                let state = state.lock().unwrap();
                assert_eq!(state.stream_url.as_deref(), Some(other));
                assert_eq!((state.stream_index, state.stream_count), (0, 1));
            }
            // The first station's resolve is stale; the other's fails
            loop {
                let resolved = controller
                    .cmd_rx
                    .recv_timeout(Duration::from_secs(20))
                    .expect("the resolve must fail");
                controller.handle_command(resolved);
                if !state.lock().unwrap().is_resolving {
                    break;
                }
            }
            let state = state.lock().unwrap();
            assert!(state.is_error);
            assert_eq!(state.stream_url.as_deref(), Some(other));
        }

        #[test]
        fn a_step_connects_to_the_default_and_falls_back_like_a_play() {
            let (mut controller, state) = controller();
            controller.fake_now = Some(std::time::Instant::now());
            controller.handle_command(AppCommand::Step {
                url: DEFAULT.into(),
                backups: vec![SECOND.into()],
                name: None,
                logo_url: None,
                country: None,
            });
            assert_eq!(tried(&state), (DEFAULT.into(), 0, 2, true, false));
            controller.fake_now = controller.fake_now.map(|now| now + STEP_SETTLE);
            controller.connect_settled();
            resolve_fails(&mut controller);
            assert_eq!(tried(&state), (SECOND.into(), 1, 2, true, false));
            resolve_fails(&mut controller);
            assert_eq!(tried(&state), (SECOND.into(), 1, 2, false, true));
        }
    }

    #[test]
    fn a_station_that_fails_after_another_shows_stopped_with_its_error() {
        let (mut controller, state) = controller_playing();
        // Switch: the engine stops the old station while the new resolves
        resolving(&mut controller, &state);
        controller.handle_engine_event(on(1, AudioEvent::Stopped));
        assert_eq!(state.lock().unwrap().playback, PlaybackState::Stopped);
        assert_eq!(state.lock().unwrap().status_text, status::CONNECTING);

        // The new station fails to start
        connecting(&mut controller, &state, 2);
        controller.handle_engine_event(on(2, AudioEvent::Error("Probe failed".into())));
        controller.handle_engine_event(on(2, AudioEvent::Stopped));
        let state = state.lock().unwrap();
        assert_eq!(state.playback, PlaybackState::Stopped);
        assert!(state.is_error);
        assert!(
            state.status_text.contains("probe failed"),
            "{}",
            state.status_text
        );
    }

    #[test]
    fn a_station_counts_as_connecting_until_it_plays_or_fails() {
        let (mut controller, state) = controller_playing();
        resolving(&mut controller, &state);
        connecting(&mut controller, &state, 2);
        // The old station's last words and the new one's buffering
        controller.handle_engine_event(on(1, AudioEvent::Stopped));
        controller.handle_engine_event(on(2, AudioEvent::Buffering(40)));
        assert!(state.lock().unwrap().is_connecting);
        controller.handle_engine_event(on(2, playing()));
        assert!(!state.lock().unwrap().is_connecting);

        resolving(&mut controller, &state);
        connecting(&mut controller, &state, 3);
        controller.handle_engine_event(on(3, AudioEvent::NoAudioTimeout));
        assert!(!state.lock().unwrap().is_connecting);
    }

    #[test]
    fn the_old_station_changes_nothing_while_the_next_resolves() {
        let (mut controller, state) = controller_playing();
        resolving(&mut controller, &state);
        for event in [
            on(1, AudioEvent::Buffering(40)),
            on(1, AudioEvent::StreamStalled),
            on(1, AudioEvent::Error("Stream error: old".into())),
            on(1, AudioEvent::NoAudioTimeout),
            on(1, AudioEvent::Stopped),
            device(AudioEvent::OutputLost),
        ] {
            controller.handle_engine_event(event);
        }
        {
            let state = state.lock().unwrap();
            assert_eq!(state.status_text, status::CONNECTING);
            assert!(!state.is_error);
            assert_eq!(state.playback, PlaybackState::Stopped);
        }

        // The old station's failure doesn't stick to the new one
        connecting(&mut controller, &state, 2);
        controller.handle_engine_event(on(2, playing()));
        controller.handle_engine_event(on(2, AudioEvent::Stopped));
        let state = state.lock().unwrap();
        assert!(!state.is_error);
        assert_eq!(state.status_text, "Stopped");
    }

    #[test]
    fn the_old_station_changes_nothing_once_the_next_is_handed_over() {
        let (mut controller, state) = controller_playing();
        resolving(&mut controller, &state);
        connecting(&mut controller, &state, 2);

        // The old station's last words arrive after the new one's start
        // was sent: they used to show as the new station's error
        controller.handle_engine_event(on(1, AudioEvent::Error("Stream error: old".into())));
        controller.handle_engine_event(on(1, AudioEvent::Stopped));
        {
            let state = state.lock().unwrap();
            assert_eq!(state.status_text, status::CONNECTING);
            assert!(!state.is_error);
        }

        controller.handle_engine_event(on(2, playing()));
        controller.handle_engine_event(on(1, AudioEvent::Paused));
        let state = state.lock().unwrap();
        assert_eq!(state.playback, PlaybackState::Playing);
        assert_eq!(state.status_text, "Playing");
    }

    #[test]
    fn no_device_at_start_says_so() {
        let (mut controller, state) = controller();
        controller.handle_engine_event(device(AudioEvent::OutputLost));
        {
            let state = state.lock().unwrap();
            assert!(state.is_error);
            assert!(state.status_text.contains("Waiting for a device"));
        }
        controller.handle_engine_event(device(AudioEvent::OutputRestored));
        let state = state.lock().unwrap();
        assert!(!state.is_error);
        assert_eq!(state.status_text, "Stopped");
    }

    #[test]
    fn a_station_far_behind_live_after_a_long_output_loss_starts_again() {
        let (mut controller, state) = controller_playing();
        {
            let mut state = state.lock().unwrap();
            // Nothing answers there: the new resolve fails in the background
            state.station_url = Some("http://127.0.0.1:9/live".into());
            state.station_name = Some("Late FM".into());
            state.station_logo_url = Some("http://127.0.0.1:9/logo.png".into());
            state.station_country = Some("Greece".into());
        }
        let play_seq = state.lock().unwrap().play_seq;
        controller.handle_engine_event(on(1, AudioEvent::OutputLost));
        controller.handle_engine_event(on(1, AudioEvent::OutputRestored));
        controller.handle_engine_event(on(1, AudioEvent::FellBehind));
        // The engine ended the old station
        controller.handle_engine_event(on(1, AudioEvent::Stopped));

        {
            let state = state.lock().unwrap();
            assert!(state.is_resolving);
            assert_eq!(state.status_text, status::CONNECTING);
            assert!(!state.is_error);
            assert_eq!(state.play_seq, play_seq + 1);
            assert_eq!(
                state.station_url.as_deref(),
                Some("http://127.0.0.1:9/live")
            );
            assert_eq!(state.station_name.as_deref(), Some("Late FM"));
            assert_eq!(
                state.station_logo_url.as_deref(),
                Some("http://127.0.0.1:9/logo.png")
            );
            assert_eq!(state.station_country.as_deref(), Some("Greece"));
        }
        assert_eq!(controller.stream_id, None);
        controller.handle_command(AppCommand::Stop);
    }

    #[test]
    fn a_stop_after_playing_again_is_a_plain_stop() {
        let (mut controller, state) = controller_playing();
        controller.handle_engine_event(on(1, AudioEvent::Error("Stream error: x".into())));
        controller.handle_engine_event(on(1, AudioEvent::Stopped));
        resolving(&mut controller, &state);
        connecting(&mut controller, &state, 2);
        controller.handle_engine_event(on(2, playing()));
        controller.handle_engine_event(on(2, AudioEvent::Stopped));

        let state = state.lock().unwrap();
        assert!(!state.is_error);
        assert_eq!(state.status_text, "Stopped");
    }
}
