//! Keeping a recording through its station's failure
//!
//! A dropout the stream's own reconnects ride out never reaches here: the
//! station keeps playing, and the recording with it. When the station
//! fails anyway (a reconnect landed on audio the decoder couldn't read, a
//! playlist moved on, a server answered with an error), the recording
//! stays open and the station is started again, with backoff, so the file
//! carries on after a short gap.
//!
//! The recording ends, and is saved, when the user stops it or picks
//! another station, when the station's reader had already given up after
//! its own reconnects, or when starting the station again hasn't worked for
//! [`RESUME_WINDOW`].
//!
//! # Playing through an outage
//!
//! A station that was playing when its reader gave up (the network or the
//! station's server was away for [`RECONNECT_GIVE_UP_SECS`]) isn't left
//! stopped: it shows "Reconnecting…" and is started again after 30 s, after
//! 60 s more, and then every 2 minutes ([`Outage`]), until it plays, the
//! user stops it or another station is picked. Its recording has ended by
//! then, as above.

use std::time::{Duration, Instant};

use radiotrope::audio::{AudioEvent, PlaybackState};
use radiotrope::config::timeouts::{
    MAX_BACKOFF_SECS, OUTAGE_RETRY_DELAYS_SECS, RECONNECT_GIVE_UP_SECS, RETRY_BASE_DELAY_SECS,
};
use radiotrope::stream::is_gave_up;

use super::AppController;

/// How long a recording waits for its station to play again
pub const RESUME_WINDOW: Duration = Duration::from_secs(RECONNECT_GIVE_UP_SECS);

/// The status while a recording waits for its station
pub const RECONNECTING: &str = radiotrope_app::text::status::RECONNECTING;

/// A recording waiting for its station, which failed, to play again
pub struct Resume {
    url: String,
    name: Option<String>,
    logo_url: Option<String>,
    country: Option<String>,
    /// When the recording stops waiting: [`RESUME_WINDOW`] after the
    /// station first failed
    until: Instant,
    /// Starts tried so far
    tries: u32,
    /// When to start the station again; `None` while a start is under way
    next_try: Option<Instant>,
}

/// The wait before start number `tries + 1`: 2 s, 4 s, 8 s, then 10 s
fn retry_delay(tries: u32) -> Duration {
    let secs = RETRY_BASE_DELAY_SECS.saturating_mul(1 << tries.min(5));
    Duration::from_secs(secs.min(MAX_BACKOFF_SECS))
}

/// A station that was playing when its reader gave up, while it is started
/// again until it plays
pub struct Outage {
    url: String,
    name: Option<String>,
    logo_url: Option<String>,
    country: Option<String>,
    /// Starts tried so far
    tries: u32,
    /// When to start the station again; `None` while a start is under way
    next_try: Option<Instant>,
}

/// The wait before start number `tries + 1` of a station an outage
/// stopped: 30 s, 60 s, then 2 min each time
fn outage_delay(tries: u32) -> Duration {
    let last = OUTAGE_RETRY_DELAYS_SECS.len() - 1;
    Duration::from_secs(OUTAGE_RETRY_DELAYS_SECS[(tries as usize).min(last)])
}

