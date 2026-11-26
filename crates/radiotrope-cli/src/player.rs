//! The player: one audio engine, the station it plays, and its recording
//!
//! Follows the app's controller: a station is resolved on a thread of its
//! own so the screen keeps going, a newer play makes an older resolve
//! stale, and events about a station that was replaced are ignored. Like
//! the app, a station whose default stream doesn't connect tries its
//! backups in turn before it shows the error.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use radiotrope::audio::{
    find_preset, AudioEngine, AudioEvent, CodecInfo, EngineEvent, RecordingFormat,
    RecordingOptions, RecordingStatus, RecordingTags, StreamId, TapPoint,
};
use radiotrope::config::timeouts::RESOLVE_TIMEOUT_SECS;
use radiotrope::stream::{
    ResolvedStream, StreamCancel, StreamMetadata, StreamResolver, StreamType,
};

use radiotrope_library::fallback::StreamList;
use radiotrope_library::text::{self, status};

use crate::library::{RecordFormat, Settings, Station};
use crate::recording;

/// Longest wait for a station to resolve, a little over the resolver's own
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(RESOLVE_TIMEOUT_SECS + 3);

/// How long a message (an error, a favorite added) stays at the bottom
pub const NOTICE_TIME: Duration = Duration::from_secs(5);
/// How long "Saved to …" stays: long enough to read the folder
const SAVED_NOTICE_TIME: Duration = Duration::from_secs(10);

/// Volume step for `+` and `-`
pub const VOLUME_STEP: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    /// Finding the stream behind the station's URL
    Resolving,
    /// Handed to the engine, waiting for the first audio
    Connecting,
    Playing,
}

/// A recording in progress
#[derive(Debug, Clone)]
pub struct Recording {
    pub duration: Duration,
}

/// A short message for the bottom line
#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub is_error: bool,
    shown_at: Instant,
    lasts: Duration,
}

type Resolved = (u64, Result<ResolvedStream, String>);

pub struct Player {
    engine: Option<AudioEngine>,
    /// The station playing, or the last one played
    pub station: Option<Station>,
    /// The station's streams, and which is tried or playing
    streams: StreamList,
    pub phase: Phase,
    pub status: String,
    pub is_error: bool,
    pub codec: Option<CodecInfo>,
    pub bitrate: Option<u32>,
    pub stream_type: Option<StreamType>,
    pub title: String,
    pub artist: String,
    /// When the station started playing
    pub started_at: Option<Instant>,
    pub volume: f32,
    pub muted: bool,
    pub recording: Option<Recording>,
    pub notice: Option<Notice>,
    /// The recordings saved this session, printed when the player closes
    pub saved: Vec<PathBuf>,
    /// The status is about the sound device, not the station
    pub output_trouble: bool,
    eq_enabled: bool,

    stream_id: Option<StreamId>,
    generation: u64,
    cancel: Option<StreamCancel>,
    resolved_tx: Sender<Resolved>,
    resolved_rx: Receiver<Resolved>,
    metadata_rx: Option<Receiver<StreamMetadata>>,
    /// The stream failed: its Stopped keeps the error showing
    stream_failed: bool,
    /// While the output device hangs: the status it covers
    output_hang: Option<(String, bool)>,
}

impl Player {
    /// A player with the app's volume and equalizer
    pub fn new(engine: AudioEngine, settings: &Settings) -> Self {
        let (resolved_tx, resolved_rx) = crossbeam_channel::unbounded();
        let player = Self {
            engine: Some(engine),
            station: None,
            streams: StreamList::default(),
            phase: Phase::Stopped,
            status: status::READY.to_string(),
            is_error: false,
            codec: None,
            bitrate: None,
            stream_type: None,
            title: String::new(),
            artist: String::new(),
            started_at: None,
            volume: settings.volume,
            muted: settings.muted,
            recording: None,
            notice: None,
            saved: Vec::new(),
            output_trouble: false,
            eq_enabled: settings.eq_enabled,
            stream_id: None,
            generation: 0,
            cancel: None,
            resolved_tx,
            resolved_rx,
            metadata_rx: None,
            stream_failed: false,
            output_hang: None,
        };
        player.apply_volume();
        player.restore_eq(settings);
        player
    }

