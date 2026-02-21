//! Application controller
//!
//! Owns the audio engine, shared state, and processes commands from all
//! frontends (GUI, MCP, tray) through a single crossbeam channel.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use radiotrope::audio::{AudioAnalysis, AudioEngine, AudioEvent, PlaybackState, SharedStats};
use radiotrope::stream::metadata::StreamMetadata;
use radiotrope::stream::{StreamResolver, StreamType};

use super::state::{AppCommand, AppSnapshot};

/// Timeout for stream resolution — if the server doesn't respond within this
/// duration the resolve attempt is abandoned.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

pub struct AppController {
    cmd_rx: Receiver<AppCommand>,
    cmd_tx: Sender<AppCommand>,
    shared_state: Arc<Mutex<AppSnapshot>>,
    engine: Option<AudioEngine>,
    metadata_rx: Option<crossbeam_channel::Receiver<StreamMetadata>>,
    /// Monotonically increasing counter to discard stale resolve results
    resolve_generation: u64,
    /// One-shot channel to send the engine's analysis Arc to the UI thread
    analysis_tx: Option<Sender<Arc<Mutex<AudioAnalysis>>>>,
    /// One-shot channel to send the engine's SharedStats to the UI thread
    stats_tx: Option<Sender<SharedStats>>,
    /// Saved volume level before mute (for restoring on unmute)
    volume_before_mute: f32,
    /// Reusable buffer for collecting engine events (avoids allocation per poll)
    event_buf: Vec<AudioEvent>,
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
            analysis_tx: Some(analysis_tx),
            stats_tx: Some(stats_tx),
            volume_before_mute: 1.0,
            event_buf: Vec::new(),
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
                eprintln!("Failed to initialize audio engine: {e}");
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
        }

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
                if let Some(engine) = &self.engine {
                    engine.stop();
                }
                self.metadata_rx = None;
                let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
                state.playback = PlaybackState::Stopped;
                state.status_text = "Ready".into();
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
            AppCommand::AddFavorite { name, url } => {
                let _ = (name, url);
                // TODO: add to favorites manager
            }
            AppCommand::RemoveFavorite(id) => {
                let _ = id;
                // TODO: remove from favorites manager
            }
            AppCommand::Search(query) => {
                let _ = query;
                // TODO: search via provider registry
            }
            AppCommand::GetState => {
                // No-op: MCP reads shared_state directly via Arc<Mutex<>>
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
        // Stop any current playback first
        if let Some(engine) = &self.engine {
            engine.stop();
        }
        self.metadata_rx = None;

        // Bump generation so any in-flight resolve becomes stale
        self.resolve_generation += 1;
        let generation = self.resolve_generation;

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
                std::thread::Builder::new()
                    .name("stream-resolve-inner".into())
                    .spawn(move || {
                        let result = StreamResolver::resolve(&url_inner).map_err(|e| e.to_string());
                        let _ = tx.send(result);
                    })
                    .expect("Failed to spawn stream-resolve-inner thread");

                let result = match rx.recv_timeout(RESOLVE_TIMEOUT) {
                    Ok(r) => r,
                    Err(_) => Err(format!(
                        "Stream resolution timed out after {}s for: {url}",
                        RESOLVE_TIMEOUT.as_secs()
                    )),
                };

                let _ = cmd_tx.send(AppCommand::InternalStreamResolved { generation, result });
            })
            .expect("Failed to spawn stream-resolve thread");
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
                    // Prefer stream-provided name, but keep pre-set name from Play command
                    if resolved.info.station_name.is_some() {
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
                self.metadata_rx = resolved.metadata_rx;

                // Start playback
                if let Some(engine) = &self.engine {
                    engine.play_with_stats(
                        resolved.reader,
                        resolved.info.format_hint,
                        resolved.info.bitrate,
                        resolved.bytes_received,
                        resolved.segments_downloaded,
                    );
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
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
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
            }
            AudioEvent::Stopped => {
                // Don't overwrite "Resolving..." when stopping the old stream
                // before a new one starts
                if !state.is_resolving {
                    state.playback = PlaybackState::Stopped;
                    state.status_text = "Ready".into();
                    state.is_error = false;
                }
            }
            AudioEvent::Paused => {
                state.playback = PlaybackState::Paused;
                state.status_text = "Paused".into();
                state.is_error = false;
            }
            AudioEvent::Resumed => {
                state.playback = PlaybackState::Playing;
                state.status_text = "Playing".into();
                state.is_error = false;
            }
            AudioEvent::Error(ref e) => {
                eprintln!("Engine error: {e}");
                state.last_error = Some(e.clone());
                state.status_text = format!("Error: {e}").into();
                state.is_error = true;
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
            AudioEvent::ProbeTimeout => {
                state.status_text = "Probe timeout".into();
                state.is_error = true;
            }
            AudioEvent::NoAudioTimeout => {
                state.status_text = "No audio".into();
                state.is_error = true;
            }
        }
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
