//! Application controller
//!
//! Owns the audio engine, shared state, and processes commands from all
//! frontends (GUI, MCP, tray) through a single crossbeam channel.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use radiotrope::audio::{
    AudioAnalysis, AudioEngine, AudioEvent, PlaybackState, RecordingFormat, RecordingOptions,
    RecordingStatus, RecordingTags, SharedStats, TapPoint,
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
    /// One-shot channel to send the engine's analysis Arc to the UI thread
    analysis_tx: Option<Sender<Arc<Mutex<AudioAnalysis>>>>,
    /// One-shot channel to send the engine's SharedStats to the UI thread
    stats_tx: Option<Sender<SharedStats>>,
    /// Saved volume level before mute (for restoring on unmute)
    volume_before_mute: f32,
    /// Reusable buffer for collecting engine events (avoids allocation per poll)
    event_buf: Vec<AudioEvent>,
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
            analysis_tx: Some(analysis_tx),
            stats_tx: Some(stats_tx),
            volume_before_mute: 1.0,
            event_buf: Vec::new(),
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

        loop {
            // Process commands (blocking with timeout so we can poll engine events)
            match self.cmd_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(cmd) => {
                    if self.handle_command(cmd) {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }

            // Poll engine events
            self.poll_engine_events();
            self.poll_recording();
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

            AppCommand::Play { url, name } => {
                self.start_stream(&url, name);
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
            AppCommand::Pause => {
                if let Some(engine) = &self.engine {
                    engine.pause();
                }
            }
            AppCommand::Resume => {
                if let Some(engine) = &self.engine {
                    engine.resume();
                }
            }
            AppCommand::SetVolume(vol) => {
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
                    if let Some(engine) = &self.engine {
                        engine.set_eq_gains(preset.gains, Some(preset.name.to_string()));
                    }
                    let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    state.eq_gains = preset.gains;
                    state.eq_preset_name = Some(preset.name.to_string());
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
            AppCommand::GetState => {
                // No-op: MCP reads shared_state directly via Arc<Mutex<>>
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
    ///
    /// Each call increments `resolve_generation`; stale results from earlier
    /// calls are discarded in `handle_stream_resolved`.
    fn start_stream(&mut self, url: &str, name: Option<String>) {
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

        {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            state.station_url = Some(url.to_string());
            state.station_name = name;
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
        }

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
    }

    /// Stop the station being resolved or played, and make any resolve
    /// result still on its way stale
    fn cancel_stream(&mut self) {
        if let Some(cancel) = self.stream_cancel.take() {
            cancel.cancel();
        }
        self.resolve_generation += 1;
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
                    engine.play_stream(resolved);
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

    /// Poll audio engine events and metadata
    fn poll_engine_events(&mut self) {
        // Collect events into reusable buffer to avoid borrow conflict with self
        self.event_buf.clear();
        if let Some(engine) = &self.engine {
            while let Some(event) = engine.try_recv_event() {
                self.event_buf.push(event);
            }
        } else {
            return;
        }

        // Temporarily take ownership of the buffer so we can iterate + call &mut self
        let mut buf = std::mem::take(&mut self.event_buf);
        for event in buf.drain(..) {
            self.handle_engine_event(event);
        }
        self.event_buf = buf; // put back (empty but retains capacity)

        // Poll metadata
        self.poll_metadata();
    }

    fn handle_engine_event(&mut self, event: AudioEvent) {
        // The stream ended or failed: keep what was recorded
        if matches!(
            event,
            AudioEvent::Stopped | AudioEvent::Error(_) | AudioEvent::NoAudioTimeout
        ) {
            self.stop_recording();
        }

        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        // While a new station resolves the engine has nothing of it yet:
        // whatever arrives is the old station's last word. Only its stop
        // matters, so the old station no longer shows as playing.
        if state.is_resolving {
            if matches!(event, AudioEvent::Stopped) {
                state.playback = PlaybackState::Stopped;
            }
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
            AudioEvent::MetadataUpdate { title, artist } => {
                state.title = title;
                state.artist = artist;
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
    /// if writing failed (e.g. the disk is full).
    fn poll_recording(&mut self) {
        let Some(engine) = &self.engine else { return };
        let Some(status) = engine.recorder().status() else {
            return;
        };
        if status.error.is_some() {
            self.stop_recording();
            return;
        }
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.recording = Some(RecordingProgress {
            path: status.path,
            duration: status.duration,
            bytes: status.bytes_written,
        });
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

    fn poll_metadata(&mut self) {
        let rx = match &self.metadata_rx {
            Some(rx) => rx,
            None => return,
        };

        // Drain all pending metadata, keep the latest
        let mut latest = None;
        while let Ok(meta) = rx.try_recv() {
            latest = Some(meta);
        }

        if let Some(meta) = latest {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(title) = meta.title {
                state.title = title;
            }
            if let Some(artist) = meta.artist {
                state.artist = artist;
            }
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

    mod output_device {
        use super::*;

        fn controller_and_state() -> (AppController, Arc<Mutex<AppSnapshot>>) {
            let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
            let (analysis_tx, _) = crossbeam_channel::unbounded();
            let (stats_tx, _) = crossbeam_channel::unbounded();
            let state = Arc::new(Mutex::new(AppSnapshot::default()));
            let controller =
                AppController::new(cmd_rx, cmd_tx, state.clone(), analysis_tx, stats_tx);
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

        #[test]
        fn a_lost_output_shows_until_a_device_is_back() {
            let (mut controller, state) = controller_and_state();
            controller.handle_engine_event(playing());
            controller.handle_engine_event(AudioEvent::OutputLost);
            {
                let state = state.lock().unwrap();
                assert!(state.is_error);
                assert!(state.status_text.contains("output lost"));
                // The station is still loaded
                assert_eq!(state.playback, PlaybackState::Playing);
            }

            controller.handle_engine_event(AudioEvent::OutputRestored);
            let state = state.lock().unwrap();
            assert!(!state.is_error);
            assert_eq!(state.status_text, "Playing");
        }

        #[test]
        fn a_device_back_while_paused_shows_paused() {
            let (mut controller, state) = controller_and_state();
            controller.handle_engine_event(playing());
            controller.handle_engine_event(AudioEvent::OutputLost);
            controller.handle_engine_event(AudioEvent::Paused);
            controller.handle_engine_event(AudioEvent::OutputRestored);

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
        fn a_stop_while_resolving_ends_the_resolve_and_stays_stopped() {
            let (mut controller, state) = controller();
            controller.start_stream(&silent_station(), Some("Silent FM".into()));
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
        fn switching_station_cancels_the_one_resolving() {
            let (mut controller, state) = controller();
            let station = silent_station();
            controller.start_stream(&station, Some("First".into()));
            let first = controller.stream_cancel.clone().unwrap();
            controller.start_stream(&station, Some("Second".into()));
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

    #[test]
    fn a_failed_stream_stops_with_its_error_showing() {
        let (mut controller, state) = controller();
        controller.handle_engine_event(playing());
        // What the engine sends when a station goes down for good
        controller.handle_engine_event(AudioEvent::Error(
            "Stream error: No audio for 2 min: HTTP 404 Not Found".into(),
        ));
        controller.handle_engine_event(AudioEvent::Stopped);

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
    fn no_audio_stays_showing_after_the_stop() {
        let (mut controller, state) = controller();
        controller.handle_engine_event(playing());
        controller.handle_engine_event(AudioEvent::NoAudioTimeout);
        controller.handle_engine_event(AudioEvent::Stopped);

        let state = state.lock().unwrap();
        assert_eq!(state.playback, PlaybackState::Stopped);
        assert!(state.is_error);
        assert_eq!(state.status_text, "No audio");
    }

    /// What `start_stream` shows while the next station resolves
    fn resolving(state: &Arc<Mutex<AppSnapshot>>) {
        let mut state = state.lock().unwrap();
        state.is_resolving = true;
        state.status_text = "Resolving...".into();
        state.is_error = false;
    }

    /// What `handle_stream_resolved` shows once it hands the station over
    fn connecting(state: &Arc<Mutex<AppSnapshot>>) {
        let mut state = state.lock().unwrap();
        state.is_resolving = false;
        state.status_text = "Connecting...".into();
    }

    #[test]
    fn a_station_that_fails_after_another_shows_stopped_with_its_error() {
        let (mut controller, state) = controller();
        controller.handle_engine_event(playing());
        // Switch: the engine stops the old station while the new resolves
        resolving(&state);
        controller.handle_engine_event(AudioEvent::Stopped);
        assert_eq!(state.lock().unwrap().playback, PlaybackState::Stopped);
        assert_eq!(state.lock().unwrap().status_text, "Resolving...");

        // The new station fails to start
        connecting(&state);
        controller.handle_engine_event(AudioEvent::Error("Probe failed".into()));
        controller.handle_engine_event(AudioEvent::Stopped);
        let state = state.lock().unwrap();
        assert_eq!(state.playback, PlaybackState::Stopped);
        assert!(state.is_error);
        assert!(state.status_text.contains("Probe failed"));
    }

    #[test]
    fn the_old_station_changes_nothing_while_the_next_resolves() {
        let (mut controller, state) = controller();
        controller.handle_engine_event(playing());
        resolving(&state);
        for event in [
            AudioEvent::Buffering(40),
            AudioEvent::StreamStalled,
            AudioEvent::MetadataUpdate {
                title: "Old song".into(),
                artist: "Old artist".into(),
            },
            AudioEvent::Error("Stream error: old".into()),
            AudioEvent::NoAudioTimeout,
            AudioEvent::Stopped,
        ] {
            controller.handle_engine_event(event);
        }
        {
            let state = state.lock().unwrap();
            assert_eq!(state.status_text, "Resolving...");
            assert!(!state.is_error);
            assert!(state.title.is_empty());
            assert_eq!(state.playback, PlaybackState::Stopped);
        }

        // The old station's failure doesn't stick to the new one
        connecting(&state);
        controller.handle_engine_event(playing());
        controller.handle_engine_event(AudioEvent::Stopped);
        let state = state.lock().unwrap();
        assert!(!state.is_error);
        assert_eq!(state.status_text, "Stopped");
    }

    #[test]
    fn no_device_at_start_says_so() {
        let (mut controller, state) = controller();
        controller.handle_engine_event(AudioEvent::OutputLost);
        {
            let state = state.lock().unwrap();
            assert!(state.is_error);
            assert!(state.status_text.contains("waiting for a device"));
        }
        controller.handle_engine_event(AudioEvent::OutputRestored);
        let state = state.lock().unwrap();
        assert!(!state.is_error);
        assert_eq!(state.status_text, "Stopped");
    }

    #[test]
    fn a_stop_after_playing_again_is_a_plain_stop() {
        let (mut controller, state) = controller();
        controller.handle_engine_event(playing());
        controller.handle_engine_event(AudioEvent::Error("Stream error: x".into()));
        controller.handle_engine_event(AudioEvent::Stopped);
        controller.handle_engine_event(playing());
        controller.handle_engine_event(AudioEvent::Stopped);

        let state = state.lock().unwrap();
        assert!(!state.is_error);
        assert_eq!(state.status_text, "Stopped");
    }
}