    fn engine(&self) -> &AudioEngine {
        self.engine
            .as_ref()
            .expect("the engine lives until shutdown")
    }

    /// The equalizer as the app left it, the way the app restores it
    fn restore_eq(&self, settings: &Settings) {
        let engine = self.engine();
        engine.set_eq_enabled(settings.eq_enabled);
        // A preset that has since been renamed or retired plays its saved
        // gains as a custom curve
        match settings.eq_preset_name.as_deref().and_then(find_preset) {
            Some(preset) => {
                engine.set_eq_gains(preset.gains, Some(preset.name.to_string()));
                // Each preset brings its own preamp, unless it was moved
                engine.set_eq_preamp(if settings.eq_preamp_moved {
                    settings.eq_preamp
                } else {
                    preset.preamp_db()
                });
            }
            None => {
                engine.set_eq_gains(settings.eq_gains, None);
                engine.set_eq_preamp(settings.eq_preamp);
            }
        }
    }

    /// Something is playing or on its way
    pub fn is_active(&self) -> bool {
        self.phase != Phase::Stopped
    }

    /// Play `station`, replacing what plays now: its default stream, and
    /// its backups in turn if that doesn't connect
    pub fn play(&mut self, station: Station) {
        self.streams = StreamList::new(&station.url, station.backups());
        self.start(station);
    }

    /// Start `station` again (to catch up with live): from the stream that
    /// last played, with every stream to try once more
    fn restart(&mut self, station: Station) {
        if self.streams.is_station(&station.url) {
            self.streams.restart();
        } else {
            self.streams = StreamList::new(&station.url, station.backups());
        }
        self.start(station);
    }

    /// Which backup plays or is tried, as `stream 2 of 3`: `None` while it
    /// is the station's default
    pub fn backup(&self) -> Option<String> {
        self.station.as_ref()?;
        self.streams.backup()
    }

    fn start(&mut self, station: Station) {
        // Switching station ends the recording of the old one
        self.stop_recording();
        self.cancel_stream();
        self.engine().stop();
        self.metadata_rx = None;
        self.stream_failed = false;

        self.station = Some(station);
        self.phase = Phase::Resolving;
        self.status = status::CONNECTING.to_string();
        self.is_error = false;
        self.codec = None;
        self.bitrate = None;
        self.stream_type = None;
        self.title.clear();
        self.artist.clear();
        self.started_at = None;

        let url = self.streams.current().to_string();
        self.connect(url);
    }

    /// The stream being tried didn't connect: try the station's next one.
    /// False when there is none (or the stream had played already), and
    /// the failure is the station's.
    fn try_next_stream(&mut self, error: &str) -> bool {
        let failed = self.streams.current().to_string();
        let Some(next) = self.streams.next_stream().map(str::to_string) else {
            return false;
        };
        log::warn!("Stream {failed} didn't connect ({error}); trying {next}");
        let handed_over = self.stream_id.is_some();
        self.cancel_stream();
        // The engine lets go of a stream it was given
        if handed_over {
            self.engine().stop();
        }
        self.metadata_rx = None;
        self.stream_failed = false;
        self.phase = Phase::Resolving;
        self.set_status(status::CONNECTING, false);
        self.codec = None;
        self.bitrate = None;
        self.stream_type = None;
        self.connect(next);
        true
    }

