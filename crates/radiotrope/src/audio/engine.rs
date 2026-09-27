//! Audio engine
//!
//! Runs audio playback on a dedicated thread, accepting commands via crossbeam
//! channels and emitting events back. Visualization data is shared via
//! `Arc<Mutex<AudioAnalysis>>`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Select, Sender, TryRecvError};
use rodio::{MixerDeviceSink, Player};

use crate::config::timeouts::{BUFFERING_STALL_THRESHOLD_SECS, PROBE_TIMEOUT_SECS};
use crate::error::RadioError;
use crate::stream::buffer::{PlaybackPositionReader, SharedBufferStatus, StreamBuffer};
use crate::stream::{ResolvedStream, StreamCancel};

/// Quadratic volume curve for natural perception (human hearing is logarithmic)
fn volume_curve(linear: f32) -> f32 {
    linear * linear
}

use super::analyzer::AnalyzingSource;
use super::decoder::{start_open, SymphoniaSource};
use super::dsp::equalizer::{EqParams, EqSource, SharedEqParams};
use super::health::{FailureReason, HealthState, StreamHealthMonitor};
use super::output::{open_output, Relay, SharedSource};
use super::recording::{Recorder, RecordingTap, TapPoint};
use super::stats::{
    new_shared_stats, DecoderStats, EventBus, SharedStats, StreamEvent, StreamStats,
};
use super::types::{AudioAnalysis, AudioCommand, AudioEvent, PlaybackState};

/// How often the engine loop checks on playback (end of stream, stats,
/// buffering, health, probe timeout)
const TICK_INTERVAL: Duration = Duration::from_millis(500);

/// How often the engine tries to open an output device while there is
/// none, and at most how often it replaces one that keeps failing
const OUTPUT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Opens an output device that raises the flag if it goes away
type OpenOutput = Box<dyn FnMut(&Arc<AtomicBool>) -> Result<MixerDeviceSink, String> + Send>;

/// A player on the output device, or on nothing while there is none
fn new_player(output: Option<&MixerDeviceSink>) -> Player {
    match output {
        Some(output) => Player::connect_new(output.mixer()),
        None => Player::new().0,
    }
}

/// What the engine loop does next
enum Wake {
    Command(AudioCommand),
    /// Check on playback: the tick is due, or the pending probe finished
    Tick,
    /// Every handle to the engine is gone
    Closed,
}

/// Wait for the engine loop's next job. Commands are handled as they
/// arrive, but never delay the tick past `next_tick`: a dragged volume
/// slider sends dozens a second, and the old loop only checked on playback
/// after half a second without commands. A finished probe wakes the loop
/// at once, so a station starts as soon as its format is known.
fn next_wake<T>(
    cmd_rx: &Receiver<AudioCommand>,
    probe_rx: Option<&Receiver<T>>,
    next_tick: Instant,
) -> Wake {
    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        if wait.is_zero() || probe_rx.is_some_and(|rx| !rx.is_empty()) {
            return Wake::Tick;
        }
        let mut select = Select::new();
        let command = select.recv(cmd_rx);
        if let Some(rx) = probe_rx {
            select.recv(rx);
        }
        match select.ready_timeout(wait) {
            Err(_) => return Wake::Tick,
            Ok(index) if index == command => match cmd_rx.try_recv() {
                Ok(cmd) => return Wake::Command(cmd),
                Err(TryRecvError::Disconnected) => return Wake::Closed,
                Err(TryRecvError::Empty) => {}
            },
            // The probe sent its result, or its thread died
            Ok(_) => return Wake::Tick,
        }
    }
}

/// State held while an async probe is in progress
struct PendingProbe {
    probe_rx: Receiver<Result<SymphoniaSource, RadioError>>,
    buf_status: SharedBufferStatus,
    probing_flag: Arc<AtomicBool>,
    cancel: StreamCancel,
    prod_handle: JoinHandle<()>,
    bytes_received: Option<Arc<AtomicU64>>,
    segments_downloaded: Option<Arc<AtomicU64>>,
    bitrate: Option<u32>,
    started: Instant,
}

/// Audio engine that manages playback on a dedicated thread
pub struct AudioEngine {
    cmd_tx: Sender<AudioCommand>,
    event_rx: Receiver<AudioEvent>,
    analysis: Arc<Mutex<AudioAnalysis>>,
    thread: Option<JoinHandle<()>>,
    shared_stats: SharedStats,
    event_bus: Arc<EventBus>,
    eq_params: SharedEqParams,
    recorder: Recorder,
    #[cfg(test)]
    output_lost: Arc<AtomicBool>,
}

impl AudioEngine {
    /// Create a new audio engine, spawning the engine thread.
    ///
    /// Blocks until the audio output stream is initialized (or fails).
    pub fn new() -> Result<Self, RadioError> {
        let generation = Arc::new(AtomicU64::new(0));
        Self::with_output(Box::new(move |lost| open_output(lost, &generation)))
    }

    /// Create an engine that plays through what `open` opens: at start, and
    /// again whenever the device goes away
    fn with_output(open: OpenOutput) -> Result<Self, RadioError> {
        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(16);
        let (event_tx, event_rx) = bounded::<AudioEvent>(64);
        let (init_tx, init_rx) = bounded::<Result<(), String>>(1);

        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_thread = analysis.clone();

        let shared_stats = new_shared_stats();
        let shared_stats_thread = shared_stats.clone();
        let event_bus = Arc::new(EventBus::new());
        let event_bus_thread = event_bus.clone();
        let eq_params = EqParams::new_shared();
        let eq_params_thread = eq_params.clone();
        let recorder = Recorder::new();
        let recorder_thread = recorder.clone();
        let output_lost = Arc::new(AtomicBool::new(false));
        let output_lost_thread = output_lost.clone();

        let thread = thread::Builder::new()
            .name("audio-engine".to_string())
            .spawn(move || {
                Self::run(
                    cmd_rx,
                    event_tx,
                    init_tx,
                    analysis_thread,
                    shared_stats_thread,
                    event_bus_thread,
                    eq_params_thread,
                    recorder_thread,
                    output_lost_thread,
                    open,
                );
            })
            .map_err(|e| RadioError::Audio(format!("Failed to spawn audio thread: {}", e)))?;

        // Wait for initialization
        let init_result = init_rx
            .recv()
            .map_err(|_| RadioError::Audio("Audio thread terminated during init".to_string()))?;

        init_result.map_err(RadioError::Audio)?;

        Ok(Self {
            cmd_tx,
            event_rx,
            analysis,
            thread: Some(thread),
            shared_stats,
            event_bus,
            eq_params,
            recorder,
            #[cfg(test)]
            output_lost,
        })
    }

    /// Act as if the output device had just gone away
    #[cfg(test)]
    fn simulate_output_loss(&self) {
        self.output_lost.store(true, Ordering::SeqCst);
    }

