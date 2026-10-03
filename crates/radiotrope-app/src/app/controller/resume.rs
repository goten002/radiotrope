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

use std::time::{Duration, Instant};

use radiotrope::audio::{AudioEvent, PlaybackState};
use radiotrope::config::timeouts::{
    MAX_BACKOFF_SECS, RECONNECT_GIVE_UP_SECS, RETRY_BASE_DELAY_SECS,
};
use radiotrope::stream::is_gave_up;

use super::AppController;

/// How long a recording waits for its station to play again
pub const RESUME_WINDOW: Duration = Duration::from_secs(RECONNECT_GIVE_UP_SECS);

/// The status while a recording waits for its station
pub const RECONNECTING: &str = "Reconnecting...";

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

impl AppController {
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
            state.status_text = "Stopped".into();
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

    #[test]
    fn retries_back_off_to_ten_seconds() {
        let secs: Vec<u64> = (0..6).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 10, 10, 10]);
    }
}