    /// Resolve the stream at `url` on a thread of its own, which sends the
    /// result back
    fn connect(&mut self, url: String) {
        let generation = self.generation;
        let cancel = StreamCancel::new();
        self.cancel = Some(cancel.clone());
        let tx = self.resolved_tx.clone();
        std::thread::Builder::new()
            .name("stream-resolve".into())
            .spawn(move || {
                // The resolve runs on a thread of its own so it can be timed
                let (inner_tx, inner_rx) = crossbeam_channel::bounded(1);
                let inner_cancel = cancel.clone();
                let inner_url = url.clone();
                std::thread::Builder::new()
                    .name("stream-resolve-inner".into())
                    .spawn(move || {
                        let result = StreamResolver::resolve_cancellable(&inner_url, &inner_cancel)
                            .map_err(|e| e.to_string());
                        let _ = inner_tx.send(result);
                    })
                    .expect("Failed to spawn stream-resolve-inner thread");
                let result = inner_rx.recv_timeout(RESOLVE_TIMEOUT).unwrap_or_else(|_| {
                    cancel.cancel();
                    // Worded so `playback_error` reads it as a timeout
                    Err(format!(
                        "Stream resolution timed out after {} s",
                        RESOLVE_TIMEOUT.as_secs()
                    ))
                });
                let _ = tx.send((generation, result));
            })
            .expect("Failed to spawn stream-resolve thread");
    }

    /// Stop playing (the station stays, for Space to play it again)
    pub fn stop(&mut self) {
        self.stop_recording();
        self.cancel_stream();
        self.engine().stop();
        self.metadata_rx = None;
        self.stream_failed = false;
        self.phase = Phase::Stopped;
        self.status = status::STOPPED.to_string();
        self.is_error = false;
        self.started_at = None;
        self.title.clear();
        self.artist.clear();
        // The format line is about the stream that played
        self.codec = None;
        self.bitrate = None;
        self.stream_type = None;
    }

    /// End the station resolving or playing, and make anything still on
    /// its way about it stale
    fn cancel_stream(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.generation += 1;
        self.stream_id = None;
    }