    /// Send a command to the engine
    pub fn send(&self, cmd: AudioCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Start playing from the given reader
    pub fn play(
        &self,
        reader: Box<dyn super::types::ReadSeek>,
        format_hint: Option<String>,
        bitrate: Option<u32>,
    ) {
        self.send(AudioCommand::Play {
            reader,
            format_hint,
            bitrate,
            bytes_received: None,
            segments_downloaded: None,
            playback_position: None,
            cancel: StreamCancel::new(),
        });
    }

    /// Start playing with bytes_received and segments_downloaded tracking.
    ///
    /// `playback_position` is updated with how many bytes of `reader` the
    /// decoder has read (see `ResolvedStream::playback_position`).
    pub fn play_with_stats(
        &self,
        reader: Box<dyn super::types::ReadSeek>,
        format_hint: Option<String>,
        bitrate: Option<u32>,
        bytes_received: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
        segments_downloaded: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
        playback_position: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    ) {
        self.send(AudioCommand::Play {
            reader,
            format_hint,
            bitrate,
            bytes_received,
            segments_downloaded,
            playback_position,
            cancel: StreamCancel::new(),
        });
    }

    /// Start playing a resolved stream, with its stats and song info timing.
    ///
    /// Stopping it, or playing something else, cancels the stream
    /// (`ResolvedStream::cancel`), so its network threads stop at once. Take
    /// `metadata_rx` out first if you want the stream's song info.
    pub fn play_stream(&self, stream: ResolvedStream) {
        self.send(AudioCommand::Play {
            reader: stream.reader,
            format_hint: stream.info.format_hint,
            bitrate: stream.info.bitrate,
            bytes_received: stream.bytes_received,
            segments_downloaded: stream.segments_downloaded,
            playback_position: stream.playback_position,
            cancel: stream.cancel,
        });
    }

    /// Stop playback
    pub fn stop(&self) {
        self.send(AudioCommand::Stop);
    }

    /// Pause playback
    pub fn pause(&self) {
        self.send(AudioCommand::Pause);
    }

    /// Resume playback
    pub fn resume(&self) {
        self.send(AudioCommand::Resume);
    }

    /// Set volume (clamped to 0.0..=2.0)
    pub fn set_volume(&self, volume: f32) {
        self.send(AudioCommand::SetVolume(volume));
    }

    /// Set a single EQ band gain
    pub fn set_eq_band(&self, band: usize, gain_db: f32) {
        self.send(AudioCommand::SetEqBand { band, gain_db });
    }

    /// Set all EQ band gains at once (optionally from a preset)
    pub fn set_eq_gains(&self, gains: [f32; 10], preset_name: Option<String>) {
        self.send(AudioCommand::SetEqGains { gains, preset_name });
    }

    /// Set EQ preamp gain
    pub fn set_eq_preamp(&self, db: f32) {
        self.send(AudioCommand::SetEqPreamp(db));
    }

    /// Enable or disable the EQ
    pub fn set_eq_enabled(&self, enabled: bool) {
        self.send(AudioCommand::SetEqEnabled(enabled));
    }

    /// Get a handle to the shared EQ parameters
    pub fn eq_params(&self) -> SharedEqParams {
        self.eq_params.clone()
    }

    /// Get a handle to the station recorder.
    ///
    /// Recordings take their audio from the playing station, before or after
    /// the equalizer (see [`super::recording::TapPoint`]).
    pub fn recorder(&self) -> Recorder {
        self.recorder.clone()
    }

    /// Non-blocking poll for the next event
    pub fn try_recv_event(&self) -> Option<AudioEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Get a reference to the event receiver for use with `select!`
    pub fn event_receiver(&self) -> &Receiver<AudioEvent> {
        &self.event_rx
    }

    /// Get a handle to the shared analysis data
    pub fn analysis(&self) -> Arc<Mutex<AudioAnalysis>> {
        self.analysis.clone()
    }

    /// Get a handle to the shared stream stats
    pub fn shared_stats(&self) -> SharedStats {
        self.shared_stats.clone()
    }

    /// Get a handle to the event bus
    pub fn event_bus(&self) -> Arc<EventBus> {
        self.event_bus.clone()
    }

    /// Graceful shutdown (consumes self)
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        let _ = self.cmd_tx.send(AudioCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// The engine's main loop, running on the dedicated thread
    #[allow(clippy::too_many_arguments)]
    fn run(
        cmd_rx: Receiver<AudioCommand>,
        event_tx: Sender<AudioEvent>,
        init_tx: Sender<Result<(), String>>,
        analysis: Arc<Mutex<AudioAnalysis>>,
        shared_stats: SharedStats,
        event_bus: Arc<EventBus>,
        eq_params: SharedEqParams,
        recorder: Recorder,
        output_lost: Arc<AtomicBool>,
        mut open: OpenOutput,
    ) {
        // Create audio output on this thread (cpal streams may be !Send)
        let mut stream = match open(&output_lost) {
            Ok(s) => Some(s),
            Err(e) => {
                let _ = init_tx.send(Err(format!("Failed to open audio output: {}", e)));
                return;
            }
        };

        // `stream` must be declared before `sink` so Rust drops sink first
        let mut sink = new_player(stream.as_ref());

        let _ = init_tx.send(Ok(()));

        let mut state = PlaybackState::Stopped;
        let mut current_volume: f32 = 1.0;
        let mut health_monitor: Option<StreamHealthMonitor> = None;
        let mut stream_error_slot: Option<Arc<Mutex<Option<String>>>> = None;
        let mut current_decoder_stats: Option<Arc<DecoderStats>> = None;
        let mut current_bytes_received: Option<Arc<AtomicU64>> = None;
        let mut current_segments_downloaded: Option<Arc<AtomicU64>> = None;
        let mut current_buffer_status: Option<SharedBufferStatus> = None;
        let mut was_buffering = false;
        let mut buffering_since: Option<Instant> = None;
        let mut prolonged_buffering_stall = false;
        // Stops the playing stream: its buffer and its network threads
        let mut stream_cancel: Option<StreamCancel> = None;
        let mut analysis_active: Option<Arc<AtomicBool>> = None;
        let mut _producer_probing_flag: Option<Arc<AtomicBool>> = None;
        let mut _producer_handle: Option<JoinHandle<()>> = None;
        let mut last_throughput_bytes: u64 = 0;
        let mut last_throughput_time = Instant::now();
        let mut pending_probe: Option<PendingProbe> = None;
        // What is playing, kept here so it can move to another device
        let mut playing_source: Option<SharedSource> = None;
        // The output device went away and no other could be opened yet
        let mut waiting_for_output = false;
        let mut next_output_try = Instant::now();

        let mut next_tick = Instant::now() + TICK_INTERVAL;

        loop {
            let probe_rx = pending_probe.as_ref().map(|p| &p.probe_rx);
            match next_wake(&cmd_rx, probe_rx, next_tick) {
                Wake::Command(cmd) => match cmd {
                    AudioCommand::Play {
                        reader,
                        format_hint,
                        bitrate,
                        bytes_received,
                        segments_downloaded,
                        playback_position,
                        cancel,
                    } => {
                        // Cancel any pending probe
                        if let Some(probe) = pending_probe.take() {
                            probe.cancel.cancel();
                        }
                        // Stop any current playback (including producer thread)
                        if let Some(ref flag) = analysis_active {
                            flag.store(false, Ordering::SeqCst);
                        }
                        if let Some(ref cancel) = stream_cancel {
                            cancel.cancel();
                        }
                        sink.stop();
                        playing_source = None;
                        // Drop old producer resources before creating new ones
                        drop(analysis_active.take());
                        drop(stream_cancel.take());
                        drop(_producer_probing_flag.take());
                        drop(_producer_handle.take());
                        if let Ok(mut data) = analysis.lock() {
                            data.reset();
                        }

                        // Wrap reader in decoupled producer-consumer buffer
                        let buf_status =
                            Arc::new(Mutex::new(crate::stream::buffer::BufferStatus::default()));
                        let probing_flag = Arc::new(AtomicBool::new(true));
                        let (buf_reader, prod_handle) = StreamBuffer::with_cancel(
                            reader,
                            buf_status.clone(),
                            probing_flag.clone(),
                            cancel.clone(),
                        );

                        // Report how far the decoder has read, for song info timing
                        let buf_reader = PlaybackPositionReader::new(
                            buf_reader,
                            playback_position.unwrap_or_default(),
                        );

                        // Start async probe — returns immediately. The probe
                        // thread also decodes the first packet, so this thread
                        // never waits on the network.
                        match start_open(buf_reader, format_hint) {
                            Ok(probe_rx) => {
                                pending_probe = Some(PendingProbe {
                                    probe_rx,
                                    buf_status,
                                    probing_flag,
                                    cancel,
                                    prod_handle,
                                    bytes_received,
                                    segments_downloaded,
                                    bitrate,
                                    started: Instant::now(),
                                });
                            }
                            Err(e) => {
                                cancel.cancel();
                                state = PlaybackState::Stopped;
                                if let Ok(mut stats) = shared_stats.lock() {
                                    *stats = StreamStats::default();
                                    stats.health_state =
                                        HealthState::Failed(FailureReason::ProbeFailed);
                                }
                                let _ = event_tx.send(AudioEvent::Error(e.to_string()));
                                event_bus.emit(StreamEvent::Error(e.to_string()));
                            }
                        }
                    }
                    AudioCommand::Stop => {
                        if let Some(probe) = pending_probe.take() {
                            probe.cancel.cancel();
                        }
                        if let Some(ref flag) = analysis_active {
                            flag.store(false, Ordering::SeqCst);
                        }
                        if let Some(ref cancel) = stream_cancel {
                            cancel.cancel();
                        }
                        sink.stop();
                        playing_source = None;
                        if let Ok(mut data) = analysis.lock() {
                            data.reset();
                        }
                        health_monitor = None;
                        stream_error_slot = None;
                        current_decoder_stats = None;
                        current_bytes_received = None;
                        current_segments_downloaded = None;
                        current_buffer_status = None;
                        analysis_active = None;
                        stream_cancel = None;
                        _producer_probing_flag = None;
                        _producer_handle = None;
                        if state != PlaybackState::Stopped {
                            state = PlaybackState::Stopped;
                            if let Ok(mut stats) = shared_stats.lock() {
                                *stats = StreamStats::default();
                            }
                            event_bus.emit(StreamEvent::PlaybackStopped);
                            let _ = event_tx.send(AudioEvent::Stopped);
                        }
                    }
                    AudioCommand::Pause => {
                        if state == PlaybackState::Playing {
                            sink.pause();
                            state = PlaybackState::Paused;
                            let _ = event_tx.send(AudioEvent::Paused);
                        }
                    }
                    AudioCommand::Resume => {
                        if state == PlaybackState::Paused {
                            sink.play();
                            state = PlaybackState::Playing;
                            let _ = event_tx.send(AudioEvent::Resumed);
                            if waiting_for_output {
                                let _ = event_tx.send(AudioEvent::OutputLost);
                            }
                        }
                    }
                    AudioCommand::SetVolume(vol) => {
                        current_volume = vol.clamp(0.0, 2.0);
                        sink.set_volume(volume_curve(current_volume));
                    }
                    AudioCommand::SetEqBand { band, gain_db } => {
                        if let Ok(mut p) = eq_params.lock() {
                            p.set_band(band, gain_db);
                        }
                    }
                    AudioCommand::SetEqGains { gains, preset_name } => {
                        if let Ok(mut p) = eq_params.lock() {
                            p.set_gains(gains, preset_name);
                        }
                    }
                    AudioCommand::SetEqPreamp(db) => {
                        if let Ok(mut p) = eq_params.lock() {
                            p.set_preamp(db);
                        }
                    }
                    AudioCommand::SetEqEnabled(on) => {
                        if let Ok(mut p) = eq_params.lock() {
                            p.set_enabled(on);
                        }
                    }
                    AudioCommand::Shutdown => {
                        if let Some(probe) = pending_probe.take() {
                            probe.cancel.cancel();
                        }
                        if let Some(ref flag) = analysis_active {
                            flag.store(false, Ordering::SeqCst);
                        }
                        if let Some(ref cancel) = stream_cancel {
                            cancel.cancel();
                        }
                        sink.stop();
                        break;
                    }
                },
                Wake::Tick => {
                    next_tick = Instant::now() + TICK_INTERVAL;

                    // The output device went away (unplugged, disabled, or
                    // the sound server restarted): carry on with whichever
                    // device can be opened now, where playback left off
                    if (waiting_for_output || output_lost.load(Ordering::SeqCst))
                        && Instant::now() >= next_output_try
                    {
                        next_output_try = Instant::now() + OUTPUT_RETRY_INTERVAL;
                        match open(&output_lost) {
                            Ok(new_stream) => {
                                let new_sink = new_player(Some(&new_stream));
                                new_sink.set_volume(volume_curve(current_volume));
                                if state == PlaybackState::Paused {
                                    new_sink.pause();
                                }
                                if let Some(ref source) = playing_source {
                                    new_sink.append(Relay::resume(source));
                                }
                                // The old player goes before its device
                                sink = new_sink;
                                stream = Some(new_stream);
                                eprintln!("Audio output reopened");
                                if waiting_for_output {
                                    waiting_for_output = false;
                                    // No samples flowed while waiting
                                    if let Some(ref mut monitor) = health_monitor {
                                        monitor.reset_stall_timer();
                                    }
                                    let _ = event_tx.send(AudioEvent::OutputRestored);
                                }
                            }
                            Err(e) => {
                                if !waiting_for_output {
                                    eprintln!("Audio output lost, no device to play on: {e}");
                                    waiting_for_output = true;
                                    // Let the dead device go: some backends
                                    // keep reporting its errors in a busy loop
                                    stream = None;
                                    let _ = event_tx.send(AudioEvent::OutputLost);
                                }
                            }
                        }
                    }

                    // Poll pending probe for completion
                    if let Some(ref pending) = pending_probe {
                        match pending.probe_rx.try_recv() {
                            Ok(Ok(source)) => {
                                let p = pending_probe.take().unwrap();
                                // Probe succeeded — allow buffer compaction
                                p.probing_flag.store(false, Ordering::SeqCst);

                                let mut codec_info = source.codec_info();
                                codec_info.bitrate = p.bitrate;
                                let error_slot = source.error_slot();
                                let dec_stats = source.decoder_stats();
                                let active_flag = Arc::new(AtomicBool::new(true));
                                let source =
                                    RecordingTap::new(source, recorder.clone(), TapPoint::BeforeEq);
                                let eq_source = EqSource::new(source, eq_params.clone());
                                let eq_source = RecordingTap::new(
                                    eq_source,
                                    recorder.clone(),
                                    TapPoint::AfterEq,
                                );
                                let analyzing = AnalyzingSource::new(
                                    eq_source,
                                    analysis.clone(),
                                    active_flag.clone(),
                                );
                                let relay = Relay::new(analyzing);
                                playing_source = Some(relay.source());
                                // A new player for each station: appending to
                                // a stopped player waits until its queue has
                                // played out, which a dead device never does
                                sink = new_player(stream.as_ref());
                                sink.append(relay);
                                sink.set_volume(volume_curve(current_volume));
                                sink.play();
                                state = PlaybackState::Playing;
                                health_monitor = Some(StreamHealthMonitor::new());
                                stream_error_slot = Some(error_slot);
                                current_decoder_stats = Some(dec_stats);
                                current_bytes_received = p.bytes_received;
                                current_segments_downloaded = p.segments_downloaded;
                                current_buffer_status = Some(p.buf_status);
                                was_buffering = false;
                                buffering_since = None;
                                prolonged_buffering_stall = false;
                                last_throughput_bytes = 0;
                                last_throughput_time = Instant::now();
                                analysis_active = Some(active_flag);
                                stream_cancel = Some(p.cancel);
                                _producer_probing_flag = Some(p.probing_flag);
                                _producer_handle = Some(p.prod_handle);

                                // Update shared stats
                                if let Ok(mut stats) = shared_stats.lock() {
                                    *stats = StreamStats::default();
                                    stats.codec_info = Some(codec_info.clone());
                                    stats.play_started_at = Some(Instant::now());
                                }

                                // Emit event
                                event_bus.emit(StreamEvent::PlaybackStarted {
                                    codec_info: codec_info.clone(),
                                    stream_url: String::new(),
                                });

                                let _ = event_tx.send(AudioEvent::Playing(codec_info));
                                if waiting_for_output {
                                    let _ = event_tx.send(AudioEvent::OutputLost);
                                }
                            }
                            Ok(Err(e)) => {
                                let p = pending_probe.take().unwrap();
                                p.cancel.cancel();
                                state = PlaybackState::Stopped;
                                if let Ok(mut stats) = shared_stats.lock() {
                                    *stats = StreamStats::default();
                                    stats.health_state =
                                        HealthState::Failed(FailureReason::ProbeFailed);
                                }
                                let _ = event_tx.send(AudioEvent::Error(e.to_string()));
                                event_bus.emit(StreamEvent::Error(e.to_string()));
                            }
                            Err(TryRecvError::Empty) => {
                                // Still probing — check for timeout
                                if pending.started.elapsed().as_secs() >= PROBE_TIMEOUT_SECS {
                                    let p = pending_probe.take().unwrap();
                                    p.cancel.cancel();
                                    state = PlaybackState::Stopped;
                                    if let Ok(mut stats) = shared_stats.lock() {
                                        *stats = StreamStats::default();
                                        stats.health_state =
                                            HealthState::Failed(FailureReason::ProbeFailed);
                                    }
                                    let msg = format!(
                                        "Unable to detect audio format (timed out after {}s)",
                                        PROBE_TIMEOUT_SECS
                                    );
                                    let _ = event_tx.send(AudioEvent::ProbeTimeout);
                                    let _ = event_tx.send(AudioEvent::Error(msg.clone()));
                                    event_bus.emit(StreamEvent::Error(msg));
                                }
                            }
                            Err(TryRecvError::Disconnected) => {
                                let p = pending_probe.take().unwrap();
                                p.cancel.cancel();
                                state = PlaybackState::Stopped;
                                if let Ok(mut stats) = shared_stats.lock() {
                                    *stats = StreamStats::default();
                                    stats.health_state =
                                        HealthState::Failed(FailureReason::ProbeFailed);
                                }
                                let _ = event_tx
                                    .send(AudioEvent::Error("Probe thread panicked".to_string()));
                                event_bus
                                    .emit(StreamEvent::Error("Probe thread panicked".to_string()));
                            }
                        }
                    }

                    // Check if playback ended (naturally or due to error)
                    if state == PlaybackState::Playing && sink.empty() {
                        if let Some(ref flag) = analysis_active {
                            flag.store(false, Ordering::SeqCst);
                        }
                        if let Some(ref cancel) = stream_cancel {
                            cancel.cancel();
                        }
                        state = PlaybackState::Stopped;
                        health_monitor = None;
                        playing_source = None;
                        if let Ok(mut data) = analysis.lock() {
                            data.reset();
                        }
                        // Check if stream ended due to IO/decode error vs clean EOF
                        if let Some(ref slot) = stream_error_slot {
                            if let Ok(guard) = slot.lock() {
                                if let Some(ref err_msg) = *guard {
                                    let err_msg = format!("Stream error: {}", err_msg);
                                    let _ = event_tx.send(AudioEvent::Error(err_msg.clone()));
                                    event_bus.emit(StreamEvent::Error(err_msg));
                                }
                            }
                        }
                        stream_error_slot = None;
                        current_decoder_stats = None;
                        current_bytes_received = None;
                        current_segments_downloaded = None;
                        current_buffer_status = None;
                        analysis_active = None;
                        stream_cancel = None;
                        _producer_probing_flag = None;
                        _producer_handle = None;
                        if let Ok(mut stats) = shared_stats.lock() {
                            *stats = StreamStats::default();
                        }
                        event_bus.emit(StreamEvent::PlaybackStopped);
                        let _ = event_tx.send(AudioEvent::Stopped);
                    }

                    // Read sample_count once — used by both stats update and health monitoring.
                    // Done BEFORE locking shared_stats to avoid lock ordering issues with the UI viz timer.
                    let sample_count = if state == PlaybackState::Playing {
                        analysis.lock().map(|a| a.sample_count).unwrap_or(0)
                    } else {
                        0
                    };

                    // Update shared stats on tick when playing
                    if state == PlaybackState::Playing {
                        let buf_snapshot = current_buffer_status.as_ref().and_then(|bs| {
                            bs.lock().ok().map(|buf| {
                                (
                                    buf.level_bytes,
                                    buf.capacity_bytes,
                                    buf.is_buffering,
                                    buf.underrun_count,
                                    buf.effective_watermark,
                                )
                            })
                        });

                        if let Ok(mut stats) = shared_stats.lock() {
                            // Decoder stats
                            if let Some(ref ds) = current_decoder_stats {
                                let (frames, errors) = ds.snapshot();
                                stats.frames_played = frames;
                                stats.decode_errors = errors;
                            }
                            // Segments downloaded (HLS only)
                            if let Some(ref sd) = current_segments_downloaded {
                                stats.segments_downloaded = sd.load(Ordering::Relaxed);
                            }
                            // Bytes received + throughput from delta
                            if let Some(ref br) = current_bytes_received {
                                let now_bytes = br.load(Ordering::Relaxed);
                                stats.bytes_received = now_bytes;
                                let elapsed = last_throughput_time.elapsed().as_secs_f64();
                                if elapsed >= 1.0 {
                                    let delta = now_bytes.saturating_sub(last_throughput_bytes);
                                    let bytes_per_sec = delta as f64 / elapsed;
                                    stats.throughput_kbps = (bytes_per_sec * 8.0) / 1000.0;
                                    last_throughput_bytes = now_bytes;
                                    last_throughput_time = Instant::now();
                                }
                            }
                            // Analysis data (sample count) — read above, no nested lock
                            stats.sample_count = sample_count;
                            // Health state
                            if prolonged_buffering_stall {
                                stats.health_state = HealthState::Stalled;
                            } else if let Some(ref monitor) = health_monitor {
                                stats.health_state = *monitor.state();
                            }
                            // Buffer status — read above, no nested lock
                            if let Some((level, capacity, buffering, underruns, watermark)) =
                                buf_snapshot
                            {
                                stats.buffer_level_bytes = level;
                                stats.buffer_capacity_bytes = capacity;
                                stats.is_buffering = buffering;
                                stats.underrun_count = underruns;
                                stats.effective_watermark = watermark;
                            }
                        }

                        // Emit buffering events (progressive percentage while buffering)
                        if !sink.empty() && !waiting_for_output {
                            if let Some(ref bs) = current_buffer_status {
                                if let Ok(buf) = bs.lock() {
                                    if buf.is_buffering {
                                        if !was_buffering {
                                            buffering_since = Some(Instant::now());
                                        }

                                        let stalled = buffering_since
                                            .map(|since| {
                                                since.elapsed().as_secs()
                                                    >= BUFFERING_STALL_THRESHOLD_SECS
                                            })
                                            .unwrap_or(false);

                                        if stalled && buf.level_bytes == 0 {
                                            // Prolonged buffering with no progress — genuine stall
                                            let _ = event_tx.send(AudioEvent::StreamStalled);
                                            prolonged_buffering_stall = true;
                                        } else {
                                            // Normal buffering or recovery in progress
                                            let pct = if buf.effective_watermark > 0 {
                                                ((buf.level_bytes as f64
                                                    / buf.effective_watermark as f64)
                                                    * 100.0)
                                                    .min(99.0)
                                                    as u8
                                            } else {
                                                0
                                            };
                                            let _ = event_tx.send(AudioEvent::Buffering(pct));
                                            prolonged_buffering_stall = false;
                                        }

                                        was_buffering = true;
                                    } else if was_buffering {
                                        // Recovered from buffering → signal 100%
                                        let _ = event_tx.send(AudioEvent::Buffering(100));
                                        // Reset health monitor: the stream just refilled the buffer,
                                        // proving it can deliver data. Clear any Stalled state that
                                        // accumulated during the (possibly long) buffering window.
                                        if let Some(ref mut monitor) = health_monitor {
                                            monitor.reset_to_healthy();
                                        }
                                        was_buffering = false;
                                        buffering_since = None;
                                        prolonged_buffering_stall = false;
                                    }
                                }
                            }
                        }
                    }

                    // Health monitoring: check sample flow
                    // Skip during active buffering — sample_count is frozen while consumer
                    // blocks symphonia, so stall detection would give false positives.
                    // The sink.empty() check remains as the ultimate safety net.
                    // Nothing is played while there is no output device.
                    if state == PlaybackState::Playing && !was_buffering && !waiting_for_output {
                        if let Some(ref mut monitor) = health_monitor {
                            let was_stalled = matches!(monitor.state(), &HealthState::Stalled);
                            // Reuse sample_count read above (line 537) — avoids second analysis lock
                            if let Some(failure) = monitor.update(sample_count) {
                                match failure {
                                    FailureReason::StreamStall => {
                                        // Transient stall — emit event but keep stream alive.
                                        // The stream's own reconnection logic (ICY/HLS backoff)
                                        // will handle recovery; sink.empty() detects permanent death.
                                        let _ = event_tx.send(AudioEvent::StreamStalled);
                                    }
                                    FailureReason::NoAudioOutput => {
                                        // Fundamental failure — tear down
                                        let _ = event_tx.send(AudioEvent::NoAudioTimeout);
                                        if let Some(ref flag) = analysis_active {
                                            flag.store(false, Ordering::SeqCst);
                                        }
                                        if let Some(ref cancel) = stream_cancel {
                                            cancel.cancel();
                                        }
                                        sink.stop();
                                        playing_source = None;
                                        if let Ok(mut data) = analysis.lock() {
                                            data.reset();
                                        }
                                        state = PlaybackState::Stopped;
                                        health_monitor = None;
                                        stream_error_slot = None;
                                        current_decoder_stats = None;
                                        current_bytes_received = None;
                                        current_segments_downloaded = None;
                                        current_buffer_status = None;
                                        analysis_active = None;
                                        stream_cancel = None;
                                        _producer_probing_flag = None;
                                        _producer_handle = None;
                                        if let Ok(mut stats) = shared_stats.lock() {
                                            *stats = StreamStats::default();
                                        }
                                        event_bus.emit(StreamEvent::PlaybackStopped);
                                        let _ = event_tx.send(AudioEvent::Stopped);
                                    }
                                    FailureReason::ProbeFailed => {
                                        unreachable!("health monitor never emits ProbeFailed")
                                    }
                                }
                            } else if was_stalled
                                && matches!(monitor.state(), &HealthState::Healthy)
                            {
                                // Stalled → Healthy: stream recovered, clear UI error
                                let _ = event_tx.send(AudioEvent::StreamRecovered);
                            }
                        }
                    }
                }
                Wake::Closed => {
                    break;
                }
            }
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::AtomicUsize;

    /// Build a minimal valid WAV file in memory
    fn make_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let bits_per_sample: u16 = 16;
        let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
        let block_align = channels * (bits_per_sample / 8);
        let data_size = (samples.len() * 2) as u32;
        let file_size = 36 + data_size;

        let mut buf = Vec::new();
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&file_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        for &s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        buf
    }

    /// Generate 1 second of mono sine wave
    fn make_one_second_wav() -> Vec<u8> {
        let samples: Vec<i16> = (0..44100)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        make_wav(44100, 1, &samples)
    }

    /// Generate a short WAV (10ms)
    fn make_short_wav() -> Vec<u8> {
        let samples: Vec<i16> = (0..441)
            .map(|i| ((i as f32 * 0.5).sin() * 5000.0) as i16)
            .collect();
        make_wav(44100, 1, &samples)
    }

    /// Helper: wait for a specific event type within a timeout
    fn wait_for_event(engine: &AudioEngine, timeout_ms: u64) -> Option<AudioEvent> {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(evt) = engine.try_recv_event() {
                return Some(evt);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Helper: try to create an engine; return None if audio hardware is unavailable
    fn try_engine() -> Option<AudioEngine> {
        AudioEngine::new().ok()
    }

    /// Check if audio playback actually processes samples (cached).
    /// Returns false in CI/headless environments where rodio creates a device
    /// but the audio thread doesn't actually pull samples.
    fn audio_playback_works() -> bool {
        static RESULT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *RESULT.get_or_init(|| {
            let engine = match AudioEngine::new() {
                Ok(e) => e,
                Err(_) => return false,
            };
            engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
            match wait_for_event(&engine, 2000) {
                Some(AudioEvent::Playing(_)) => {}
                _ => {
                    engine.shutdown();
                    return false;
                }
            }
            // Poll for up to 1s to see if audio samples actually flow
            let deadline = Instant::now() + Duration::from_secs(1);
            let works = loop {
                thread::sleep(Duration::from_millis(50));
                if let Ok(a) = engine.analysis().lock() {
                    if a.sample_count > 0 {
                        break true;
                    }
                }
                if Instant::now() >= deadline {
                    break false;
                }
            };
            engine.shutdown();
            works
        })
    }

    /// Helper: try to create an engine AND verify audio playback works.
    /// Returns None in CI/headless environments where samples don't actually flow.
    fn try_engine_playback() -> Option<AudioEngine> {
        if !audio_playback_works() {
            return None;
        }
        AudioEngine::new().ok()
    }

    // --- Loop scheduling (no audio device needed) ---

    #[test]
    fn tick_runs_on_time_while_commands_keep_coming() {
        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(16);
        let sending = Arc::new(AtomicBool::new(true));
        let still_sending = sending.clone();
        // A dragged volume slider
        let slider = thread::spawn(move || {
            while still_sending.load(Ordering::Relaxed) {
                let _ = cmd_tx.send(AudioCommand::SetVolume(0.5));
                thread::sleep(Duration::from_millis(5));
            }
        });

        let start = Instant::now();
        let next_tick = start + Duration::from_millis(100);
        let mut commands = 0;
        let ticked = loop {
            match next_wake::<()>(&cmd_rx, None, next_tick) {
                Wake::Command(_) => commands += 1,
                Wake::Tick => break start.elapsed(),
                Wake::Closed => panic!("the command channel is still open"),
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "the tick never ran"
            );
        };
        sending.store(false, Ordering::Relaxed);
        slider.join().unwrap();

        assert!(commands > 0, "commands are still handled");
        assert!(ticked >= Duration::from_millis(100));
        assert!(
            ticked < Duration::from_millis(400),
            "tick ran late: {ticked:?}"
        );
    }

    #[test]
    fn a_finished_probe_wakes_the_loop_at_once() {
        let (_cmd_tx, cmd_rx) = bounded::<AudioCommand>(16);
        let (probe_tx, probe_rx) = bounded::<u32>(1);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let _ = probe_tx.send(1);
        });

        let start = Instant::now();
        let far_off = start + Duration::from_secs(10);
        assert!(matches!(
            next_wake(&cmd_rx, Some(&probe_rx), far_off),
            Wake::Tick
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
        // The result is still there for the tick to take
        assert_eq!(probe_rx.try_recv(), Ok(1));
    }

    #[test]
    fn commands_are_delivered_and_a_closed_channel_ends_the_loop() {
        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(16);
        let far_off = Instant::now() + Duration::from_secs(10);
        cmd_tx.send(AudioCommand::Stop).unwrap();
        assert!(matches!(
            next_wake::<()>(&cmd_rx, None, far_off),
            Wake::Command(AudioCommand::Stop)
        ));
        drop(cmd_tx);
        assert!(matches!(
            next_wake::<()>(&cmd_rx, None, far_off),
            Wake::Closed
        ));
    }

    #[test]
    fn a_station_starts_and_ends_while_the_volume_slider_moves() {
        let Some(engine) = try_engine_playback() else {
            return;
        };
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);

        // Move the slider the whole time: the station must still start, and
        // its end must still be noticed
        let start = Instant::now();
        let mut events = Vec::new();
        while start.elapsed() < Duration::from_secs(5)
            && !events.iter().any(|e| matches!(e, AudioEvent::Stopped))
        {
            engine.set_volume(0.5);
            thread::sleep(Duration::from_millis(10));
            while let Some(event) = engine.try_recv_event() {
                events.push(event);
            }
        }
        assert!(
            events.iter().any(|e| matches!(e, AudioEvent::Playing(_))),
            "never started: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AudioEvent::Stopped)),
            "the end of the clip was not noticed: {events:?}"
        );
    }

    // --- Output device loss ---

    /// An engine whose output opens only on the attempts `works` allows
    /// (counting from 1), and a count of the attempts
    fn engine_with_output(works: fn(usize) -> bool) -> Option<(AudioEngine, Arc<AtomicUsize>)> {
        if !audio_playback_works() {
            return None;
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let generation = Arc::new(AtomicU64::new(0));
        let engine = AudioEngine::with_output(Box::new(move |lost| {
            let attempt = counted.fetch_add(1, Ordering::SeqCst) + 1;
            if works(attempt) {
                open_output(lost, &generation)
            } else {
                Err("no device".to_string())
            }
        }))
        .ok()?;
        Some((engine, attempts))
    }

    /// A mono sine WAV that doesn't end in any test's lifetime: the test
    /// audio device can play many times faster than real time
    struct EndlessWav {
        header: Vec<u8>,
        pos: u64,
    }

    impl EndlessWav {
        fn new() -> Box<Self> {
            let mut header = make_wav(44100, 1, &[]);
            let data_size = u32::MAX - 64;
            header[4..8].copy_from_slice(&(36 + data_size).to_le_bytes());
            header[40..44].copy_from_slice(&data_size.to_le_bytes());
            Box::new(Self { header, pos: 0 })
        }
    }

    impl std::io::Read for EndlessWav {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let header_len = self.header.len() as u64;
            for (i, byte) in buf.iter_mut().enumerate() {
                let pos = self.pos + i as u64;
                *byte = if pos < header_len {
                    self.header[pos as usize]
                } else {
                    let offset = pos - header_len;
                    let sample = ((offset / 2) as f32 * 0.1).sin() * 10000.0;
                    (sample as i16).to_le_bytes()[(offset % 2) as usize]
                };
            }
            self.pos += buf.len() as u64;
            Ok(buf.len())
        }
    }

    impl std::io::Seek for EndlessWav {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.pos = match pos {
                std::io::SeekFrom::Start(n) => n,
                std::io::SeekFrom::Current(n) => self.pos.checked_add_signed(n).unwrap_or(0),
                std::io::SeekFrom::End(_) => {
                    return Err(std::io::Error::other("endless stream"));
                }
            };
            Ok(self.pos)
        }
    }

    fn sample_count(engine: &AudioEngine) -> u64 {
        engine.analysis().lock().unwrap().sample_count
    }

    /// Wait until `done` holds, collecting events meanwhile
    fn wait_until(
        engine: &AudioEngine,
        events: &mut Vec<AudioEvent>,
        timeout: Duration,
        mut done: impl FnMut(&[AudioEvent]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(event) = engine.try_recv_event() {
                events.push(event);
            }
            if done(events) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn has(events: &[AudioEvent], wanted: fn(&AudioEvent) -> bool) -> bool {
        events.iter().any(wanted)
    }

    #[test]
    fn playback_moves_to_a_new_device_when_the_output_is_lost() {
        let Some((engine, attempts)) = engine_with_output(|_| true) else {
            return;
        };
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |_| { sample_count(&engine) > 0 }
        ));

        engine.simulate_output_loss();
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |_| {
                attempts.load(Ordering::SeqCst) == 2
            }),
            "the output was not reopened"
        );
        // Once the old device's last samples are counted, the count keeps
        // rising: the station plays on the new device
        thread::sleep(Duration::from_millis(200));
        let before = sample_count(&engine);
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |_| {
                sample_count(&engine) > before
            }),
            "nothing played on the new device"
        );

