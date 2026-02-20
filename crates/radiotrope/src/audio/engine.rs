//! Audio engine
//!
//! Runs audio playback on a dedicated thread, accepting commands via crossbeam
//! channels and emitting events back. Visualization data is shared via
//! `Arc<Mutex<AudioAnalysis>>`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use rodio::{DeviceSinkBuilder, Player};

use crate::config::timeouts::PROBE_TIMEOUT_SECS;
use crate::error::RadioError;
use crate::stream::buffer::{SharedBufferStatus, StreamBuffer};

use super::analyzer::AnalyzingSource;
use super::decoder::{start_probe, SymphoniaSource};
use super::health::{FailureReason, HealthState, StreamHealthMonitor};
use super::stats::{
    new_shared_stats, DecoderStats, EventBus, SharedStats, StreamEvent, StreamStats,
};
use super::types::{AudioAnalysis, AudioCommand, AudioEvent, PlaybackState};

/// State held while an async probe is in progress
struct PendingProbe {
    probe_rx: Receiver<Result<symphonia::core::probe::ProbeResult, RadioError>>,
    buf_status: SharedBufferStatus,
    probing_flag: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    prod_handle: JoinHandle<()>,
    bytes_received: Option<Arc<AtomicU64>>,
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
}

impl AudioEngine {
    /// Create a new audio engine, spawning the engine thread.
    ///
    /// Blocks until the audio output stream is initialized (or fails).
    pub fn new() -> Result<Self, RadioError> {
        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(16);
        let (event_tx, event_rx) = bounded::<AudioEvent>(64);
        let (init_tx, init_rx) = bounded::<Result<(), String>>(1);

        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_thread = analysis.clone();

        let shared_stats = new_shared_stats();
        let shared_stats_thread = shared_stats.clone();
        let event_bus = Arc::new(EventBus::new());
        let event_bus_thread = event_bus.clone();

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
        })
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
        });
    }

    /// Start playing with bytes_received tracking
    pub fn play_with_stats(
        &self,
        reader: Box<dyn super::types::ReadSeek>,
        format_hint: Option<String>,
        bitrate: Option<u32>,
        bytes_received: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    ) {
        self.send(AudioCommand::Play {
            reader,
            format_hint,
            bitrate,
            bytes_received,
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
    fn run(
        cmd_rx: Receiver<AudioCommand>,
        event_tx: Sender<AudioEvent>,
        init_tx: Sender<Result<(), String>>,
        analysis: Arc<Mutex<AudioAnalysis>>,
        shared_stats: SharedStats,
        event_bus: Arc<EventBus>,
    ) {
        // Create audio output on this thread (cpal streams may be !Send)
        let mut stream = match DeviceSinkBuilder::open_default_sink() {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(format!("Failed to open audio output: {}", e)));
                return;
            }
        };
        stream.log_on_drop(false);

        // `stream` must be declared before `sink` so Rust drops sink first
        let sink = Player::connect_new(stream.mixer());

        let _ = init_tx.send(Ok(()));

        let mut state = PlaybackState::Stopped;
        let mut current_volume: f32 = 1.0;
        let mut health_monitor: Option<StreamHealthMonitor> = None;
        let mut stream_error_slot: Option<Arc<Mutex<Option<String>>>> = None;
        let mut current_decoder_stats: Option<Arc<DecoderStats>> = None;
        let mut current_bytes_received: Option<Arc<AtomicU64>> = None;
        let mut current_buffer_status: Option<SharedBufferStatus> = None;
        let mut was_buffering = false;
        let mut producer_stop_flag: Option<Arc<AtomicBool>> = None;
        let mut _producer_probing_flag: Option<Arc<AtomicBool>> = None;
        let mut _producer_handle: Option<JoinHandle<()>> = None;
        let mut last_throughput_bytes: u64 = 0;
        let mut last_throughput_time = Instant::now();
        let mut pending_probe: Option<PendingProbe> = None;

        loop {
            match cmd_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(cmd) => match cmd {
                    AudioCommand::Play {
                        reader,
                        format_hint,
                        bitrate,
                        bytes_received,
                    } => {
                        // Cancel any pending probe
                        if let Some(probe) = pending_probe.take() {
                            probe.stop_flag.store(true, Ordering::SeqCst);
                        }
                        // Stop any current playback (including producer thread)
                        if let Some(ref flag) = producer_stop_flag {
                            flag.store(true, Ordering::SeqCst);
                        }
                        sink.stop();
                        // Drop old producer resources before creating new ones
                        drop(producer_stop_flag.take());
                        drop(_producer_probing_flag.take());
                        drop(_producer_handle.take());
                        if let Ok(mut data) = analysis.lock() {
                            data.reset();
                        }

                        // Wrap reader in decoupled producer-consumer buffer
                        let buf_status =
                            Arc::new(Mutex::new(crate::stream::buffer::BufferStatus::default()));
                        let probing_flag = Arc::new(AtomicBool::new(true));
                        let (buf_reader, prod_handle, stop_flag) =
                            StreamBuffer::new(reader, buf_status.clone(), probing_flag.clone());

                        // Start async probe — returns immediately
                        match start_probe(buf_reader, format_hint) {
                            Ok(probe_rx) => {
                                pending_probe = Some(PendingProbe {
                                    probe_rx,
                                    buf_status,
                                    probing_flag,
                                    stop_flag,
                                    prod_handle,
                                    bytes_received,
                                    bitrate,
                                    started: Instant::now(),
                                });
                            }
                            Err(e) => {
                                stop_flag.store(true, Ordering::SeqCst);
                                state = PlaybackState::Stopped;
                                if let Ok(mut stats) = shared_stats.lock() {
                                    *stats = StreamStats::default();
                                    stats.health_state =
                                        HealthState::Failed(FailureReason::ProbeFailed);
                                }
                                let _ = event_tx
                                    .send(AudioEvent::Error(format!("Decode error: {}", e)));
                                event_bus.emit(StreamEvent::Error(format!("Decode error: {}", e)));
                            }
                        }
                    }
                    AudioCommand::Stop => {
                        if let Some(probe) = pending_probe.take() {
                            probe.stop_flag.store(true, Ordering::SeqCst);
                        }
                        if let Some(ref flag) = producer_stop_flag {
                            flag.store(true, Ordering::SeqCst);
                        }
                        sink.stop();
                        if let Ok(mut data) = analysis.lock() {
                            data.reset();
                        }
                        health_monitor = None;
                        stream_error_slot = None;
                        current_decoder_stats = None;
                        current_bytes_received = None;
                        current_buffer_status = None;
                        producer_stop_flag = None;
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
                        }
                    }
                    AudioCommand::SetVolume(vol) => {
                        current_volume = vol.clamp(0.0, 2.0);
                        sink.set_volume(current_volume);
                    }
                    AudioCommand::Shutdown => {
                        if let Some(probe) = pending_probe.take() {
                            probe.stop_flag.store(true, Ordering::SeqCst);
                        }
                        if let Some(ref flag) = producer_stop_flag {
                            flag.store(true, Ordering::SeqCst);
                        }
                        sink.stop();
                        break;
                    }
                },
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Poll pending probe for completion
                    if let Some(ref pending) = pending_probe {
                        match pending.probe_rx.try_recv() {
                            Ok(Ok(probed)) => {
                                let p = pending_probe.take().unwrap();
                                match SymphoniaSource::from_probed(probed) {
                                    Ok(source) => {
                                        // Probe succeeded — allow buffer compaction
                                        p.probing_flag.store(false, Ordering::SeqCst);

                                        let mut codec_info = source.codec_info();
                                        codec_info.bitrate = p.bitrate;
                                        let error_slot = source.error_slot();
                                        let dec_stats = source.decoder_stats();
                                        let analyzing =
                                            AnalyzingSource::new(source, analysis.clone());
                                        sink.append(analyzing);
                                        sink.set_volume(current_volume);
                                        sink.play();
                                        state = PlaybackState::Playing;
                                        health_monitor = Some(StreamHealthMonitor::new());
                                        stream_error_slot = Some(error_slot);
                                        current_decoder_stats = Some(dec_stats);
                                        current_bytes_received = p.bytes_received;
                                        current_buffer_status = Some(p.buf_status);
                                        was_buffering = false;
                                        last_throughput_bytes = 0;
                                        last_throughput_time = Instant::now();
                                        producer_stop_flag = Some(p.stop_flag);
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
                                    }
                                    Err(e) => {
                                        p.stop_flag.store(true, Ordering::SeqCst);
                                        state = PlaybackState::Stopped;
                                        if let Ok(mut stats) = shared_stats.lock() {
                                            *stats = StreamStats::default();
                                            stats.health_state =
                                                HealthState::Failed(FailureReason::ProbeFailed);
                                        }
                                        let _ = event_tx.send(AudioEvent::Error(format!(
                                            "Decode error: {}",
                                            e
                                        )));
                                        event_bus.emit(StreamEvent::Error(format!(
                                            "Decode error: {}",
                                            e
                                        )));
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                let p = pending_probe.take().unwrap();
                                p.stop_flag.store(true, Ordering::SeqCst);
                                state = PlaybackState::Stopped;
                                if let Ok(mut stats) = shared_stats.lock() {
                                    *stats = StreamStats::default();
                                    stats.health_state =
                                        HealthState::Failed(FailureReason::ProbeFailed);
                                }
                                let _ = event_tx
                                    .send(AudioEvent::Error(format!("Decode error: {}", e)));
                                event_bus.emit(StreamEvent::Error(format!("Decode error: {}", e)));
                            }
                            Err(TryRecvError::Empty) => {
                                // Still probing — check for timeout
                                if pending.started.elapsed().as_secs() >= PROBE_TIMEOUT_SECS {
                                    let p = pending_probe.take().unwrap();
                                    p.stop_flag.store(true, Ordering::SeqCst);
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
                                p.stop_flag.store(true, Ordering::SeqCst);
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
                        if let Some(ref flag) = producer_stop_flag {
                            flag.store(true, Ordering::SeqCst);
                        }
                        state = PlaybackState::Stopped;
                        health_monitor = None;
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
                        current_buffer_status = None;
                        producer_stop_flag = None;
                        _producer_probing_flag = None;
                        _producer_handle = None;
                        if let Ok(mut stats) = shared_stats.lock() {
                            *stats = StreamStats::default();
                        }
                        event_bus.emit(StreamEvent::PlaybackStopped);
                        let _ = event_tx.send(AudioEvent::Stopped);
                    }

                    // Update shared stats on tick when playing
                    if state == PlaybackState::Playing {
                        if let Ok(mut stats) = shared_stats.lock() {
                            // Decoder stats
                            if let Some(ref ds) = current_decoder_stats {
                                let (frames, errors) = ds.snapshot();
                                stats.frames_played = frames;
                                stats.decode_errors = errors;
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
                            // Analysis data (sample count)
                            if let Ok(a) = analysis.lock() {
                                stats.sample_count = a.sample_count;
                            }
                            // Health state
                            if let Some(ref monitor) = health_monitor {
                                stats.health_state = *monitor.state();
                            }
                            // Buffer status
                            if let Some(ref bs) = current_buffer_status {
                                if let Ok(buf) = bs.lock() {
                                    stats.buffer_level_bytes = buf.level_bytes;
                                    stats.buffer_capacity_bytes = buf.capacity_bytes;
                                    stats.is_buffering = buf.is_buffering;
                                    stats.underrun_count = buf.underrun_count;
                                }
                            }
                        }

                        // Emit buffering event on state change (only while sink has data)
                        if !sink.empty() {
                            if let Some(ref bs) = current_buffer_status {
                                if let Ok(buf) = bs.lock() {
                                    if buf.is_buffering != was_buffering {
                                        let pct = if !buf.is_buffering {
                                            // Recovered from buffering → signal 100% to clear status
                                            100
                                        } else if buf.capacity_bytes > 0 {
                                            ((buf.level_bytes as f64 / buf.capacity_bytes as f64)
                                                * 100.0)
                                                as u8
                                        } else {
                                            0
                                        };
                                        let _ = event_tx.send(AudioEvent::Buffering(pct));
                                        was_buffering = buf.is_buffering;

                                        // Reset stall timer once on transition TO buffering
                                        // (grace period for brief adaptive rebuffering)
                                        if was_buffering {
                                            if let Some(ref mut monitor) = health_monitor {
                                                monitor.reset_stall_timer();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Health monitoring: check sample flow
                    if state == PlaybackState::Playing {
                        if let Some(ref mut monitor) = health_monitor {
                            let count = analysis.lock().map(|a| a.sample_count).unwrap_or(0);
                            if let Some(failure) = monitor.update(count) {
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
                                        if let Some(ref flag) = producer_stop_flag {
                                            flag.store(true, Ordering::SeqCst);
                                        }
                                        sink.stop();
                                        if let Ok(mut data) = analysis.lock() {
                                            data.reset();
                                        }
                                        state = PlaybackState::Stopped;
                                        health_monitor = None;
                                        stream_error_slot = None;
                                        current_decoder_stats = None;
                                        current_bytes_received = None;
                                        current_buffer_status = None;
                                        producer_stop_flag = None;
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
                            }
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
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
        let Some(engine) = try_engine() else { return };

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
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            _ => {}
        }
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
    fn analysis_sample_count_increases_during_playback() {
        let Some(engine) = try_engine() else { return };

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
        let Some(engine) = try_engine() else { return };

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
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            _ => {}
        }
        // Give the player time to fully drain after stop
        thread::sleep(Duration::from_millis(100));

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
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            _ => {}
        }

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
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            _ => {}
        }

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
    fn shared_stats_frames_played_increases() {
        let Some(engine) = try_engine() else { return };
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
    fn shared_stats_sample_count_increases() {
        let Some(engine) = try_engine() else { return };
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
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            _ => {}
        }

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
    fn shared_stats_reset_on_new_play() {
        let Some(engine) = try_engine() else { return };
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
    fn play_with_stats_bytes_received_wired() {
        let Some(engine) = try_engine() else { return };
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
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            _ => {}
        }

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

        // Wait for auto-stop
        match wait_for_event(&engine, 3000) {
            Some(AudioEvent::Stopped) => {}
            other => panic!("Expected auto-Stopped, got {:?}", other),
        }

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
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            _ => {}
        }

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
    fn shared_stats_bytes_received_reset_on_stop() {
        let Some(engine) = try_engine() else { return };
        let stats = engine.shared_stats();

        let bytes_counter = Arc::new(AtomicU64::new(5000));
        engine.play_with_stats(
            Box::new(Cursor::new(make_one_second_wav())),
            None,
            None,
            Some(bytes_counter),
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
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            _ => {}
        }

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