    pub fn change_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.muted = false;
        self.apply_volume();
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        self.apply_volume();
    }

    fn apply_volume(&self) {
        self.engine()
            .set_volume(if self.muted { 0.0 } else { self.volume });
    }

    /// Take in what happened since the last call: resolves, engine events,
    /// song info and the recording's progress
    pub fn poll(&mut self) {
        while let Ok((generation, result)) = self.resolved_rx.try_recv() {
            self.on_resolved(generation, result);
        }
        let events: Vec<EngineEvent> = self.engine().event_receiver().try_iter().collect();
        for event in events {
            self.on_event(event);
        }
        if let Some(rx) = &self.metadata_rx {
            let mut latest = None;
            while let Ok(meta) = rx.try_recv() {
                latest = Some(meta);
            }
            if let Some(meta) = latest {
                if let Some(title) = meta.title {
                    self.title = title;
                }
                if let Some(artist) = meta.artist {
                    self.artist = artist;
                }
            }
        }
        self.poll_recording();
        if self
            .notice
            .as_ref()
            .is_some_and(|n| n.shown_at.elapsed() > n.lasts)
        {
            self.notice = None;
        }
    }

    fn on_resolved(&mut self, generation: u64, result: Result<ResolvedStream, String>) {
        // A newer play came while this one resolved
        if generation != self.generation {
            if let Ok(stream) = result {
                stream.cancel.cancel();
            }
            return;
        }
        match result {
            Ok(mut stream) => {
                if let Some(station) = &mut self.station {
                    if station.name.is_empty() {
                        station.name = stream.info.station_name.clone().unwrap_or_default();
                    }
                }
                self.bitrate = stream.info.bitrate;
                self.stream_type = Some(stream.info.stream_type);
                self.metadata_rx = stream.metadata_rx.take();
                self.phase = Phase::Connecting;
                self.stream_id = Some(self.engine().play_stream(stream));
            }
            Err(e) => {
                log::warn!("Can't resolve the station: {e}");
                // The station's next stream may still connect
                if self.try_next_stream(&e) {
                    return;
                }
                self.cancel = None;
                self.phase = Phase::Stopped;
                self.status = text::playback_error(&e);
                self.is_error = true;
            }
        }
    }

    fn on_event(&mut self, event: EngineEvent) {
        // An earlier station's last words
        if event.stream.is_some() && event.stream != self.stream_id {
            return;
        }
        let about_station = event.stream.is_some();
        let event = event.event;

        // A stream that fails before it plays gives way to the station's
        // next one; the failure counts only when the last has failed too
        match &event {
            AudioEvent::Error(e) if about_station && self.try_next_stream(e) => return,
            AudioEvent::NoAudioTimeout
                if about_station && self.try_next_stream(status::NO_AUDIO) =>
            {
                return
            }
            _ => {}
        }

        // The stream ended or failed: keep what was recorded
        if matches!(
            event,
            AudioEvent::Stopped | AudioEvent::Error(_) | AudioEvent::NoAudioTimeout
        ) {
            self.stop_recording();
        }

        // The output was gone so long that the station plays far behind
        // live: start it again, to catch up
        if matches!(event, AudioEvent::FellBehind) {
            if let Some(station) = self.station.clone() {
                self.restart(station);
            }
            return;
        }

        // While a station resolves, news of the device waits: the engine
        // repeats it once the station plays. A hung device shows at once.
        if self.phase == Phase::Resolving
            && !matches!(
                event,
                AudioEvent::OutputNotResponding | AudioEvent::OutputResponding
            )
        {
            return;
        }

        match event {
            AudioEvent::Playing(info) => {
                self.phase = Phase::Playing;
                if self.bitrate.is_none() {
                    self.bitrate = info.bitrate;
                }
                self.codec = Some(info);
                self.started_at.get_or_insert_with(Instant::now);
                self.set_status(status::PLAYING, false);
                self.stream_failed = false;
                // This is the stream a restart begins with
                self.streams.played();
            }
            AudioEvent::CodecChanged(info) => {
                if let Some(codec) = &mut self.codec {
                    codec.codec_name = info.codec_name;
                }
            }
            AudioEvent::Stopped => {
                self.phase = Phase::Stopped;
                self.started_at = None;
                self.cancel = None;
                if !std::mem::take(&mut self.stream_failed) {
                    self.set_status(status::STOPPED, false);
                }
            }
            AudioEvent::Paused => self.set_status(status::PAUSED, false),
            AudioEvent::Resumed => self.set_status(status::PLAYING, false),
            AudioEvent::Error(e) => {
                log::warn!("Engine error: {e}");
                self.status = text::playback_error(&e);
                self.is_error = true;
                self.stream_failed = true;
            }
            AudioEvent::Buffering(pct) => {
                if pct < 100 {
                    self.set_status(&status::buffering(pct), false);
                } else {
                    self.set_status(status::PLAYING, false);
                }
            }
            AudioEvent::StreamStalled => self.set_status(status::WAITING_FOR_DATA, true),
            AudioEvent::StreamRecovered => self.set_status(status::PLAYING, false),
            AudioEvent::OutputLost => {
                self.set_status(status::OUTPUT_LOST, true);
                self.output_trouble = true;
            }
            AudioEvent::OutputNotResponding => {
                self.output_hang = Some((self.status.clone(), self.is_error));
                self.set_status(status::OUTPUT_NOT_RESPONDING, true);
                self.output_trouble = true;
            }
            AudioEvent::OutputResponding => {
                if let Some((status, is_error)) = self.output_hang.take() {
                    if self.status == status::OUTPUT_NOT_RESPONDING {
                        self.output_trouble = status == status::OUTPUT_LOST;
                        self.status = status;
                        self.is_error = is_error;
                    }
                }
            }
            AudioEvent::OutputRestored => {
                let status = match self.phase {
                    Phase::Stopped => status::STOPPED,
                    _ => status::PLAYING,
                };
                self.set_status(status, false);
            }
            AudioEvent::ProbeTimeout => self.set_status(status::NOT_RESPONDING, true),
            AudioEvent::NoAudioTimeout => {
                self.set_status(status::NO_AUDIO, true);
                self.stream_failed = true;
            }
            AudioEvent::FellBehind => {}
        }
    }

    fn set_status(&mut self, status: &str, is_error: bool) {
        self.status = status.to_string();
        self.is_error = is_error;
        // Any later status means the device is back, or no longer the news
        self.output_trouble = false;
    }

    /// The running time is worth showing: not while the sound device is
    /// missing, when nothing is heard
    pub fn shows_clock(&self) -> bool {
        self.started_at.is_some() && !self.output_trouble
    }

    pub fn notify(&mut self, text: impl Into<String>, is_error: bool) {
        self.notify_for(text, is_error, NOTICE_TIME);
    }

    fn notify_for(&mut self, text: impl Into<String>, is_error: bool, lasts: Duration) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error,
            shown_at: Instant::now(),
            lasts,
        });
    }

    /// Start recording, or stop and save the one running
    pub fn toggle_recording(&mut self, settings: &Settings) {
        if self.recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording(settings);
        }
    }

    /// Record the playing station with the app's recording settings
    fn start_recording(&mut self, settings: &Settings) {
        let Some(station) = self
            .station
            .clone()
            .filter(|_| self.phase == Phase::Playing)
        else {
            self.notify("Start a station before recording", true);
            return;
        };
        let folder = recording::folder(settings.recording_dir.as_deref());
        if let Err(e) = recording::prepare_dir(&folder) {
            self.notify(e, true);
            return;
        }
        let format = match settings.recording_format {
            RecordFormat::Mp3 => RecordingFormat::Mp3,
            RecordFormat::Opus => RecordingFormat::Opus,
            RecordFormat::Wav => RecordingFormat::Wav,
        };
        let now = chrono::Local::now();
        let path = recording::new_file_path(&folder, &station.name, now, format.extension());
        let name = if station.name.is_empty() {
            "Radio".to_string()
        } else {
            station.name.clone()
        };
        let options = RecordingOptions {
            path,
            format,
            bitrate_kbps: recording::recording_kbps(settings.recording_bitrate, self.bitrate),
            // After the equalizer only while it is on, as in the app
            tap: if settings.record_with_eq && self.eq_enabled {
                TapPoint::AfterEq
            } else {
                TapPoint::BeforeEq
            },
            tags: RecordingTags {
                title: format!("{name}, {}", now.format("%Y-%m-%d %H:%M")),
                artist: name,
                album: "Radiotrope recordings".to_string(),
                comment: station.url,
                cover: None,
            },
        };
        match self.engine().recorder().start(options) {
            Ok(()) => {
                self.recording = Some(Recording {
                    duration: Duration::ZERO,
                })
            }
            Err(e) => self.notify(text::cant_start_recording(&e.to_string()), true),
        }
    }

    /// Stop the recording, if one runs, and say where it was saved
    pub fn stop_recording(&mut self) {
        self.recording = None;
        let Some(status) = self.engine.as_ref().and_then(|e| e.recorder().stop()) else {
            return;
        };
        self.report_saved(&status);
    }

    fn poll_recording(&mut self) {
        if self.recording.is_none() {
            return;
        }
        let Some(status) = self.engine().recorder().status() else {
            self.recording = None;
            return;
        };
        if status.error.is_some() {
            // Writing failed (a full disk, say): stop and keep what's there
            self.stop_recording();
            return;
        }
        self.recording = Some(Recording {
            duration: status.duration,
        });
    }

    /// Say which folder the recording went to (a terminal can't open it,
    /// so the path is what the user needs), and keep its path for the exit
    fn report_saved(&mut self, status: &RecordingStatus) {
        let folder = status
            .path
            .parent()
            .map(recording::display_path)
            .unwrap_or_default();
        self.saved.push(status.path.clone());
        match &status.error {
            Some(e) => self.notify_for(
                format!("Recording stopped: {e}. Saved to {folder}"),
                true,
                SAVED_NOTICE_TIME,
            ),
            None => self.notify_for(format!("Saved to {folder}"), false, SAVED_NOTICE_TIME),
        }
    }

    /// Save any recording and close the engine. Returns the recordings
    /// saved this session.
    pub fn shutdown(mut self) -> Vec<PathBuf> {
        self.stop_recording();
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
        }
        std::mem::take(&mut self.saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radiotrope::audio::{EngineConfig, EngineOutput};
    use radiotrope_library::data::types::Stream;

    /// A player on a silent output, so it runs on a machine with no sound
    fn player() -> Player {
        let engine = AudioEngine::with_config(EngineConfig {
            output: EngineOutput::Silent { speed: 20.0 },
            ..EngineConfig::default()
        })
        .expect("the engine starts");
        Player::new(engine, &Settings::default())
    }

    /// Nothing listens on port 1, so each of these fails to connect at once
    fn dead(name: &str) -> String {
        format!("http://127.0.0.1:1/{name}.mp3")
    }

    /// Poll `player` until it stops, keeping which backup it said it tried
    fn until_stopped(player: &mut Player) -> Vec<Option<String>> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut said = vec![player.backup()];
        while player.phase != Phase::Stopped {
            assert!(Instant::now() < deadline, "the player never gave up");
            std::thread::sleep(Duration::from_millis(5));
            player.poll();
            if said.last() != Some(&player.backup()) {
                said.push(player.backup());
            }
        }
        said
    }

    #[test]
    fn a_station_whose_default_doesnt_connect_tries_its_backups_in_turn() {
        let mut player = player();
        let station = Station::new("Jazz", dead("a")).with_streams(vec![
            Stream::new(dead("a")),
            Stream::new(dead("b")),
            Stream::new(dead("c")),
        ]);
        player.play(station);
        assert_eq!(player.backup(), None, "the default first");

        let said = until_stopped(&mut player);
        assert_eq!(
            said,
            [
                None,
                Some("stream 2 of 3".into()),
                Some("stream 3 of 3".into())
            ]
        );
        // The last one failed too: the station's error shows
        assert!(player.is_error);
        assert_ne!(player.status, status::CONNECTING);
        player.shutdown();
    }

    #[test]
    fn a_station_with_one_stream_shows_its_error_at_once() {
        let mut player = player();
        player.play(Station::new("Jazz", dead("a")));
        assert_eq!(until_stopped(&mut player), [None]);
        assert!(player.is_error);
        player.shutdown();
    }

    /// The texts the terminal shows for engine events are the app's: each
    /// is written in the app's controller (as a literal or by name), so a
    /// change there that isn't made here too fails
    #[test]
    fn status_texts_match_the_apps() {
        let app = include_str!("../../radiotrope-app/src/app/controller.rs");
        let source = |text: &str| format!("\"{}\"", text.replace('\u{2026}', "\\u{2026}"));
        for (name, text) in [
            ("CONNECTING", status::CONNECTING),
            ("PLAYING", status::PLAYING),
            ("STOPPED", status::STOPPED),
            ("PAUSED", status::PAUSED),
            ("WAITING_FOR_DATA", status::WAITING_FOR_DATA),
            ("OUTPUT_LOST", status::OUTPUT_LOST),
            ("OUTPUT_NOT_RESPONDING", status::OUTPUT_NOT_RESPONDING),
            ("NOT_RESPONDING", status::NOT_RESPONDING),
            ("NO_AUDIO", status::NO_AUDIO),
        ] {
            assert!(
                app.contains(&source(text)) || app.contains(&format!("status::{name}")),
                "the app no longer says {text:?}"
            );
        }
        assert!(app.contains("\"Buffering {}%\"") || app.contains("status::buffering"));
        assert_eq!(status::buffering(40), "Buffering 40%");
        assert!(
            app.contains("\"Can't start recording: {e}\"") || app.contains("cant_start_recording")
        );
        assert_eq!(
            text::cant_start_recording("disk full"),
            "Can't start recording: disk full"
        );
        // The resolve timeout reads as one
        assert_eq!(
            text::playback_error(&format!(
                "Stream resolution timed out after {} s",
                RESOLVE_TIMEOUT.as_secs()
            )),
            "Station not responding. Try again later."
        );
    }
}