impl AppController {
    /// The time the outage's retries go by
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.fake_now {
            return now;
        }
        Instant::now()
    }

    /// The playing station's `event`, as it concerns an outage: a reader
    /// that gave up on a station that was playing begins one, a start that
    /// fails waits for the next, and the station playing ends it
    pub(super) fn outage_station_event(&mut self, event: &AudioEvent) {
        match event {
            AudioEvent::Playing(_) => self.outage = None,
            AudioEvent::Error(_) | AudioEvent::NoAudioTimeout if self.outage.is_some() => {
                self.outage_try_failed();
            }
            AudioEvent::Error(e) if is_gave_up(e) => self.begin_outage(),
            // A start that ended without saying why. The Stopped of the
            // station before it comes while the start still resolves.
            AudioEvent::Stopped if self.outage.is_some() => {
                let resolving = self
                    .shared_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_resolving;
                if !resolving {
                    self.outage_try_failed();
                }
            }
            _ => {}
        }
    }

    /// The playing station's reader gave up: start the station again in a
    /// while. One that never played just fails.
    fn begin_outage(&mut self) {
        let next_try = Some(self.now() + outage_delay(0));
        let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        if state.playback != PlaybackState::Playing || state.alarm_beep {
            return;
        }
        let Some(url) = state.station_url.clone() else {
            return;
        };
        self.outage = Some(Outage {
            url,
            name: state.station_name.clone(),
            logo_url: state.station_logo_url.clone(),
            country: state.station_country.clone(),
            tries: 0,
            next_try,
        });
    }

    /// The station was playing and is being started again (it fell far
    /// behind live): if that start fails, the next ones come as after an
    /// outage. Not while a recording waits for it, which has its own tries.
    pub(super) fn expect_outage(
        &mut self,
        url: String,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
    ) {
        if self.resume.is_some() {
            return;
        }
        self.outage = Some(Outage {
            url,
            name,
            logo_url,
            country,
            tries: 0,
            next_try: None,
        });
    }

    /// The start of a station an outage stopped failed: the next one comes
    /// after a longer wait, and the status still says it reconnects
    pub(super) fn outage_try_failed(&mut self) {
        let now = self.now();
        let Some(outage) = self.outage.as_mut() else {
            return;
        };
        // An error and the Stopped after it are one failure
        if outage.next_try.is_none() {
            outage.next_try = Some(now + outage_delay(outage.tries));
        }
        self.show_outage();
    }

    /// When the station an outage stopped is started again, if one waits
    pub(super) fn outage_due(&self) -> Option<Instant> {
        self.outage.as_ref().and_then(|outage| outage.next_try)
    }

    /// Start the station an outage stopped, when its time has come
    pub(super) fn retry_outage(&mut self) {
        let now = self.now();
        if !self.outage_due().is_some_and(|at| now >= at) {
            return;
        }
        // An alarm's fallback sound plays instead: it isn't cut off
        let beeping = self
            .shared_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .alarm_beep;
        let Some(outage) = self.outage.as_mut().filter(|_| !beeping) else {
            self.outage = None;
            return;
        };
        outage.tries += 1;
        outage.next_try = None;
        let (url, name, logo_url, country) = (
            outage.url.clone(),
            outage.name.clone(),
            outage.logo_url.clone(),
            outage.country.clone(),
        );
        self.open_stream(&url, name, logo_url, country);
        // A start that failed at once has already said so
        if self.outage.is_some() {
            self.shared_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status_text = RECONNECTING.into();
        }
    }

    /// While a station an outage stopped waits for its next start, the
    /// status says it reconnects, and it counts as on its way
    fn show_outage(&mut self) {
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.status_text = RECONNECTING.into();
        state.is_error = false;
        state.is_connecting = true;
    }

    /// The playing station's `event`, as it concerns a running recording:
    /// a failure keeps the recording waiting for the station, a plain end
    /// saves it
    pub(super) fn recording_station_event(&mut self, event: &AudioEvent) {
        match event {
            // It plays (again): the recording carries on
            AudioEvent::Playing(_) => self.resume = None,
            AudioEvent::Error(e) => self.station_failed(e),
            AudioEvent::NoAudioTimeout => self.station_failed("no audio"),
            // A failure comes just before; otherwise the station ended (a
            // file played to its end), and so does the recording
            AudioEvent::Stopped if self.resume.is_none() => self.stop_recording(),
            _ => {}
        }
    }

    /// The station failed with `error`: keep a running recording and start
    /// the station again soon, unless it is no use
    fn station_failed(&mut self, error: &str) {
        let recording = self
            .engine
            .as_ref()
            .is_some_and(|engine| engine.recorder().is_recording());
        if !recording {
            self.resume = None;
            return;
        }
        // Its reader already reconnected for as long as a recording waits
        if is_gave_up(error) {
            self.stop_recording_with(Some("the station stopped".into()));
            return;
        }
        let now = Instant::now();
        match self.resume.as_mut() {
            Some(resume) if now >= resume.until => {
                self.stop_recording_with(Some("the station didn't come back".into()));
            }
            Some(resume) => resume.next_try = Some(now + retry_delay(resume.tries)),
            None => {
                let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                let Some(url) = state.station_url.clone() else {
                    drop(state);
                    self.stop_recording();
                    return;
                };
                self.resume = Some(Resume {
                    url,
                    name: state.station_name.clone(),
                    logo_url: state.station_logo_url.clone(),
                    country: state.station_country.clone(),
                    until: now + RESUME_WINDOW,
                    tries: 0,
                    next_try: Some(now + retry_delay(0)),
                });
            }
        }
    }

    /// Starting the station again failed before it played (`error`)
    pub(super) fn resume_failed(&mut self, error: &str) {
        if self.resume.is_some() {
            self.station_failed(error);
            self.show_resuming();
        }
    }

    /// Start the station a recording waits for, when its time has come
    pub(super) fn retry_resume(&mut self) {
        let due = self
            .resume
            .as_ref()
            .and_then(|resume| resume.next_try)
            .is_some_and(|at| Instant::now() >= at);
        if due {
            self.resume_station_now();
        }
    }

    /// True if a recording waits for the station at `url`
    pub(super) fn resumes(&self, url: &str) -> bool {
        self.resume.as_ref().is_some_and(|resume| resume.url == url)
    }

    /// Start the station a recording waits for, keeping the recording.
    /// Returns the station's `play_seq`.
    pub(super) fn resume_station_now(&mut self) -> u64 {
        let Some(resume) = self.resume.as_mut() else {
            return self.play_seq();
        };
        resume.tries += 1;
        resume.next_try = None;
        let (url, name, logo_url, country) = (
            resume.url.clone(),
            resume.name.clone(),
            resume.logo_url.clone(),
            resume.country.clone(),
        );
        let seq = self.open_stream(&url, name, logo_url, country);
        self.shared_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status_text = RECONNECTING.into();
        seq
    }

    /// While a recording waits for its station, the status says so
    pub(super) fn show_resuming(&mut self) {
        if self.resume.is_some() {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            state.status_text = RECONNECTING.into();
            state.is_error = false;
        }
    }

    /// The recording no longer waits: a station that isn't playing shows
    /// that it stopped, rather than that it reconnects
    pub(super) fn end_resume(&mut self) {
        if self.resume.take().is_none() {
            return;
        }
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        if state.status_text == RECONNECTING
            && state.playback == PlaybackState::Stopped
            && !state.is_resolving
        {
            state.status_text = radiotrope_app::text::status::STOPPED.into();
        }
    }

    fn play_seq(&self) -> u64 {
        self.shared_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .play_seq
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use radiotrope::audio::{
        AudioEngine, EngineConfig, EngineEvent, EngineOutput, RecordingFormat, RecordingOptions,
        RecordingTags, TapPoint,
    };

    use super::*;
    use crate::app::state::{AppCommand, AppSnapshot};

    /// A station nothing answers at, so starting it again fails at once
    const STATION: &str = "http://127.0.0.1:1/live";

    /// A controller with a silent engine, recording `STATION`
    fn recording() -> (AppController, Arc<Mutex<AppSnapshot>>, std::path::PathBuf) {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (analysis_tx, _) = crossbeam_channel::unbounded();
        let (stats_tx, _) = crossbeam_channel::unbounded();
        let state = Arc::new(Mutex::new(AppSnapshot::default()));
        let mut controller =
            AppController::new(cmd_rx, cmd_tx, state.clone(), analysis_tx, stats_tx);
        let engine = AudioEngine::with_config(EngineConfig {
            output: EngineOutput::Silent { speed: 1.0 },
            ..EngineConfig::default()
        })
        .expect("the engine starts");
        let path = std::env::temp_dir().join(format!(
            "radiotrope-resume-{}-{:?}.wav",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        engine
            .recorder()
            .start(RecordingOptions {
                path: path.clone(),
                format: RecordingFormat::Wav,
                bitrate_kbps: 128,
                tap: TapPoint::BeforeEq,
                tags: RecordingTags::default(),
            })
            .unwrap();
        controller.engine = Some(engine);
        {
            let mut state = state.lock().unwrap();
            state.station_url = Some(STATION.into());
            state.station_name = Some("Flaky FM".into());
            state.playback = PlaybackState::Playing;
        }
        (controller, state, path)
    }

    fn event(controller: &mut AppController, event: AudioEvent) {
        controller.handle_engine_event(EngineEvent {
            stream: None,
            event,
        });
    }

    /// The station fails as a reconnect into junk makes it
    fn fail(controller: &mut AppController) {
        event(
            controller,
            AudioEvent::Error("Stream error: unsupported feature: adts".into()),
        );
        event(controller, AudioEvent::Stopped);
    }

    fn is_recording(controller: &AppController) -> bool {
        controller
            .engine
            .as_ref()
            .unwrap()
            .recorder()
            .is_recording()
    }

    fn notice(state: &Arc<Mutex<AppSnapshot>>) -> Option<(String, bool)> {
        let state = state.lock().unwrap();
        state
            .recording_notice
            .as_ref()
            .map(|n| (n.text.clone(), n.is_error))
    }

    #[test]
    fn a_failed_station_keeps_its_recording_and_waits_for_it() {
        let (mut controller, state, path) = recording();
        fail(&mut controller);
        assert!(is_recording(&controller));
        assert!(controller.resumes(STATION));
        assert_eq!(state.lock().unwrap().status_text, RECONNECTING);
        assert_eq!(notice(&state), None);

        // It plays again: the recording carries on in the same file
        let codec = radiotrope::audio::CodecInfo {
            codec_name: "AAC".into(),
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: None,
            bitrate: None,
        };
        event(&mut controller, AudioEvent::Playing(codec));
        assert!(controller.resume.is_none());
        assert!(is_recording(&controller));
        controller.stop_recording();
        assert!(path.exists());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_station_whose_reader_gave_up_ends_the_recording() {
        let (mut controller, state, path) = recording();
        event(
            &mut controller,
            AudioEvent::Error("Stream error: No audio for 2 min: connection lost".into()),
        );
        event(&mut controller, AudioEvent::Stopped);
        assert!(!is_recording(&controller));
        assert!(controller.resume.is_none());
        let (text, is_error) = notice(&state).expect("a notice");
        assert!(text.contains("the station stopped"), "{text}");
        assert!(is_error);
        // The station itself is started again later
        assert!(controller.outage.is_some());
        assert_eq!(state.lock().unwrap().status_text, RECONNECTING);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_station_that_ends_by_itself_ends_the_recording() {
        let (mut controller, state, path) = recording();
        event(&mut controller, AudioEvent::Stopped);
        assert!(!is_recording(&controller));
        let (text, is_error) = notice(&state).expect("a notice");
        assert!(text.starts_with("Saved"), "{text}");
        assert!(!is_error);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_station_that_doesnt_come_back_ends_the_recording() {
        let (mut controller, state, path) = recording();
        fail(&mut controller);
        controller.resume.as_mut().unwrap().until = Instant::now();
        fail(&mut controller);
        assert!(!is_recording(&controller));
        assert!(controller.resume.is_none());
        let (text, _) = notice(&state).expect("a notice");
        assert!(text.contains("the station didn't come back"), "{text}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_station_is_started_again_and_a_failed_start_waits_longer() {
        let (mut controller, state, path) = recording();
        fail(&mut controller);
        // Not yet
        controller.retry_resume();
        assert_eq!(controller.resume.as_ref().unwrap().tries, 0);

        controller.resume.as_mut().unwrap().next_try = Some(Instant::now());
        controller.retry_resume();
        assert_eq!(controller.resume.as_ref().unwrap().tries, 1);
        assert_eq!(controller.resume.as_ref().unwrap().next_try, None);
        assert_eq!(state.lock().unwrap().status_text, RECONNECTING);

        // Nothing answers there: the start fails, and the next waits 4 s
        let resolved = controller
            .cmd_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the start must fail");
        assert!(matches!(
            resolved,
            AppCommand::InternalStreamResolved { .. }
        ));
        controller.handle_command(resolved);
        assert!(is_recording(&controller));
        let next = controller
            .resume
            .as_ref()
            .unwrap()
            .next_try
            .expect("a next try");
        let wait = next.saturating_duration_since(Instant::now());
        assert!(
            wait > Duration::from_secs(3) && wait <= Duration::from_secs(4),
            "{wait:?}"
        );
        assert_eq!(state.lock().unwrap().status_text, RECONNECTING);
        controller.stop_recording();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stopping_the_recording_while_it_waits_shows_the_station_stopped() {
        let (mut controller, state, path) = recording();
        fail(&mut controller);
        controller.handle_command(AppCommand::StopRecording);
        assert!(!is_recording(&controller));
        assert!(controller.resume.is_none());
        assert_eq!(state.lock().unwrap().status_text, "Stopped");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn playing_the_waiting_station_keeps_the_recording_and_another_ends_it() {
        let (mut controller, _state, path) = recording();
        fail(&mut controller);
        controller.start_stream(STATION, Some("Flaky FM".into()), None, None);
        assert!(is_recording(&controller));
        assert_eq!(controller.resume.as_ref().unwrap().tries, 1);

        controller.start_stream("http://127.0.0.1:1/other", None, None, None);
        assert!(!is_recording(&controller));
        assert!(controller.resume.is_none());
        let _ = std::fs::remove_file(path);
    }

    /// A controller with no engine, playing `STATION`, its clock at the
    /// time returned
    fn playing() -> (AppController, Arc<Mutex<AppSnapshot>>, Instant) {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (analysis_tx, _) = crossbeam_channel::unbounded();
        let (stats_tx, _) = crossbeam_channel::unbounded();
        let state = Arc::new(Mutex::new(AppSnapshot::default()));
        let mut controller =
            AppController::new(cmd_rx, cmd_tx, state.clone(), analysis_tx, stats_tx);
        let now = Instant::now();
        controller.fake_now = Some(now);
        {
            let mut state = state.lock().unwrap();
            state.station_url = Some(STATION.into());
            state.station_name = Some("Flaky FM".into());
            state.playback = PlaybackState::Playing;
            state.status_text = radiotrope_app::text::status::PLAYING.into();
        }
        (controller, state, now)
    }

    /// The station's reader gives up, as after two minutes without network
    fn give_up(controller: &mut AppController) {
        event(
            controller,
            AudioEvent::Error("Stream error: No audio for 2 min: connection lost".into()),
        );
        event(controller, AudioEvent::Stopped);
    }

    /// Move the clock on by `secs`, and start the station if it is due
    fn after(controller: &mut AppController, secs: u64) {
        let now = controller.fake_now.unwrap() + Duration::from_secs(secs);
        controller.fake_now = Some(now);
        controller.retry_outage();
    }

    /// The start under way fails, as nothing answers at `STATION`
    fn start_fails(controller: &mut AppController) {
        let resolved = controller
            .cmd_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the start must fail");
        assert!(matches!(
            resolved,
            AppCommand::InternalStreamResolved { .. }
        ));
        controller.handle_command(resolved);
    }

    fn tries(controller: &AppController) -> u32 {
        controller.outage.as_ref().expect("an outage").tries
    }

    /// How long from now the station is started again
    fn due_in(controller: &AppController) -> Option<u64> {
        let now = controller.fake_now.unwrap();
        controller
            .outage_due()
            .map(|at| at.duration_since(now).as_secs())
    }

    #[test]
    fn a_station_whose_reader_gave_up_shows_reconnecting() {
        let (mut controller, state, _) = playing();
        give_up(&mut controller);
        assert_eq!(tries(&controller), 0);
        assert_eq!(due_in(&controller), Some(30));
        let state = state.lock().unwrap();
        assert_eq!(state.playback, PlaybackState::Stopped);
        assert_eq!(state.status_text, RECONNECTING);
        assert!(!state.is_error);
        // Not heard, and on its way
        assert!(state.is_connecting);
        // Agents and the log still get the reason
        assert!(state.last_error.as_deref().unwrap().contains("No audio"));
    }

    #[test]
    fn after_an_outage_the_station_is_started_at_30_s_60_s_then_every_2_min() {
        let (mut controller, state, _) = playing();
        give_up(&mut controller);
        let play_seq = state.lock().unwrap().play_seq;

        for (n, (wait, next)) in [(30, 60), (60, 120), (120, 120), (120, 120)]
            .into_iter()
            .enumerate()
        {
            let n = n as u32;
            // Not a second early
            after(&mut controller, wait - 1);
            assert_eq!(tries(&controller), n);
            assert!(!state.lock().unwrap().is_resolving);

            after(&mut controller, 1);
            assert_eq!(tries(&controller), n + 1);
            assert_eq!(due_in(&controller), None);
            {
                let state = state.lock().unwrap();
                assert!(state.is_resolving);
                assert_eq!(state.status_text, RECONNECTING);
                assert_eq!(state.station_url.as_deref(), Some(STATION));
                assert_eq!(state.station_name.as_deref(), Some("Flaky FM"));
            }

            start_fails(&mut controller);
            assert_eq!(due_in(&controller), Some(next));
            let state = state.lock().unwrap();
            assert_eq!(state.status_text, RECONNECTING);
            assert!(!state.is_error);
            assert!(!state.is_resolving);
            assert!(state.is_connecting);
        }
        assert_eq!(state.lock().unwrap().play_seq, play_seq + 4);
    }

    #[test]
    fn stop_ends_the_retries() {
        let (mut controller, state, _) = playing();
        give_up(&mut controller);
        controller.handle_command(AppCommand::Stop);
        assert!(controller.outage.is_none());
        let play_seq = {
            let state = state.lock().unwrap();
            assert_eq!(state.status_text, "Stopped");
            assert!(!state.is_connecting);
            state.play_seq
        };

        after(&mut controller, 3600);
        let state = state.lock().unwrap();
        assert!(!state.is_resolving);
        assert_eq!(state.play_seq, play_seq);
        assert_eq!(state.status_text, "Stopped");
    }

    #[test]
    fn stop_during_a_retry_ends_them() {
        let (mut controller, state, _) = playing();
        give_up(&mut controller);
        after(&mut controller, 30);
        assert!(state.lock().unwrap().is_resolving);
        controller.handle_command(AppCommand::Stop);
        assert!(controller.outage.is_none());

        // The start's result comes after the stop and changes nothing
        start_fails(&mut controller);
        assert!(controller.outage.is_none());
        let state = state.lock().unwrap();
        assert_eq!(state.status_text, "Stopped");
        assert!(!state.is_error);
    }

    #[test]
    fn another_station_ends_the_retries() {
        let (mut controller, state, _) = playing();
        give_up(&mut controller);
        controller.handle_command(AppCommand::Play {
            url: "http://127.0.0.1:1/other".into(),
            name: Some("Other FM".into()),
            logo_url: None,
            country: None,
            taken: None,
        });
        assert!(controller.outage.is_none());
        let play_seq = state.lock().unwrap().play_seq;

        // It fails like any station picked, and nothing starts the old one
        start_fails(&mut controller);
        after(&mut controller, 3600);
        let state = state.lock().unwrap();
        assert!(state.is_error, "{}", state.status_text);
        assert_eq!(state.play_seq, play_seq);
        assert_eq!(state.station_name.as_deref(), Some("Other FM"));
    }

    #[test]
    fn the_station_playing_again_ends_the_outage() {
        let (mut controller, state, _) = playing();
        give_up(&mut controller);
        after(&mut controller, 30);
        // The start resolves and the engine plays it
        let _ = controller.cmd_rx.recv_timeout(Duration::from_secs(20));
        state.lock().unwrap().is_resolving = false;
        let codec = radiotrope::audio::CodecInfo {
            codec_name: "MP3".into(),
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: None,
            bitrate: None,
        };
        event(&mut controller, AudioEvent::Playing(codec));
        assert!(controller.outage.is_none());
        {
            let state = state.lock().unwrap();
            assert_eq!(state.playback, PlaybackState::Playing);
            assert_eq!(state.status_text, "Playing");
        }

        // The next outage starts from 30 s again
        give_up(&mut controller);
        assert_eq!(tries(&controller), 0);
        assert_eq!(due_in(&controller), Some(30));
    }

    #[test]
    fn a_start_the_engine_fails_waits_once_for_the_next() {
        let (mut controller, state, _) = playing();
        give_up(&mut controller);
        after(&mut controller, 30);
        // The start resolves, and then the engine can't read the audio
        let _ = controller.cmd_rx.recv_timeout(Duration::from_secs(20));
        state.lock().unwrap().is_resolving = false;
        event(&mut controller, AudioEvent::Error("Probe failed".into()));
        assert_eq!(due_in(&controller), Some(60));
        // Its Stopped, a moment later, is the same failure
        controller.fake_now = Some(controller.fake_now.unwrap() + Duration::from_secs(1));
        event(&mut controller, AudioEvent::Stopped);
        assert_eq!(due_in(&controller), Some(59));
        let state = state.lock().unwrap();
        assert_eq!(state.status_text, RECONNECTING);
        assert!(!state.is_error);
    }

    #[test]
    fn a_station_that_never_played_just_fails() {
        let (mut controller, state, _) = playing();
        state.lock().unwrap().playback = PlaybackState::Stopped;
        give_up(&mut controller);
        assert!(controller.outage.is_none());
        let state = state.lock().unwrap();
        assert!(state.is_error);
        assert!(
            state.status_text.contains("No audio"),
            "{}",
            state.status_text
        );
    }

    #[test]
    fn a_failure_that_isnt_an_outage_stops_the_station() {
        let (mut controller, state, _) = playing();
        fail(&mut controller);
        assert!(controller.outage.is_none());
        let state = state.lock().unwrap();
        assert!(state.is_error);
        assert_eq!(state.playback, PlaybackState::Stopped);
    }

    #[test]
    fn a_station_restarted_to_catch_up_is_tried_again_when_that_fails() {
        let (mut controller, state, _) = playing();
        event(&mut controller, AudioEvent::FellBehind);
        assert_eq!(tries(&controller), 0);
        assert_eq!(due_in(&controller), None);
        assert_eq!(
            state.lock().unwrap().status_text,
            radiotrope_app::text::status::CONNECTING
        );

        start_fails(&mut controller);
        assert_eq!(due_in(&controller), Some(30));
        let state = state.lock().unwrap();
        assert_eq!(state.status_text, RECONNECTING);
        assert!(!state.is_error);
    }

    #[test]
    fn outage_retries_wait_30_s_60_s_then_2_min() {
        let secs: Vec<u64> = (0..5).map(|n| outage_delay(n).as_secs()).collect();
        assert_eq!(secs, [30, 60, 120, 120, 120]);
    }

    #[test]
    fn retries_back_off_to_ten_seconds() {
        let secs: Vec<u64> = (0..6).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 10, 10, 10]);
    }
}
