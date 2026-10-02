//! The player: one audio engine, the station it plays, and its recording
//!
//! Follows the app's controller: a station is resolved on a thread of its
//! own so the screen keeps going, a newer play makes an older resolve
//! stale, and events about a station that was replaced are ignored.

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

use crate::library::{RecordFormat, Settings, Station};
use crate::recording;

/// Longest wait for a station to resolve, a little over the resolver's own
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(RESOLVE_TIMEOUT_SECS + 3);

/// How long a message (a saved recording, an error) stays at the bottom
const NOTICE_TIME: Duration = Duration::from_secs(5);

/// Shown while the output device hangs on opening
const OUTPUT_NOT_RESPONDING: &str = "Audio device not responding";

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
}

type Resolved = (u64, Result<ResolvedStream, String>);

pub struct Player {
    engine: Option<AudioEngine>,
    /// The station playing, or the last one played
    pub station: Option<Station>,
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
            phase: Phase::Stopped,
            status: "Stopped".to_string(),
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

    /// Play `station`, replacing what plays now
    pub fn play(&mut self, station: Station) {
        // Switching station ends the recording of the old one
        self.stop_recording();
        self.cancel_stream();
        self.engine().stop();
        self.metadata_rx = None;
        self.stream_failed = false;

        self.station = Some(station.clone());
        self.phase = Phase::Resolving;
        self.status = "Connecting".to_string();
        self.is_error = false;
        self.codec = None;
        self.bitrate = None;
        self.stream_type = None;
        self.title.clear();
        self.artist.clear();
        self.started_at = None;

        let generation = self.generation;
        let cancel = StreamCancel::new();
        self.cancel = Some(cancel.clone());
        let tx = self.resolved_tx.clone();
        let url = station.url;
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
                    Err(format!("No answer after {}s", RESOLVE_TIMEOUT.as_secs()))
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
        self.status = "Stopped".to_string();
        self.is_error = false;
        self.started_at = None;
        self.title.clear();
        self.artist.clear();
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
            .is_some_and(|n| n.shown_at.elapsed() > NOTICE_TIME)
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
                self.cancel = None;
                self.phase = Phase::Stopped;
                self.status = e;
                self.is_error = true;
            }
        }
    }

    fn on_event(&mut self, event: EngineEvent) {
        // An earlier station's last words
        if event.stream.is_some() && event.stream != self.stream_id {
            return;
        }
        let event = event.event;

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
                self.play(station);
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
                self.set_status("Playing", false);
                self.stream_failed = false;
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
                    self.set_status("Stopped", false);
                }
            }
            AudioEvent::Paused => self.set_status("Paused", false),
            AudioEvent::Resumed => self.set_status("Playing", false),
            AudioEvent::Error(e) => {
                self.status = e;
                self.is_error = true;
                self.stream_failed = true;
            }
            AudioEvent::Buffering(pct) => {
                if pct < 100 {
                    self.set_status(&format!("Buffering {pct}%"), false);
                } else {
                    self.set_status("Playing", false);
                }
            }
            AudioEvent::StreamStalled => self.set_status("Stalled", true),
            AudioEvent::StreamRecovered => self.set_status("Playing", false),
            AudioEvent::OutputLost => {
                self.set_status("Audio output lost, waiting for a device", true)
            }
            AudioEvent::OutputNotResponding => {
                self.output_hang = Some((self.status.clone(), self.is_error));
                self.set_status(OUTPUT_NOT_RESPONDING, true);
            }
            AudioEvent::OutputResponding => {
                if let Some((status, is_error)) = self.output_hang.take() {
                    if self.status == OUTPUT_NOT_RESPONDING {
                        self.status = status;
                        self.is_error = is_error;
                    }
                }
            }
            AudioEvent::OutputRestored => {
                let status = match self.phase {
                    Phase::Stopped => "Stopped",
                    _ => "Playing",
                };
                self.set_status(status, false);
            }
            AudioEvent::ProbeTimeout => self.set_status("Format not recognised", true),
            AudioEvent::NoAudioTimeout => {
                self.set_status("No audio", true);
                self.stream_failed = true;
            }
            AudioEvent::FellBehind => {}
        }
    }

    fn set_status(&mut self, status: &str, is_error: bool) {
        self.status = status.to_string();
        self.is_error = is_error;
    }

    pub fn notify(&mut self, text: impl Into<String>, is_error: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error,
            shown_at: Instant::now(),
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
            self.notify(format!("Recording folder not available. {e}"), true);
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
            Err(e) => self.notify(format!("Could not start recording: {e}"), true),
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

    fn report_saved(&mut self, status: &RecordingStatus) {
        let name = status
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match &status.error {
            Some(e) => self.notify(format!("Recording stopped: {e}. Saved {name}"), true),
            None => self.notify(format!("Saved {name}"), false),
        }
    }

    /// Save any recording and close the engine
    pub fn shutdown(mut self) {
        self.stop_recording();
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
        }
    }
}
