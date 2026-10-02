//! Application controller
//!
//! Owns the audio engine, shared state, and processes commands from all
//! frontends (GUI, MCP, tray) through a single crossbeam channel.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crossbeam_channel::{never, select, tick, Receiver, Sender};

use radiotrope::audio::{
    AudioAnalysis, AudioEngine, AudioEvent, EngineEvent, PlaybackState, RecordingFormat,
    RecordingOptions, RecordingStatus, RecordingTags, SharedStats, StreamId, TapPoint,
};
use radiotrope::config::timeouts::RESOLVE_TIMEOUT_SECS;
use radiotrope::stream::metadata::StreamMetadata;
use radiotrope::stream::{StreamCancel, StreamResolver, StreamType};
use radiotrope_app::data::recordings;

use super::state::{AppCommand, AppSnapshot, RecordingNotice, RecordingProgress};

/// Timeout for stream resolution — if the server doesn't respond within this
/// duration the resolve attempt is abandoned. The engine ends a resolve by
/// its own deadline with the reason; this is a little longer, as a backstop.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(RESOLVE_TIMEOUT_SECS + 3);

/// How often a recording's progress is read while one runs (the UI shows
/// it about as often)
const RECORDING_POLL: Duration = Duration::from_millis(200);

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
}

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
                self.engine = Some(engine);
            }
            Err(e) => {
                // Not a missing device (the engine waits for one): say why
                // nothing will play
                eprintln!("Failed to initialize audio engine: {e}");
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.status_text = format!("Error: {e}").into();
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
        let no_tick = never();
        let mut recording = false;
        loop {
            let metadata = self.metadata_rx.clone().unwrap_or_else(never);
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
            }
            recording = self.poll_recording();
        }

        // Finish any recording before the engine goes away
        self.stop_recording();

        // Shutdown engine
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
        }
    }

    /// Handle a single command. Returns true if the loop should exit.
    fn handle_command(&mut self, cmd: AppCommand) -> bool {
        match cmd {
            AppCommand::Shutdown => return true,

            AppCommand::Play {
                url,
                name,
                logo_url,
                country,
                taken,
            } => {
                let seq = self.start_stream(&url, name, logo_url, country);
                if let Some(taken) = taken {
                    // An agent that stopped waiting has dropped its end
                    let _ = taken.send(seq);
                }
            }
            AppCommand::Stop => {
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
                state.playback = PlaybackState::Stopped;
                state.status_text = "Stopped".into();
                state.is_error = false;
                state.title.clear();
                state.artist.clear();
            }
            AppCommand::SetVolume(vol) => {
                // Keep NaN out of the shared state and the saved settings
                if !vol.is_finite() {
                    return false;
                }
                let vol = vol.clamp(0.0, 1.0);
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.volume = vol;
                // Auto-unmute when volume is changed to a non-zero value
                if state.is_muted && vol > 0.0 {
                    state.is_muted = false;
                }
                // When muted, engine stays at 0; otherwise apply the new volume
                let engine_vol = if state.is_muted { 0.0 } else { vol };
                drop(state);
                if let Some(engine) = &self.engine {
                    engine.set_volume(engine_vol);
                }
            }
            AppCommand::Mute => {
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                self.volume_before_mute = state.volume;
                state.is_muted = true;
                drop(state);
                if let Some(engine) = &self.engine {
                    engine.set_volume(0.0);
                }
            }
            AppCommand::Unmute => {
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.is_muted = false;
                state.volume = self.volume_before_mute;
                let vol = self.volume_before_mute;
                drop(state);
                if let Some(engine) = &self.engine {
                    engine.set_volume(vol);
                }
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
            }
            AppCommand::SetEqEnabled(on) => {
                if let Some(engine) = &self.engine {
                    engine.set_eq_enabled(on);
                }
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.eq_enabled = on;
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
                self.stop_recording();
            }
            AppCommand::InternalStreamResolved { generation, result } => {
                self.handle_stream_resolved(generation, result);
            }
        }
        false
    }

    /// Resolve the stream on a worker thread, then send the result back.
    /// Returns the station's `play_seq`.
    ///
    /// Each call increments `resolve_generation`; stale results from earlier
    /// calls are discarded in `handle_stream_resolved`.
    fn start_stream(
        &mut self,
        url: &str,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
    ) -> u64 {
        // Switching station ends the recording of the old one
        self.stop_recording();

        // Stop any current playback first, and any station still resolving
        self.cancel_stream();
        if let Some(engine) = &self.engine {
            engine.stop();
        }
        self.metadata_rx = None;

        self.stream_failed = false;

        let generation = self.resolve_generation;
        let cancel = StreamCancel::new();
        self.stream_cancel = Some(cancel.clone());

        let seq = {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            state.play_seq += 1;
            state.station_url = Some(url.to_string());
            state.station_name = name;
            state.station_logo_url = logo_url.filter(|logo| !logo.is_empty());
            state.station_country = country.filter(|country| !country.is_empty());
            state.title.clear();
            state.artist.clear();
            state.last_error = None;
            state.is_resolving = true;
            state.codec_name.clear();
            state.stream_type.clear();
            state.sample_rate = 0;
            state.channels = 0;
            state.bitrate = None;
            state.status_text = "Resolving...".into();
            state.is_error = false;
            // The old station was stopped above. Its own Stopped is ignored
            // like the rest of its events.
            state.playback = PlaybackState::Stopped;
            state.play_seq
        };

        let url: Arc<str> = Arc::from(url);
        let cmd_tx = self.cmd_tx.clone();

        std::thread::Builder::new()
            .name("stream-resolve".into())
            .spawn(move || {
                // Run the actual resolve on a nested thread so we can enforce a timeout
                let (tx, rx) = crossbeam_channel::bounded(1);
                let url_inner = Arc::clone(&url);
                let cancel_inner = cancel.clone();
                std::thread::Builder::new()
                    .name("stream-resolve-inner".into())
                    .spawn(move || {
                        let result = StreamResolver::resolve_cancellable(&url_inner, &cancel_inner)
                            .map_err(|e| e.to_string());
                        let _ = tx.send(result);
                    })
                    .expect("Failed to spawn stream-resolve-inner thread");

                let result = match rx.recv_timeout(RESOLVE_TIMEOUT) {
                    Ok(r) => r,
                    Err(_) => {
                        // Stop the resolve, which would otherwise carry on
                        cancel.cancel();
                        Err(format!(
                            "Stream resolution timed out after {}s for: {url}",
                            RESOLVE_TIMEOUT.as_secs()
                        ))
                    }
                };

                let _ = cmd_tx.send(AppCommand::InternalStreamResolved { generation, result });
            })
            .expect("Failed to spawn stream-resolve thread");
        seq
    }

    /// Stop the station being resolved or played, and make any resolve
    /// result still on its way stale, and any event about the station
    fn cancel_stream(&mut self) {
        if let Some(cancel) = self.stream_cancel.take() {
            cancel.cancel();
        }
        self.resolve_generation += 1;
        self.stream_id = None;
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
                {
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
                    state.status_text = "Connecting...".into();
                    state.is_error = false;
                }

                // Store metadata receiver for polling
                let mut resolved = resolved;
                self.metadata_rx = resolved.metadata_rx.take();

                // Start playback. The engine cancels the stream when it stops.
                if let Some(engine) = &self.engine {
                    self.stream_id = Some(engine.play_stream(resolved));
                }
            }
            Err(e) => {
                eprintln!("Stream resolution failed: {e}");
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.playback = PlaybackState::Stopped;
                state.station_url = None;
                state.last_error = Some(e.clone());
                state.is_resolving = false;
                state.status_text = format!("Error: {e}").into();
                state.is_error = true;
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
        let event = event.event;

        // The stream ended or failed: keep what was recorded
        if matches!(
            event,
            AudioEvent::Stopped | AudioEvent::Error(_) | AudioEvent::NoAudioTimeout
        ) {
            self.stop_recording();
        }

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
                self.start_stream(&url, name, logo_url, country);
            }
            return;
        }

        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        // While a new station resolves, news of the device waits: the
        // engine repeats a lost device once the station plays
        if state.is_resolving {
            return;
        }
        match event {
            AudioEvent::Playing(codec_info) => {
                state.playback = PlaybackState::Playing;
                state.codec_name = codec_info.codec_name;
                state.channels = codec_info.channels;
                state.sample_rate = codec_info.sample_rate;
                // Prefer ICY/HLS bitrate (already stored), fall back to codec-detected
                if state.bitrate.is_none() {
                    state.bitrate = codec_info.bitrate;
                }
                state.status_text = "Playing".into();
                state.is_error = false;
                self.stream_failed = false;
            }
            // AAC and AAC+, or a chained Ogg stream's next codec
            AudioEvent::CodecChanged(codec_info) => {
                state.codec_name = codec_info.codec_name;
            }
            AudioEvent::Stopped => {
                state.playback = PlaybackState::Stopped;
                // A stream that failed stops with its error still showing
                if !std::mem::take(&mut self.stream_failed) {
                    state.status_text = "Stopped".into();
                    state.is_error = false;
                }
            }
            AudioEvent::Paused => {
                state.playback = PlaybackState::Paused;
                state.status_text = "Paused".into();
                state.is_error = false;
                self.stream_failed = false;
            }
            AudioEvent::Resumed => {
                state.playback = PlaybackState::Playing;
                state.status_text = "Playing".into();
                state.is_error = false;
                self.stream_failed = false;
            }
            AudioEvent::Error(ref e) => {
                eprintln!("Engine error: {e}");
                state.last_error = Some(e.clone());
                state.status_text = format!("Error: {e}").into();
                state.is_error = true;
                self.stream_failed = true;
            }
            AudioEvent::Buffering(pct) => {
                state.status_text = if pct < 100 {
                    format!("Buffering {}%", pct).into()
                } else {
                    "Playing".into()
                };
                state.is_error = false;
            }
            AudioEvent::StreamStalled => {
                state.status_text = "Stalled".into();
                state.is_error = true;
            }
            AudioEvent::StreamRecovered => {
                state.status_text = "Playing".into();
                state.is_error = false;
            }
            AudioEvent::OutputLost => {
                // The station stays loaded, and so does a recording
                state.status_text = "Audio output lost, waiting for a device".into();
                state.is_error = true;
            }
            AudioEvent::OutputRestored => {
                state.status_text = match state.playback {
                    PlaybackState::Playing => "Playing",
                    PlaybackState::Paused => "Paused",
                    PlaybackState::Stopped => "Stopped",
                }
                .into();
                state.is_error = false;
            }
            AudioEvent::ProbeTimeout => {
                state.status_text = "Probe timeout".into();
                state.is_error = true;
            }
            AudioEvent::NoAudioTimeout => {
                state.status_text = "No audio".into();
                state.is_error = true;
                self.stream_failed = true;
            }
            // Handled above
            AudioEvent::FellBehind => {}
        }
    }

    /// Start recording the playing station into `folder`.
    fn start_recording(
        &mut self,
        folder: &Path,
        format: RecordingFormat,
        bitrate: Option<u32>,
        with_eq: bool,
        cover: Option<Vec<u8>>,
    ) {
        let Some(engine) = &self.engine else { return };
        let recorder = engine.recorder();
        if recorder.is_recording() {
            return;
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
            self.notify("Start a station before recording", true);
            return;
        }

        if let Err(e) = recordings::prepare_dir(folder) {
            self.notify(&format!("Recording folder not available. {e}"), true);
            return;
        }

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
            }
            Err(e) => self.notify(&format!("Could not start recording: {e}"), true),
        }
    }

    /// Stop the running recording, if any, and say where it was saved.
    fn stop_recording(&mut self) {
        let Some(engine) = &self.engine else { return };
        let Some(status) = engine.recorder().stop() else {
            return;
        };
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
            Some(e) => self.notify(&format!("Recording stopped: {e}. Saved {name}"), true),
            None => self.notify(&format!("Saved {name}"), false),
        }
    }

    fn notify(&mut self, text: &str, is_error: bool) {
        self.notice_seq += 1;
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.recording_notice = Some(RecordingNotice {
            seq: self.notice_seq,
            text: text.to_string(),
            is_error,
        });
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
        fn a_nan_volume_is_ignored() {
            let (mut controller, state) = controller();
            controller.handle_command(AppCommand::SetVolume(0.4));
            controller.handle_command(AppCommand::SetVolume(f32::NAN));
            assert_eq!(state.lock().unwrap().volume, 0.4);
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
                name: None,
                logo_url: None,
                country: None,
                taken: None,
            });
            let (taken, mut seq) = tokio::sync::oneshot::channel();
            controller.handle_command(AppCommand::Play {
                url: silent_station(),
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
        // What the engine sends when a station goes down for good
        controller.handle_engine_event(on(
            1,
            AudioEvent::Error("Stream error: No audio for 2 min: HTTP 404 Not Found".into()),
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
        assert_eq!(state.status_text, "No audio");
    }

    /// What `start_stream` does while the next station resolves
    fn resolving(controller: &mut AppController, state: &Arc<Mutex<AppSnapshot>>) {
        controller.cancel_stream();
        let mut state = state.lock().unwrap();
        state.is_resolving = true;
        state.status_text = "Resolving...".into();
        state.is_error = false;
        state.playback = PlaybackState::Stopped;
    }

    /// What `handle_stream_resolved` does once it hands station `id` over
    fn connecting(controller: &mut AppController, state: &Arc<Mutex<AppSnapshot>>, id: u64) {
        controller.stream_id = Some(StreamId(id));
        let mut state = state.lock().unwrap();
        state.is_resolving = false;
        state.status_text = "Connecting...".into();
    }

    #[test]
    fn a_station_that_fails_after_another_shows_stopped_with_its_error() {
        let (mut controller, state) = controller_playing();
        // Switch: the engine stops the old station while the new resolves
        resolving(&mut controller, &state);
        controller.handle_engine_event(on(1, AudioEvent::Stopped));
        assert_eq!(state.lock().unwrap().playback, PlaybackState::Stopped);
        assert_eq!(state.lock().unwrap().status_text, "Resolving...");

        // The new station fails to start
        connecting(&mut controller, &state, 2);
        controller.handle_engine_event(on(2, AudioEvent::Error("Probe failed".into())));
        controller.handle_engine_event(on(2, AudioEvent::Stopped));
        let state = state.lock().unwrap();
        assert_eq!(state.playback, PlaybackState::Stopped);
        assert!(state.is_error);
        assert!(state.status_text.contains("Probe failed"));
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
            assert_eq!(state.status_text, "Resolving...");
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
            assert_eq!(state.status_text, "Connecting...");
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
            assert!(state.status_text.contains("waiting for a device"));
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
            assert_eq!(state.status_text, "Resolving...");
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