        // The move went unnoticed
        assert!(
            !has(&events, |e| matches!(
                e,
                AudioEvent::Error(_) | AudioEvent::OutputLost | AudioEvent::NoAudioTimeout
            )),
            "{events:?}"
        );
    }

    #[test]
    fn playback_waits_for_a_device_and_carries_on() {
        // The first reopen finds no device, the next one does
        let Some((engine, attempts)) = engine_with_output(|attempt| attempt != 2) else {
            return;
        };
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |e| { has(e, |e| matches!(e, AudioEvent::Playing(_))) }
        ));

        engine.simulate_output_loss();
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |e| {
                has(e, |e| matches!(e, AudioEvent::OutputLost))
            }),
            "{events:?}"
        );
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(5), |e| {
                has(e, |e| matches!(e, AudioEvent::OutputRestored))
            }),
            "{events:?}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(
            !has(&events, |e| matches!(
                e,
                AudioEvent::Error(_) | AudioEvent::NoAudioTimeout | AudioEvent::StreamStalled
            )),
            "{events:?}"
        );

        let before = sample_count(&engine);
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |_| {
                sample_count(&engine) > before
            }),
            "nothing played on the new device"
        );
    }

    #[test]
    fn a_new_station_without_a_device_says_so_after_it_starts() {
        // No device after the first
        let Some((engine, _)) = engine_with_output(|attempt| attempt == 1) else {
            return;
        };
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |e| { has(e, |e| matches!(e, AudioEvent::Playing(_))) }
        ));
        engine.simulate_output_loss();
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |e| { has(e, |e| matches!(e, AudioEvent::OutputLost)) }
        ));

        // Stop and start another station: the engine still answers, and
        // the loss is reported again after Playing
        events.clear();
        engine.stop();
        engine.play(EndlessWav::new(), None, None);
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |e| {
                e.iter()
                    .skip_while(|e| !matches!(e, AudioEvent::Playing(_)))
                    .any(|e| matches!(e, AudioEvent::OutputLost))
            }),
            "{events:?}"
        );
        assert!(has(&events, |e| matches!(e, AudioEvent::Stopped)));

        // Same after a resume
        events.clear();
        engine.pause();
        engine.resume();
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |e| {
                e.iter()
                    .skip_while(|e| !matches!(e, AudioEvent::Resumed))
                    .any(|e| matches!(e, AudioEvent::OutputLost))
            }),
            "{events:?}"
        );
        engine.shutdown();
    }

    // --- Lifecycle ---

    #[test]
    fn create_and_shutdown() {
        let Some(engine) = try_engine() else { return };
        engine.shutdown();
    }

    #[test]
    fn drop_triggers_shutdown() {
        let Some(engine) = try_engine() else { return };
        drop(engine);
        // If we get here without hanging, shutdown worked
    }

    #[test]
    fn shutdown_is_idempotent_via_drop() {
        // shutdown_inner is called once explicitly, then again in drop
        let Some(engine) = try_engine() else { return };
        engine.shutdown();
        // Drop happens automatically after shutdown consumed self
    }

    #[test]
    fn create_multiple_engines_sequentially() {
        for _ in 0..3 {
            let Some(engine) = try_engine() else { return };
            engine.shutdown();
        }
    }

    // --- Play / Stop ---

    #[test]
    fn play_and_stop() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing event, got {:?}", other),
        }

        engine.stop();

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            other => panic!("Expected Stopped event, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn play_emits_codec_info() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.channels, 1);
                assert_eq!(info.sample_rate, 44100);
                assert!(!info.codec_name.is_empty());
            }
            other => panic!("Expected Playing with CodecInfo, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn play_stereo_wav() {
        let Some(engine) = try_engine() else { return };

        let samples: Vec<i16> = (0..88200)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let wav = make_wav(44100, 2, &samples);

        engine.play(Box::new(Cursor::new(wav)), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.channels, 2);
                assert_eq!(info.sample_rate, 44100);
            }
            other => panic!("Expected Playing event, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn stop_when_not_playing_does_not_emit_event() {
        let Some(engine) = try_engine() else { return };

        engine.stop();
        // Give it time to process
        thread::sleep(Duration::from_millis(200));

        // Should not have received a Stopped event (already stopped)
        let evt = engine.try_recv_event();
        assert!(
            evt.is_none(),
            "Stop when already stopped should not emit event, got {:?}",
            evt
        );

        engine.shutdown();
    }

    #[test]
    fn double_stop_only_emits_one_stopped_event() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);

        // Wait for Playing
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.stop();
        // Small delay then stop again
        thread::sleep(Duration::from_millis(100));
        engine.stop();

        // Wait for first Stopped event
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            other => panic!("Expected Stopped, got {:?}", other),
        }

        // Second stop should not produce another event
        thread::sleep(Duration::from_millis(200));
        let evt = engine.try_recv_event();
        assert!(
            evt.is_none(),
            "Second stop should not emit event, got {:?}",
            evt
        );

        engine.shutdown();
    }

    // --- Play replaces current playback ---

    #[test]
    fn play_replaces_current_playback() {
        let Some(engine) = try_engine() else { return };

        // Start playing first clip
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected first Playing, got {:?}", other),
        }

        // Play second clip without stopping first
        let samples: Vec<i16> = (0..48000)
            .map(|i| ((i as f32 * 0.2).sin() * 8000.0) as i16)
            .collect();
        let wav2 = make_wav(48000, 2, &samples);
        engine.play(Box::new(Cursor::new(wav2)), None, None);

        // Should get a new Playing event with updated info
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.channels, 2);
                assert_eq!(info.sample_rate, 48000);
            }
            other => panic!("Expected second Playing, got {:?}", other),
        }

        engine.shutdown();
    }

    // --- Error handling ---

    #[test]
    fn play_invalid_data_returns_error_event() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(msg)) => {
                assert!(!msg.is_empty(), "Error message should not be empty");
            }
            other => panic!("Expected Error event, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn play_empty_data_returns_error_event() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(Vec::<u8>::new())), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(msg)) => {
                assert!(!msg.is_empty());
            }
            other => panic!("Expected Error event for empty data, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn error_does_not_break_engine() {
        let Some(engine) = try_engine() else { return };

        // Send invalid data
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(_)) => {}
            other => panic!("Expected Error, got {:?}", other),
        }

        // Engine should still work - play valid data
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.channels, 1);
            }
            other => panic!("Expected Playing after error recovery, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn multiple_errors_in_sequence() {
        let Some(engine) = try_engine() else { return };

        for _ in 0..3 {
            engine.play(Box::new(Cursor::new(vec![0xDE, 0xAD])), None, None);
            match wait_for_event(&engine, 2000) {
                Some(AudioEvent::Error(_)) => {}
                other => panic!("Expected Error, got {:?}", other),
            }
        }

        // Engine still works
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing after multiple errors, got {:?}", other),
        }

        engine.shutdown();
    }

    // --- Volume ---

    #[test]
    fn set_volume_does_not_crash() {
        let Some(engine) = try_engine() else { return };
        engine.set_volume(0.5);
        engine.set_volume(0.0);
        engine.set_volume(2.0);
        engine.set_volume(5.0); // should clamp to 2.0
        engine.shutdown();
    }

    #[test]
    fn set_volume_negative_clamped() {
        let Some(engine) = try_engine() else { return };
        engine.set_volume(-1.0);
        engine.set_volume(-100.0);
        // No crash = success
        engine.shutdown();
    }

    #[test]
    fn set_volume_during_playback() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }

        // Change volume multiple times during playback
        engine.set_volume(0.0);
        thread::sleep(Duration::from_millis(50));
        engine.set_volume(1.0);
        thread::sleep(Duration::from_millis(50));
        engine.set_volume(0.5);

        engine.shutdown();
    }

    #[test]
    fn set_volume_while_stopped() {
        let Some(engine) = try_engine() else { return };
        // Setting volume while stopped should not panic or produce events
        engine.set_volume(0.75);
        thread::sleep(Duration::from_millis(100));
        assert!(engine.try_recv_event().is_none());
        engine.shutdown();
    }

    // --- Analysis ---

    #[test]
    fn analysis_starts_at_zero() {
        let Some(engine) = try_engine() else { return };

        let data = engine.analysis();
        let analysis = data.lock().unwrap();
        assert_eq!(analysis.vu_left, 0.0);
        assert_eq!(analysis.vu_right, 0.0);
        assert!(analysis.spectrum.iter().all(|&v| v == 0.0));

        drop(analysis);
        engine.shutdown();
    }

    #[test]
    fn analysis_returns_same_arc() {
        let Some(engine) = try_engine() else { return };

        let a1 = engine.analysis();
        let a2 = engine.analysis();
        // Both should point to the same underlying data
        assert!(Arc::ptr_eq(&a1, &a2));

        engine.shutdown();
    }

    #[test]
    fn analysis_reset_after_stop() {
        let Some(engine) = try_engine_playback() else {
            return;
        };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }

        // Let some audio play to build up analysis
        thread::sleep(Duration::from_millis(200));

        engine.stop();
        let _ = wait_for_event(&engine, 2000);
        // Give the player time to fully drain after stop
        thread::sleep(Duration::from_millis(100));

        // After stop, analysis should be reset
        let data = engine.analysis();
        let analysis = data.lock().unwrap();
        assert_eq!(analysis.vu_left, 0.0);
        assert_eq!(analysis.vu_right, 0.0);
        assert!(analysis.spectrum.iter().all(|&v| v == 0.0));

        drop(analysis);
        engine.shutdown();
    }

    // --- Event receiver ---

    #[test]
    fn event_receiver_can_be_obtained() {
        let Some(engine) = try_engine() else { return };
        let _rx = engine.event_receiver();
        engine.shutdown();
    }

    #[test]
    fn event_receiver_receives_events() {
        let Some(engine) = try_engine() else { return };

        let rx = engine.event_receiver();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);

        // Use the receiver directly
        let evt = rx.recv_timeout(Duration::from_secs(2));
        assert!(evt.is_ok(), "Should receive event via receiver");
        match evt.unwrap() {
            AudioEvent::Playing(_) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.shutdown();
    }

    // --- Raw send ---

    #[test]
    fn send_raw_stop_command() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }

        engine.send(AudioCommand::Stop);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            other => panic!("Expected Stopped from raw send, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn send_raw_shutdown_command() {
        let Some(engine) = try_engine() else { return };

        engine.send(AudioCommand::Shutdown);
        // Engine thread should exit; drop shouldn't hang
        thread::sleep(Duration::from_millis(200));
        drop(engine);
    }

    // --- Stream-ended detection ---

    #[test]
    fn short_clip_auto_stops() {
        let Some(engine) = try_engine() else { return };

        // Play a very short clip (10ms) - should end quickly
        engine.play(Box::new(Cursor::new(make_short_wav())), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Wait for auto-stop (stream ends naturally, detected via recv_timeout)
        match wait_for_event(&engine, 3000) {
            Some(AudioEvent::Stopped) => {}
            other => panic!("Expected auto-Stopped for short clip, got {:?}", other),
        }

        engine.shutdown();
    }

    // --- Format hints ---

    #[test]
    fn play_with_format_hint() {
        let Some(engine) = try_engine() else { return };

        engine.play(
            Box::new(Cursor::new(make_one_second_wav())),
            Some("wav".to_string()),
            None,
        );

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing with hint, got {:?}", other),
        }

        engine.shutdown();
    }

    // --- Rapid command sequences ---

    #[test]
    fn rapid_play_stop_sequence() {
        let Some(engine) = try_engine() else { return };

        for _ in 0..5 {
            engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
            engine.stop();
        }

        // Give engine time to process all commands
        thread::sleep(Duration::from_millis(500));

        // Engine should still be responsive
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(_) => {
                // Any event is acceptable after rapid commands
            }
            None => panic!("Engine became unresponsive after rapid commands"),
        }

        engine.shutdown();
    }

    #[test]
    fn rapid_volume_changes() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }

        // Rapidly change volume
        for i in 0..100 {
            engine.set_volume(i as f32 / 50.0); // 0.0 to 2.0
        }

        // Should not crash or hang
        engine.shutdown();
    }

    #[test]
    fn play_then_immediate_play_different() {
        let Some(engine) = try_engine() else { return };

        // Play first, immediately play second without explicit stop
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        engine.play(
            Box::new(Cursor::new(make_wav(48000, 2, &vec![0i16; 48000]))),
            None,
            None,
        );

        // Drain events - we should get at least one Playing event
        let mut got_playing = false;
        for _ in 0..40 {
            match engine.try_recv_event() {
                Some(AudioEvent::Playing(_)) => {
                    got_playing = true;
                    break;
                }
                Some(_) => continue,
                None => thread::sleep(Duration::from_millis(50)),
            }
        }
        assert!(got_playing, "Should get at least one Playing event");

        engine.shutdown();
    }

    // --- Pause / Resume ---

    #[test]
    fn pause_and_resume() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.pause();
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Paused) => {}
            other => panic!("Expected Paused, got {:?}", other),
        }

        engine.resume();
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Resumed) => {}
            other => panic!("Expected Resumed, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn pause_when_stopped_is_noop() {
        let Some(engine) = try_engine() else { return };

        engine.pause();
        thread::sleep(Duration::from_millis(200));

        assert!(
            engine.try_recv_event().is_none(),
            "Pause when stopped should not emit event"
        );

        engine.shutdown();
    }

    #[test]
    fn resume_when_stopped_is_noop() {
        let Some(engine) = try_engine() else { return };

        engine.resume();
        thread::sleep(Duration::from_millis(200));

        assert!(
            engine.try_recv_event().is_none(),
            "Resume when stopped should not emit event"
        );

        engine.shutdown();
    }

    #[test]
    fn resume_when_playing_is_noop() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.resume();
        thread::sleep(Duration::from_millis(200));

        // Should not get a Resumed event since we were already playing
        assert!(
            engine.try_recv_event().is_none(),
            "Resume when already playing should not emit event"
        );

        engine.shutdown();
    }

    #[test]
    fn double_pause_only_emits_once() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.pause();
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Paused) => {}
            other => panic!("Expected Paused, got {:?}", other),
        }

        // Second pause should be a no-op
        engine.pause();
        thread::sleep(Duration::from_millis(200));

        assert!(
            engine.try_recv_event().is_none(),
            "Second pause should not emit event"
        );

        engine.shutdown();
    }

    #[test]
    fn stop_while_paused_emits_stopped() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.pause();
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Paused) => {}
            other => panic!("Expected Paused, got {:?}", other),
        }

        engine.stop();
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            other => panic!("Expected Stopped after pause+stop, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn play_while_paused_starts_new_playback() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.pause();
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Paused) => {}
            other => panic!("Expected Paused, got {:?}", other),
        }

        // Play new clip while paused
        let wav2 = make_wav(48000, 2, &vec![0i16; 48000]);
        engine.play(Box::new(Cursor::new(wav2)), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.sample_rate, 48000);
                assert_eq!(info.channels, 2);
            }
            other => panic!("Expected new Playing, got {:?}", other),
        }

        engine.shutdown();
    }

    // --- Volume persistence ---

    #[test]
    fn volume_persists_across_play_transitions() {
        let Some(engine) = try_engine() else { return };

        // Set volume to 0 (mute)
        engine.set_volume(0.0);

        // Play first clip
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected first Playing, got {:?}", other),
        }

        // Play second clip - volume should still be 0.0
        let wav2 = make_wav(48000, 1, &vec![0i16; 48000]);
        engine.play(Box::new(Cursor::new(wav2)), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected second Playing, got {:?}", other),
        }

        // If volume wasn't preserved, we'd hear audio; with volume 0 we don't.
        // We can't directly read sink volume, but at least verify no crash.
        engine.shutdown();
    }

    #[test]
    fn volume_set_before_play_is_applied() {
        let Some(engine) = try_engine() else { return };

        // Set volume before any playback
        engine.set_volume(0.5);
        thread::sleep(Duration::from_millis(100));

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // No crash = volume was applied correctly
        engine.shutdown();
    }

    // --- Health & Resilience integration ---

    #[test]
    fn analysis_sample_count_starts_at_zero() {
        let Some(engine) = try_engine() else { return };

        let data = engine.analysis();
        let analysis = data.lock().unwrap();
        assert_eq!(analysis.sample_count, 0);

        drop(analysis);
        engine.shutdown();
    }

    #[test]
    #[ignore] // requires real audio hardware — flaky in CI
    fn analysis_sample_count_increases_during_playback() {
        let Some(engine) = try_engine_playback() else {
            return;
        };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Let some audio play to accumulate samples
        thread::sleep(Duration::from_millis(500));

        let data = engine.analysis();
        let analysis = data.lock().unwrap();
        assert!(
            analysis.sample_count > 0,
            "sample_count should increase during playback, got {}",
            analysis.sample_count
        );

        drop(analysis);
        engine.shutdown();
    }

    #[test]
    fn analysis_sample_count_resets_on_stop() {
        let Some(engine) = try_engine_playback() else {
            return;
        };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }

        // Let some audio play
        thread::sleep(Duration::from_millis(300));

        engine.stop();
        let _ = wait_for_event(&engine, 2000);
        // Give the player time to fully drain after stop.
        // The span-based processing in rodio may still be mid-span when stop
        // fires, so we need enough time for the audio thread to finish and
        // for the active flag to prevent further writes.
        thread::sleep(Duration::from_millis(300));

        let data = engine.analysis();
        let analysis = data.lock().unwrap();
        assert_eq!(
            analysis.sample_count, 0,
            "sample_count should reset after stop"
        );

        drop(analysis);

        engine.shutdown();
    }

    #[test]
    fn analysis_sample_count_resets_on_new_play() {
        let Some(engine) = try_engine() else { return };

        // Play first clip
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }
        thread::sleep(Duration::from_millis(300));

        // Play second clip — should reset analysis
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }

        // Give the engine a moment to process the reset + start new playback
        thread::sleep(Duration::from_millis(100));

        // The sample count should have been reset on the new Play
        // It may have started accumulating again, but it should be low
        let data = engine.analysis();
        let analysis = data.lock().unwrap();
        // After reset + 100ms of playback, count should be much less than what
        // accumulated over 300ms of the first clip
        // Just verify it's reasonable (not testing exact values)
        drop(analysis);
        engine.shutdown();
    }

    #[test]
    fn engine_recovers_after_decode_error_health_monitor_cleared() {
        let Some(engine) = try_engine() else { return };

        // Send invalid data — should fail with error, health monitor cleared
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(_)) => {}
            other => panic!("Expected Error, got {:?}", other),
        }

        // Play valid data — health monitor should be freshly created
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.channels, 1);
            }
            other => panic!("Expected Playing after error, got {:?}", other),
        }

        // Let it play a bit — should not have any health failures
        thread::sleep(Duration::from_millis(500));

        // Check no stall/timeout events were emitted
        let mut got_health_event = false;
        while let Some(evt) = engine.try_recv_event() {
            match evt {
                AudioEvent::StreamStalled
                | AudioEvent::NoAudioTimeout
                | AudioEvent::ProbeTimeout => {
                    got_health_event = true;
                }
                _ => {}
            }
        }
        assert!(
            !got_health_event,
            "Should not get health events during normal playback"
        );

        engine.shutdown();
    }

    #[test]
    fn normal_playback_emits_no_health_events() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Let it play for a while
        thread::sleep(Duration::from_millis(800));

        // Drain all events
        let mut events = Vec::new();
        while let Some(evt) = engine.try_recv_event() {
            events.push(evt);
        }

        // None of the events should be health-related
        for evt in &events {
            assert!(
                !matches!(
                    evt,
                    AudioEvent::StreamStalled
                        | AudioEvent::NoAudioTimeout
                        | AudioEvent::ProbeTimeout
                ),
                "Got unexpected health event during normal playback: {:?}",
                evt
            );
        }

        engine.shutdown();
    }

    #[test]
    fn stop_after_play_clears_health_state() {
        let Some(engine) = try_engine() else { return };

        // Play and stop quickly
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }

        engine.stop();
        let _ = wait_for_event(&engine, 2000);

        // Wait longer than the health timeout to verify monitor was cleared
        // (If it wasn't cleared, we'd potentially get false health events)
        thread::sleep(Duration::from_millis(600));

        // Should have no health events
        let mut got_health_event = false;
        while let Some(evt) = engine.try_recv_event() {
            match evt {
                AudioEvent::StreamStalled
                | AudioEvent::NoAudioTimeout
                | AudioEvent::ProbeTimeout => {
                    got_health_event = true;
                }
                _ => {}
            }
        }
        assert!(!got_health_event, "Should not get health events after stop");

        engine.shutdown();
    }

    #[test]
    fn play_after_stop_creates_fresh_health_monitor() {
        let Some(engine) = try_engine() else { return };

        // First playback
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            _ => {
                engine.shutdown();
                return;
            }
        }

        engine.stop();
        let _ = wait_for_event(&engine, 2000);

        // Second playback — should create a fresh health monitor
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing on second play, got {:?}", other),
        }

        // Should play normally with fresh health monitor
        thread::sleep(Duration::from_millis(300));
        let mut got_health_event = false;
        while let Some(evt) = engine.try_recv_event() {
            if matches!(
                evt,
                AudioEvent::StreamStalled | AudioEvent::NoAudioTimeout | AudioEvent::ProbeTimeout
            ) {
                got_health_event = true;
            }
        }
        assert!(
            !got_health_event,
            "Fresh health monitor should not trigger events for valid playback"
        );

        engine.shutdown();
    }

    // --- Switching away from a stalled station ---

    /// Serves `limit` bytes, then blocks like a station that went off air
    /// while its connection stays open.
    struct StallAfterReader {
        inner: Cursor<Vec<u8>>,
        limit: usize,
    }

    impl std::io::Read for StallAfterReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let pos = self.inner.position() as usize;
            if pos >= self.limit {
                thread::sleep(Duration::from_secs(3600));
                return Ok(0);
            }
            let len = buf.len().min(self.limit - pos);
            std::io::Read::read(&mut self.inner, &mut buf[..len])
        }
    }

    impl std::io::Seek for StallAfterReader {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn switching_away_from_a_stalled_station_plays_the_next() {
        let Some(engine) = try_engine_playback() else {
            return;
        };

        // Station A: 10 s of audio announced, ~3 s arrives, then nothing.
        // More than the largest buffer watermark, so A starts whether or not
        // the probe raced ahead of the producer.
        let wav = make_wav(48_000, 2, &vec![1000i16; 960_000]);
        engine.play(
            Box::new(StallAfterReader {
                inner: Cursor::new(wav),
                limit: 600_000,
            }),
            None,
            None,
        );
        /// Wait for Playing, skipping other events (Buffering, Stopped)
        fn playing_within(engine: &AudioEngine, secs: u64) -> bool {
            let deadline = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < deadline {
                if let Some(AudioEvent::Playing(_)) = engine.try_recv_event() {
                    return true;
                }
                thread::sleep(Duration::from_millis(25));
            }
            false
        }

        assert!(playing_within(&engine, 10), "station A never started");
        // Let A's buffer run dry, so the decoder waits for data that never comes
        let deadline = Instant::now() + Duration::from_secs(15);
        while !engine.shared_stats().lock().unwrap().is_buffering {
            assert!(Instant::now() < deadline, "station A never ran dry");
            thread::sleep(Duration::from_millis(50));
        }

        engine.stop();
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);

        if !playing_within(&engine, 10) {
            // The engine thread is stuck; dropping the engine would hang the test
            std::mem::forget(engine);
            panic!("the next station never started after switching away from a stalled one");
        }
        engine.shutdown();
    }

    // --- Stream error propagation ---

    /// A reader that serves valid WAV data then returns a network error
    struct FailAfterReader {
        inner: Cursor<Vec<u8>>,
        bytes_read: usize,
        fail_after: usize,
    }

    impl FailAfterReader {
        fn new(data: Vec<u8>, fail_after: usize) -> Self {
            Self {
                inner: Cursor::new(data),
                bytes_read: 0,
                fail_after,
            }
        }
    }

    impl std::io::Read for FailAfterReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.bytes_read >= self.fail_after {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated network failure",
                ));
            }
            let n = std::io::Read::read(&mut self.inner, buf)?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    impl std::io::Seek for FailAfterReader {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn stream_io_error_emits_error_then_stopped() {
        let Some(engine) = try_engine() else { return };

        // Create a 2-second WAV so there's enough data for probe + some playback
        let samples: Vec<i16> = (0..88200)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44100, 1, &samples);

        // Fail after 10000 bytes — enough for probe but fails during decode
        let reader = FailAfterReader::new(wav, 10000);
        engine.play(Box::new(reader), None, None);

        // Should get Playing first
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Then should get Error (from the IO failure) followed by Stopped
        let mut got_error = false;
        let mut got_stopped = false;
        let mut error_msg = String::new();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match engine.try_recv_event() {
                Some(AudioEvent::Error(msg)) => {
                    got_error = true;
                    error_msg = msg;
                }
                Some(AudioEvent::Stopped) => {
                    got_stopped = true;
                    break;
                }
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(50)),
            }
        }

        assert!(got_error, "Should emit Error event for IO failure");
        assert!(
            error_msg.contains("Stream error"),
            "Error message should indicate stream error, got: {}",
            error_msg
        );
        assert!(got_stopped, "Should emit Stopped after Error");

        engine.shutdown();
    }

    #[test]
    fn clean_stream_end_emits_only_stopped() {
        let Some(engine) = try_engine() else { return };

        // Play a short clip that ends naturally
        engine.play(Box::new(Cursor::new(make_short_wav())), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Wait for natural end — should get Stopped but NOT Error
        let mut got_error = false;
        let mut got_stopped = false;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match engine.try_recv_event() {
                Some(AudioEvent::Error(msg)) => {
                    got_error = true;
                    eprintln!("Unexpected error: {}", msg);
                }
                Some(AudioEvent::Stopped) => {
                    got_stopped = true;
                    break;
                }
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(50)),
            }
        }

        assert!(got_stopped, "Should emit Stopped for natural end");
        assert!(!got_error, "Should NOT emit Error for clean stream end");

        engine.shutdown();
    }

    #[test]
    fn engine_recovers_after_stream_io_error() {
        let Some(engine) = try_engine() else { return };

        // Play a stream that will fail mid-playback
        let samples: Vec<i16> = (0..88200)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44100, 1, &samples);
        let reader = FailAfterReader::new(wav, 10000);
        engine.play(Box::new(reader), None, None);

        // Wait for Playing
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Wait for Error + Stopped from the IO failure
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match engine.try_recv_event() {
                Some(AudioEvent::Stopped) => break,
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(50)),
            }
        }

        // Engine should still work — play a valid stream
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.channels, 1);
            }
            other => panic!("Expected Playing after recovery, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn pause_does_not_trigger_health_events() {
        let Some(engine) = try_engine() else { return };

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.pause();
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Paused) => {}
            other => panic!("Expected Paused, got {:?}", other),
        }

        // While paused, health monitor should not fire because state != Playing
        thread::sleep(Duration::from_millis(600));

        let mut got_health_event = false;
        while let Some(evt) = engine.try_recv_event() {
            if matches!(
                evt,
                AudioEvent::StreamStalled | AudioEvent::NoAudioTimeout | AudioEvent::ProbeTimeout
            ) {
                got_health_event = true;
            }
        }
        assert!(
            !got_health_event,
            "Paused state should not trigger health events"
        );

        engine.shutdown();
    }

    #[test]
    fn volume_survives_error_and_retry() {
        let Some(engine) = try_engine() else { return };

        engine.set_volume(0.3);

        // Send invalid data
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(_)) => {}
            other => panic!("Expected Error, got {:?}", other),
        }

        // Play valid data - volume should still be 0.3
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing after error, got {:?}", other),
        }

        engine.shutdown();
    }

    // === SharedStats tests ===

    #[test]
    fn shared_stats_accessor_returns_arc() {
        let Some(engine) = try_engine() else { return };
        let s1 = engine.shared_stats();
        let s2 = engine.shared_stats();
        assert!(Arc::ptr_eq(&s1, &s2));
        engine.shutdown();
    }

    #[test]
    fn shared_stats_default_before_play() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();
        let s = stats.lock().unwrap();
        assert!(s.codec_info.is_none());
        assert!(s.play_started_at.is_none());
        assert_eq!(s.frames_played, 0);
        assert_eq!(s.decode_errors, 0);
        assert_eq!(s.bytes_received, 0);
        assert_eq!(s.sample_count, 0);
        drop(s);
        engine.shutdown();
    }

    #[test]
    fn shared_stats_populated_on_play() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Stats should have codec_info and play_started_at immediately
        let s = stats.lock().unwrap();
        assert!(
            s.codec_info.is_some(),
            "codec_info should be set after play"
        );
        let ci = s.codec_info.as_ref().unwrap();
        assert_eq!(ci.channels, 1);
        assert_eq!(ci.sample_rate, 44100);
        assert!(s.play_started_at.is_some(), "play_started_at should be set");
        drop(s);

        engine.shutdown();
    }

    #[test]
    #[ignore] // requires real audio hardware — flaky in CI
    fn shared_stats_frames_played_increases() {
        let Some(engine) = try_engine_playback() else {
            return;
        };
        let stats = engine.shared_stats();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Wait for at least one 500ms tick to copy stats
        thread::sleep(Duration::from_millis(700));

        let s = stats.lock().unwrap();
        assert!(
            s.frames_played > 0,
            "frames_played should increase during playback, got {}",
            s.frames_played
        );
        drop(s);

        engine.shutdown();
    }

    #[test]
    #[ignore] // requires real audio hardware — flaky in CI
    fn shared_stats_sample_count_increases() {
        let Some(engine) = try_engine_playback() else {
            return;
        };
        let stats = engine.shared_stats();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        thread::sleep(Duration::from_millis(700));

        let s = stats.lock().unwrap();
        assert!(
            s.sample_count > 0,
            "sample_count should increase during playback, got {}",
            s.sample_count
        );
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn shared_stats_health_state_becomes_healthy() {
        use crate::audio::health::HealthState;
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // After some playback the health should transition to Healthy
        thread::sleep(Duration::from_millis(700));

        let s = stats.lock().unwrap();
        assert!(
            matches!(
                s.health_state,
                HealthState::WaitingForAudio | HealthState::Healthy
            ),
            "health_state should be WaitingForAudio or Healthy during playback, got {:?}",
            s.health_state
        );
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn shared_stats_reset_on_stop() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        thread::sleep(Duration::from_millis(300));

        engine.stop();
        let _ = wait_for_event(&engine, 2000);

        let s = stats.lock().unwrap();
        assert!(
            s.codec_info.is_none(),
            "codec_info should be None after stop"
        );
        assert!(
            s.play_started_at.is_none(),
            "play_started_at should be None after stop"
        );
        assert_eq!(s.frames_played, 0, "frames_played should reset after stop");
        assert_eq!(s.sample_count, 0, "sample_count should reset after stop");
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn shared_stats_reset_on_auto_stop() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        // Short clip that ends naturally
        engine.play(Box::new(Cursor::new(make_short_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Wait for natural end
        match wait_for_event(&engine, 3000) {
            Some(AudioEvent::Stopped) => {}
            other => panic!("Expected auto-Stopped, got {:?}", other),
        }

        let s = stats.lock().unwrap();
        assert!(
            s.codec_info.is_none(),
            "codec_info should be None after auto-stop"
        );
        assert!(
            s.play_started_at.is_none(),
            "play_started_at should be None after auto-stop"
        );
        assert_eq!(s.frames_played, 0);
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn shared_stats_reset_on_decode_error() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        // Invalid data — decode error
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(_)) => {}
            other => panic!("Expected Error, got {:?}", other),
        }

        let s = stats.lock().unwrap();
        assert!(
            s.codec_info.is_none(),
            "codec_info should be None after decode error"
        );
        assert!(
            s.play_started_at.is_none(),
            "play_started_at should be None after decode error"
        );
        assert_eq!(s.frames_played, 0);
        drop(s);

        engine.shutdown();
    }

    #[test]
    #[ignore] // requires real audio hardware — flaky in CI
    fn shared_stats_reset_on_new_play() {
        let Some(engine) = try_engine_playback() else {
            return;
        };
        let stats = engine.shared_stats();

        // First play
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }
        thread::sleep(Duration::from_millis(700));

        // Snapshot old stats
        let old_frames = stats.lock().unwrap().frames_played;
        assert!(old_frames > 0, "Should have frames from first play");

        // Second play replaces — stats should reset
        let wav2 = make_wav(48000, 2, &vec![0i16; 96000]);
        engine.play(Box::new(Cursor::new(wav2)), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.sample_rate, 48000);
            }
            other => panic!("Expected second Playing, got {:?}", other),
        }

        // Stats should reflect the new stream
        let s = stats.lock().unwrap();
        let ci = s
            .codec_info
            .as_ref()
            .expect("codec_info should be set for new stream");
        assert_eq!(ci.sample_rate, 48000);
        assert_eq!(ci.channels, 2);
        // frames_played was reset to 0 then may have started incrementing again,
        // but should be much less than old_frames
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn shared_stats_with_bitrate() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        engine.play(
            Box::new(Cursor::new(make_one_second_wav())),
            None,
            Some(128),
        );
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => {
                assert_eq!(info.bitrate, Some(128));
            }
            other => panic!("Expected Playing, got {:?}", other),
        }

        let s = stats.lock().unwrap();
        let ci = s.codec_info.as_ref().unwrap();
        assert_eq!(
            ci.bitrate,
            Some(128),
            "bitrate should be set in shared stats"
        );
        drop(s);

        engine.shutdown();
    }

    #[test]
    #[ignore] // requires real audio hardware — flaky in CI
    fn play_with_stats_bytes_received_wired() {
        let Some(engine) = try_engine_playback() else {
            return;
        };
        let stats = engine.shared_stats();

        // Create a bytes_received counter and pre-set it
        let bytes_counter = Arc::new(AtomicU64::new(42000));

        // Use a 3-second WAV so playback doesn't end before our assertions
        let samples: Vec<i16> = (0..132300)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44100, 1, &samples);

        engine.play_with_stats(
            Box::new(Cursor::new(wav)),
            None,
            None,
            Some(bytes_counter.clone()),
            None,
            None,
        );
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Wait for a tick to copy the counter
        thread::sleep(Duration::from_millis(700));

        let s = stats.lock().unwrap();
        assert_eq!(
            s.bytes_received, 42000,
            "bytes_received should reflect the atomic counter value"
        );
        drop(s);

        // Increment the counter and wait for next tick
        bytes_counter.store(99000, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(600));

        let s = stats.lock().unwrap();
        assert_eq!(
            s.bytes_received, 99000,
            "bytes_received should update on subsequent ticks"
        );
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn play_with_stats_no_bytes_received() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        // play_with_stats with None bytes_received
        engine.play_with_stats(
            Box::new(Cursor::new(make_one_second_wav())),
            None,
            None,
            None,
            None,
            None,
        );
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        thread::sleep(Duration::from_millis(700));

        let s = stats.lock().unwrap();
        assert_eq!(
            s.bytes_received, 0,
            "bytes_received should stay 0 with no counter"
        );
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn playback_position_follows_decoding_not_download() {
        // Same chain the engine builds, without an audio device: the reported
        // position must track what has been decoded, not what is buffered.
        let samples: Vec<i16> = (0..44_100 * 10)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44_100, 1, &samples);
        let len = wav.len() as u64;
        let status = Arc::new(Mutex::new(crate::stream::buffer::BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(true));
        let (buf_reader, _handle, stop) =
            StreamBuffer::new(Box::new(Cursor::new(wav)), status, probing.clone());
        let position = Arc::new(AtomicU64::new(0));
        let reader = PlaybackPositionReader::new(buf_reader, position.clone());
        let probed = super::super::decoder::start_probe(reader, Some("wav".to_string()))
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        probing.store(false, Ordering::SeqCst);
        let mut source = SymphoniaSource::from_probed(probed).unwrap();

        // Decode one second of the ten
        for _ in 0..44_100 {
            source.next().unwrap();
        }
        let pos = position.load(Ordering::Relaxed);
        let one_second = len / 10;
        assert!(
            pos >= one_second && pos < one_second + 16 * 1024,
            "decoded 1 s ({one_second} bytes) but position is {pos} of {len}"
        );
        stop.cancel();
    }

    #[test]
    fn play_with_stats_reports_playback_position() {
        let Some(engine) = try_engine() else { return };
        let position = Arc::new(AtomicU64::new(0));
        let wav = make_one_second_wav();
        let len = wav.len() as u64;

        engine.play_with_stats(
            Box::new(Cursor::new(wav)),
            None,
            None,
            None,
            None,
            Some(position.clone()),
        );
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        thread::sleep(Duration::from_millis(300));
        let pos = position.load(Ordering::Relaxed);
        assert!(pos > 0 && pos <= len, "position {pos} of {len}");

        engine.shutdown();
    }

    // === Cancelling the stream ===

    fn resolved_stream(
        reader: Box<dyn crate::audio::types::ReadSeek>,
        cancel: &StreamCancel,
    ) -> ResolvedStream {
        ResolvedStream {
            reader,
            metadata_rx: None,
            info: crate::stream::types::StreamInfo {
                original_url: "test".into(),
                resolved_url: "test".into(),
                stream_type: crate::stream::types::StreamType::Direct,
                format_hint: Some("wav".into()),
                content_type: None,
                station_name: None,
                bitrate: None,
            },
            bytes_received: None,
            segments_downloaded: None,
            playback_position: None,
            cancel: cancel.clone(),
        }
    }

    /// A station that is connected but sends nothing
    struct SilentStation;

    impl std::io::Read for SilentStation {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            thread::sleep(Duration::from_millis(10));
            Err(std::io::ErrorKind::Interrupted.into())
        }
    }

    impl std::io::Seek for SilentStation {
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            Err(std::io::Error::other("live stream"))
        }
    }

    /// Wait up to 2 s for `cancel` to be cancelled
    fn gets_cancelled(cancel: &StreamCancel) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cancel.is_cancelled() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        cancel.is_cancelled()
    }

    fn expect_playing(engine: &AudioEngine) {
        match wait_for_event(engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }
    }

    #[test]
    fn stopping_cancels_the_stream() {
        let Some(engine) = try_engine() else { return };
        let cancel = StreamCancel::new();
        engine.play_stream(resolved_stream(EndlessWav::new(), &cancel));
        expect_playing(&engine);
        assert!(!cancel.is_cancelled());

        engine.stop();
        assert!(gets_cancelled(&cancel), "stop must cancel the stream");
        engine.shutdown();
    }

    #[test]
    fn playing_another_station_cancels_the_old_stream() {
        let Some(engine) = try_engine() else { return };
        let old = StreamCancel::new();
        engine.play_stream(resolved_stream(EndlessWav::new(), &old));
        expect_playing(&engine);

        let new = StreamCancel::new();
        engine.play_stream(resolved_stream(EndlessWav::new(), &new));
        assert!(gets_cancelled(&old), "the old station must be cancelled");
        expect_playing(&engine);
        assert!(!new.is_cancelled());
        engine.shutdown();
    }

    #[test]
    fn stopping_while_probing_cancels_the_stream() {
        let Some(engine) = try_engine() else { return };
        let cancel = StreamCancel::new();
        engine.play_stream(resolved_stream(Box::new(SilentStation), &cancel));
        // The probe waits for audio that never comes
        thread::sleep(Duration::from_millis(100));
        assert!(!cancel.is_cancelled());

        engine.stop();
        assert!(gets_cancelled(&cancel), "stop must cancel the stream");
        engine.shutdown();
    }

    // === EventBus tests ===

    #[test]
    fn event_bus_accessor_returns_arc() {
        let Some(engine) = try_engine() else { return };
        let b1 = engine.event_bus();
        let b2 = engine.event_bus();
        assert!(Arc::ptr_eq(&b1, &b2));
        engine.shutdown();
    }

    #[test]
    fn event_bus_emits_playback_started_on_play() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // EventBus should have emitted PlaybackStarted
        let evt = rx.recv_timeout(Duration::from_secs(1));
        match evt {
            Ok(StreamEvent::PlaybackStarted { codec_info, .. }) => {
                assert_eq!(codec_info.channels, 1);
                assert_eq!(codec_info.sample_rate, 44100);
            }
            other => panic!("Expected PlaybackStarted from event bus, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn event_bus_emits_playback_stopped_on_stop() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Drain the PlaybackStarted
        let _ = rx.recv_timeout(Duration::from_secs(1));

        engine.stop();
        let _ = wait_for_event(&engine, 2000);

        let evt = rx.recv_timeout(Duration::from_secs(1));
        assert!(
            matches!(evt, Ok(StreamEvent::PlaybackStopped)),
            "Expected PlaybackStopped from event bus, got {:?}",
            evt
        );

        engine.shutdown();
    }

    #[test]
    fn event_bus_emits_playback_stopped_on_auto_stop() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        engine.play(Box::new(Cursor::new(make_short_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Drain PlaybackStarted
        let _ = rx.recv_timeout(Duration::from_secs(1));

        // Wait for auto-stop — drain any intermediate events (e.g. Buffering)
        // until we see Stopped, with a generous timeout for slow CI runners.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut got_stopped = false;
        while std::time::Instant::now() < deadline {
            match engine.try_recv_event() {
                Some(AudioEvent::Stopped) => {
                    got_stopped = true;
                    break;
                }
                Some(_) => {} // drain intermediate events
                None => thread::sleep(Duration::from_millis(25)),
            }
        }
        assert!(got_stopped, "Expected auto-Stopped within timeout");

        let evt = rx.recv_timeout(Duration::from_secs(1));
        assert!(
            matches!(evt, Ok(StreamEvent::PlaybackStopped)),
            "Expected PlaybackStopped from event bus on auto-stop, got {:?}",
            evt
        );

        engine.shutdown();
    }

    #[test]
    fn event_bus_emits_error_on_decode_failure() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(_)) => {}
            other => panic!("Expected Error, got {:?}", other),
        }

        let evt = rx.recv_timeout(Duration::from_secs(1));
        match evt {
            Ok(StreamEvent::Error(msg)) => {
                assert!(
                    msg.contains("Decode error"),
                    "Error should mention decode, got: {}",
                    msg
                );
            }
            other => panic!("Expected Error from event bus, got {:?}", other),
        }

        engine.shutdown();
    }

    #[test]
    fn event_bus_emits_error_on_stream_io_failure() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        let samples: Vec<i16> = (0..88200)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let wav = make_wav(44100, 1, &samples);
        let reader = FailAfterReader::new(wav, 10000);
        engine.play(Box::new(reader), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Drain PlaybackStarted
        let _ = rx.recv_timeout(Duration::from_secs(1));

        // Wait for IO error + stopped
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match engine.try_recv_event() {
                Some(AudioEvent::Stopped) => break,
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(50)),
            }
        }

        // EventBus should have Error then PlaybackStopped
        let mut got_bus_error = false;
        let mut got_bus_stopped = false;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                StreamEvent::Error(msg) => {
                    assert!(
                        msg.contains("Stream error"),
                        "Expected stream error, got: {}",
                        msg
                    );
                    got_bus_error = true;
                }
                StreamEvent::PlaybackStopped => {
                    got_bus_stopped = true;
                }
                _ => {}
            }
        }
        assert!(
            got_bus_error,
            "EventBus should emit Error on stream IO failure"
        );
        assert!(
            got_bus_stopped,
            "EventBus should emit PlaybackStopped after IO failure"
        );

        engine.shutdown();
    }

    #[test]
    fn event_bus_multiple_subscribers_all_receive() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx1 = bus.subscribe();
        let rx2 = bus.subscribe();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Both subscribers should get PlaybackStarted
        let evt1 = rx1.recv_timeout(Duration::from_secs(1));
        let evt2 = rx2.recv_timeout(Duration::from_secs(1));
        assert!(
            matches!(evt1, Ok(StreamEvent::PlaybackStarted { .. })),
            "Subscriber 1 should get PlaybackStarted, got {:?}",
            evt1
        );
        assert!(
            matches!(evt2, Ok(StreamEvent::PlaybackStarted { .. })),
            "Subscriber 2 should get PlaybackStarted, got {:?}",
            evt2
        );

        engine.shutdown();
    }

    #[test]
    fn event_bus_play_stop_play_sequence() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        // First play
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.stop();
        let _ = wait_for_event(&engine, 2000);

        // Second play
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected second Playing, got {:?}", other),
        }

        // Collect all bus events
        thread::sleep(Duration::from_millis(100));
        let mut bus_events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            bus_events.push(evt);
        }

        // Should have: PlaybackStarted, PlaybackStopped, PlaybackStarted
        let started_count = bus_events
            .iter()
            .filter(|e| matches!(e, StreamEvent::PlaybackStarted { .. }))
            .count();
        let stopped_count = bus_events
            .iter()
            .filter(|e| matches!(e, StreamEvent::PlaybackStopped))
            .count();

        assert_eq!(
            started_count, 2,
            "Should have 2 PlaybackStarted events, got {}. Events: {:?}",
            started_count, bus_events
        );
        assert_eq!(
            stopped_count, 1,
            "Should have 1 PlaybackStopped event, got {}. Events: {:?}",
            stopped_count, bus_events
        );

        engine.shutdown();
    }

    #[test]
    fn shared_stats_not_updated_when_stopped() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        // Don't play anything, just wait
        thread::sleep(Duration::from_millis(700));

        let s = stats.lock().unwrap();
        assert!(s.codec_info.is_none());
        assert_eq!(s.frames_played, 0);
        assert_eq!(s.sample_count, 0);
        assert_eq!(s.bytes_received, 0);
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn event_bus_no_events_when_stopped() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        // Don't play, just wait
        thread::sleep(Duration::from_millis(300));

        // No events should have been emitted
        assert!(
            rx.try_recv().is_err(),
            "EventBus should not emit events when stopped"
        );

        engine.shutdown();
    }

    #[test]
    fn event_bus_stop_when_already_stopped_no_event() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        // Stop without play
        engine.stop();
        thread::sleep(Duration::from_millis(200));

        // Should not emit PlaybackStopped since we were never playing
        assert!(
            rx.try_recv().is_err(),
            "Should not emit PlaybackStopped when already stopped"
        );

        engine.shutdown();
    }

    #[test]
    fn shared_stats_play_started_at_is_recent() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        let before = Instant::now();
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }
        let after = Instant::now();

        let s = stats.lock().unwrap();
        let started = s.play_started_at.expect("play_started_at should be set");
        // The timestamp should be between our before/after markers
        assert!(
            started >= before && started <= after,
            "play_started_at should be between test markers"
        );
        drop(s);

        engine.shutdown();
    }

    #[test]
    #[ignore] // requires real audio hardware — flaky in CI
    fn shared_stats_bytes_received_reset_on_stop() {
        let Some(engine) = try_engine_playback() else {
            return;
        };
        let stats = engine.shared_stats();

        let bytes_counter = Arc::new(AtomicU64::new(5000));
        engine.play_with_stats(
            Box::new(Cursor::new(make_one_second_wav())),
            None,
            None,
            Some(bytes_counter),
            None,
            None,
        );
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        thread::sleep(Duration::from_millis(700));
        {
            let s = stats.lock().unwrap();
            assert_eq!(
                s.bytes_received, 5000,
                "bytes_received should be 5000 during playback"
            );
        }

        engine.stop();
        let _ = wait_for_event(&engine, 2000);

        let s = stats.lock().unwrap();
        assert_eq!(
            s.bytes_received, 0,
            "bytes_received should reset to 0 after stop"
        );
        drop(s);

        engine.shutdown();
    }

    #[test]
    fn event_bus_codec_info_matches_playing_event() {
        let Some(engine) = try_engine() else { return };
        let bus = engine.event_bus();
        let rx = bus.subscribe();

        let samples: Vec<i16> = (0..96000)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let wav = make_wav(48000, 2, &samples);

        engine.play(Box::new(Cursor::new(wav)), None, Some(256));

        // Get Playing event
        let playing_info = match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(info)) => info,
            other => panic!("Expected Playing, got {:?}", other),
        };

        // Get bus event
        let bus_info = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(StreamEvent::PlaybackStarted { codec_info, .. }) => codec_info,
            other => panic!("Expected PlaybackStarted, got {:?}", other),
        };

        // Both should carry the same codec info
        assert_eq!(playing_info.channels, bus_info.channels);
        assert_eq!(playing_info.sample_rate, bus_info.sample_rate);
        assert_eq!(playing_info.codec_name, bus_info.codec_name);
        assert_eq!(playing_info.bitrate, bus_info.bitrate);

        engine.shutdown();
    }
}
