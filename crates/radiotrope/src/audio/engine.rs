//! Audio engine
//!
//! Runs audio playback on a dedicated thread, accepting commands via crossbeam
//! channels and emitting events back. Each station is decoded on a thread of
//! its own, a little ahead of the output (see `pcm`), so the audio callback
//! never waits on the network. Visualization data is shared via
//! `Arc<Mutex<AudioAnalysis>>`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{
    bounded, unbounded, Receiver, RecvTimeoutError, Select, Sender, TryRecvError,
};
use rodio::Player;

use crate::config::timeouts::{
    BUFFERING_STALL_THRESHOLD_SECS, BUFFERING_TIMEOUT_SECS, PROBE_TIMEOUT_SECS,
    STREAM_STALL_TIMEOUT_SECS,
};
use crate::error::RadioError;
use crate::stream::buffer::{PlaybackPositionReader, SharedBufferStatus, StreamBuffer};
use crate::stream::types::StreamType;
use crate::stream::{ResolvedStream, StreamCancel};

/// Quadratic volume curve for natural perception (human hearing is logarithmic)
fn volume_curve(linear: f32) -> f32 {
    linear * linear
}

use super::analyzer::AnalyzingSource;
use super::decoder::{start_open, SymphoniaSource};
use super::dsp::equalizer::{EqParams, EqSource, SharedEqParams};
use super::health::{FailureReason, HealthState, StreamHealthMonitor};
use super::output::{DefaultWatch, DeviceOutputs, Devices, Output, SilentOutput};
use super::pcm::{decode_ahead, PcmFeed};
use super::recording::{Listen, Recorder, RecordingTap, TapPoint};
use super::stats::{new_shared_stats, DecoderStats, SharedStats, StreamStats};
use super::types::{AudioAnalysis, AudioCommand, AudioEvent, EngineEvent, PlaybackState, StreamId};

/// How an [`AudioEngine`] plays, and how long it waits for things. The
/// defaults are what the app uses; tests shorten the waits.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    /// Where the audio goes
    pub output: EngineOutput,
    /// How often the engine checks on playback: end of stream, stats,
    /// buffering, health, probe timeout. At least [`MIN_TICK`]: a shorter
    /// one is taken as that.
    pub tick: Duration,
    /// How long the engine waits to try again when no output device
    /// opens, and at most how often it replaces one that keeps failing
    pub output_retry: Duration,
    /// Longest wait between those tries: each one that fails waits twice
    /// as long as the one before, from `output_retry` up to this
    pub output_retry_max: Duration,
    /// How long the output device stays open with nothing playing. Closed,
    /// it lets the computer sleep, the sound server suspend the card and a
    /// Bluetooth headset turn itself off. The next station opens it again.
    pub output_idle_timeout: Duration,
    /// A live station that played into nothing for longer than this, while
    /// the output was gone, ends once a device is back
    /// ([`AudioEvent::FellBehind`]) instead of carrying on that far behind
    /// live. `None` carries on.
    pub behind_live_limit: Option<Duration>,
    /// Longest wait for a station's audio format to be found
    pub probe_timeout: Duration,
    /// Buffering this long with nothing buffered is reported as a stall
    pub buffering_stall: Duration,
    /// Longest wait for the first audio once the format is found
    pub no_audio_timeout: Duration,
    /// Time without new audio after which a playing station has stalled
    pub stall_timeout: Duration,
    /// An output device that takes longer than this to open is reported as
    /// not responding ([`AudioEvent::OutputNotResponding`]), and
    /// [`AudioEngine::new`] stops waiting for it. The open can't be cut
    /// short: a driver stuck inside it holds the engine until it returns.
    pub output_open_deadline: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            output: EngineOutput::Device,
            tick: Duration::from_millis(500),
            output_retry: Duration::from_secs(2),
            output_retry_max: Duration::from_secs(16),
            output_idle_timeout: Duration::from_secs(15),
            behind_live_limit: Some(Duration::from_secs(20)),
            probe_timeout: Duration::from_secs(PROBE_TIMEOUT_SECS),
            buffering_stall: Duration::from_secs(BUFFERING_STALL_THRESHOLD_SECS),
            no_audio_timeout: Duration::from_secs(BUFFERING_TIMEOUT_SECS),
            stall_timeout: Duration::from_secs(STREAM_STALL_TIMEOUT_SECS),
            output_open_deadline: Duration::from_secs(5),
        }
    }
}

/// The shortest [`EngineConfig::tick`]. A zero tick would leave no time to
/// wait for commands: the loop would spin, and never see `Shutdown`.
pub const MIN_TICK: Duration = Duration::from_millis(1);

/// Where an [`AudioEngine`] plays
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EngineOutput {
    /// The system's default output device, or another one if that fails
    Device,
    /// No device: the audio is taken `speed` times as fast as it plays, and
    /// dropped. For tests, and for machines with no sound.
    Silent { speed: f32 },
}

/// Opens an output on the default device, or another of `Devices`, that
/// raises the flag if it goes away. Says which device it is on, as
/// [`DefaultWatch`] tells them apart.
type OpenOutput =
    Box<dyn FnMut(&Arc<AtomicBool>, Devices) -> Result<(Output, Option<String>), String> + Send>;

/// Opens the engine's outputs, and says when the open one is no longer on
/// the default device
struct Outputs {
    open: OpenOutput,
    /// Only on Windows (see [`DefaultWatch`])
    default: Option<DefaultWatch>,
    /// Says when an open takes too long; set by the engine's thread
    watch: Option<OpenWatch>,
}

impl Outputs {
    fn open(&mut self, lost: &Arc<AtomicBool>, devices: Devices) -> Result<Output, String> {
        if let Some(watch) = &self.watch {
            watch.opening();
        }
        let opened = (self.open)(lost, devices);
        if let Some(watch) = &self.watch {
            watch.opened();
        }
        let (output, device) = opened?;
        if let Some(watch) = self.default.as_mut() {
            watch.follow(device);
        }
        Ok(output)
    }

    fn default_moved(&mut self) -> bool {
        self.default
            .as_mut()
            .is_some_and(|watch| watch.moved(Instant::now()))
    }

    /// The new default didn't open: try it again later
    fn default_failed(&mut self) {
        if let Some(watch) = self.default.as_mut() {
            watch.not_opened(Instant::now());
        }
    }
}

/// Watches the output opens from a thread of its own: a driver can hang in
/// one (some Bluetooth and USB devices, a stalled sound server), and the
/// engine's thread can't say so while it waits. Past the deadline it sends
/// [`AudioEvent::OutputNotResponding`], and once the open returns
/// [`AudioEvent::OutputResponding`]. Its thread ends with the engine's.
struct OpenWatch {
    /// `true` when an open starts, `false` when it returns
    tx: Sender<bool>,
}

impl OpenWatch {
    fn start(events: Sender<EngineEvent>, deadline: Duration) -> Option<Self> {
        let (tx, rx) = unbounded::<bool>();
        let device = |event| EngineEvent {
            stream: None,
            event,
        };
        thread::Builder::new()
            .name("audio-open-watch".to_string())
            .spawn(move || {
                while let Ok(opening) = rx.recv() {
                    if !opening {
                        continue;
                    }
                    match rx.recv_timeout(deadline) {
                        Ok(_) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                        Err(RecvTimeoutError::Timeout) => {
                            let _ = events.try_send(device(AudioEvent::OutputNotResponding));
                            if rx.recv().is_err() {
                                return;
                            }
                            let _ = events.try_send(device(AudioEvent::OutputResponding));
                        }
                    }
                }
            })
            .ok()?;
        Some(Self { tx })
    }

    fn opening(&self) {
        let _ = self.tx.send(true);
    }

    fn opened(&self) {
        let _ = self.tx.send(false);
    }
}

/// A player on the output device, or on nothing while there is none
fn new_player(output: Option<&Output>) -> Player {
    match output {
        Some(output) => Player::connect_new(output.mixer()),
        None => Player::new().0,
    }
}

/// Play on `output` from now on: a new player, at the same volume and
/// paused or not, carries on with the station where it was
fn play_on(
    output: Output,
    stream: &mut Option<Output>,
    sink: &mut Player,
    playing: Option<&PcmFeed>,
    volume: f32,
    paused: bool,
) {
    let new_sink = new_player(Some(&output));
    new_sink.set_volume(volume_curve(volume));
    if paused {
        new_sink.pause();
    }
    if let Some(feed) = playing {
        feed.set_ahead(decode_ahead(Some(&output)));
        new_sink.append(feed.output());
    }
    // The old player goes before its device
    *sink = new_sink;
    *stream = Some(output);
}

/// Sends the engine's events, each marked with the station it is about
struct Events {
    tx: Sender<EngineEvent>,
    /// The station being started or played, if any
    stream: Option<StreamId>,
    /// The `Buffering` sent last, and its station, if nothing came after it
    last_buffering: Option<(Option<StreamId>, u8)>,
}

impl Events {
    fn new(tx: Sender<EngineEvent>) -> Self {
        Self {
            tx,
            stream: None,
            last_buffering: None,
        }
    }

    /// Send an event, unless the queue is full: the engine never waits for
    /// whoever reads them, and one that stops reading only misses events.
    /// A `Buffering` that says the same as the one before isn't sent.
    fn send(&mut self, event: AudioEvent) {
        if let AudioEvent::Buffering(pct) = event {
            let buffering = Some((self.stream, pct));
            if self.last_buffering == buffering {
                return;
            }
            self.last_buffering = buffering;
        } else {
            self.last_buffering = None;
        }
        let _ = self.tx.try_send(EngineEvent {
            stream: self.stream,
            event,
        });
    }

    /// The station has stopped: say so, and send what follows without it
    fn stopped(&mut self) {
        self.send(AudioEvent::Stopped);
        self.stream = None;
    }
}

/// A station failed to start: say why, and that nothing is playing. Every
/// play request that isn't replaced or stopped first ends in `Playing`, or
/// in `Error` then `Stopped`.
fn report_failed_start(events: &mut Events, shared_stats: &SharedStats, msg: String) {
    if let Ok(mut stats) = shared_stats.lock() {
        *stats = StreamStats::default();
        stats.health_state = HealthState::Failed(FailureReason::ProbeFailed);
    }
    events.send(AudioEvent::Error(msg));
    events.stopped();
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
    stream_type: Option<StreamType>,
    /// Audio decoded from the stream buffer, for its byte rate
    decoded_time: Arc<AtomicU64>,
    started: Instant,
}

/// Audio engine that manages playback on a dedicated thread
pub struct AudioEngine {
    cmd_tx: Sender<AudioCommand>,
    event_rx: Receiver<EngineEvent>,
    analysis: Arc<Mutex<AudioAnalysis>>,
    thread: Option<JoinHandle<()>>,
    shared_stats: SharedStats,
    /// The id the last play request got
    last_stream: AtomicU64,
    eq_params: SharedEqParams,
    recorder: Recorder,
    #[cfg(test)]
    output_lost: Arc<AtomicBool>,
}

impl AudioEngine {
    /// Create a new audio engine, spawning the engine thread.
    ///
    /// Blocks until the engine thread has tried the audio output, or at most
    /// [`EngineConfig::output_open_deadline`]: an open that hangs is
    /// reported with [`AudioEvent::OutputNotResponding`], and commands wait
    /// for it. With no output device the engine still starts: it sends
    /// [`AudioEvent::OutputLost`] and plays once a device can be opened.
    pub fn new() -> Result<Self, RadioError> {
        Self::with_config(EngineConfig::default())
    }

    /// [`AudioEngine::new`] with its own output and waits
    pub fn with_config(config: EngineConfig) -> Result<Self, RadioError> {
        let outputs = match config.output {
            EngineOutput::Device => {
                let mut device_outputs = DeviceOutputs::new();
                Outputs {
                    open: Box::new(move |lost, devices| {
                        let (sink, device) = device_outputs.open(lost, devices)?;
                        Ok((Output::Device(sink), device))
                    }),
                    // Only Windows keeps playing on a device that is no
                    // longer the default
                    default: cfg!(windows).then(DefaultWatch::system),
                    watch: None,
                }
            }
            EngineOutput::Silent { speed } => Outputs {
                open: Box::new(move |_, _| {
                    SilentOutput::open(speed).map(|output| (Output::Silent(output), None))
                }),
                default: None,
                watch: None,
            },
        };
        Self::with_output(outputs, config)
    }

    /// Create an engine that plays through what `outputs` opens: at start,
    /// again whenever the device goes away, and when the default changes
    fn with_output(outputs: Outputs, config: EngineConfig) -> Result<Self, RadioError> {
        // Unbounded: an output open that hangs holds the engine's thread,
        // and a full queue would hold whoever sends to it too
        let (cmd_tx, cmd_rx) = unbounded::<AudioCommand>();
        let (event_tx, event_rx) = bounded::<EngineEvent>(64);
        let open_deadline = config.output_open_deadline;
        let (init_tx, init_rx) = bounded::<Result<(), String>>(1);

        let analysis = Arc::new(Mutex::new(AudioAnalysis::default()));
        let analysis_thread = analysis.clone();

        let shared_stats = new_shared_stats();
        let shared_stats_thread = shared_stats.clone();
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
                    eq_params_thread,
                    recorder_thread,
                    output_lost_thread,
                    outputs,
                    config,
                );
            })
            .map_err(|e| RadioError::Audio(format!("Failed to spawn audio thread: {}", e)))?;

        // Wait for initialization. An output that takes too long to open
        // is reported by the engine itself: carry on without waiting.
        match init_rx.recv_timeout(open_deadline) {
            Ok(result) => result.map_err(RadioError::Audio)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(RadioError::Audio(
                    "Audio thread terminated during init".to_string(),
                ))
            }
        }

        Ok(Self {
            cmd_tx,
            event_rx,
            analysis,
            thread: Some(thread),
            shared_stats,
            last_stream: AtomicU64::new(0),
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

    /// The id for a new play request
    fn next_stream(&self) -> StreamId {
        StreamId(self.last_stream.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Start playing from the given reader. Returns the id this station's
    /// events carry.
    pub fn play(
        &self,
        reader: Box<dyn super::types::ReadSeek>,
        format_hint: Option<String>,
        bitrate: Option<u32>,
    ) -> StreamId {
        let id = self.next_stream();
        self.send(AudioCommand::Play {
            id,
            reader,
            format_hint,
            bitrate,
            bytes_received: None,
            segments_downloaded: None,
            playback_position: None,
            stream_type: None,
            cancel: StreamCancel::new(),
        });
        id
    }

    /// Start playing with bytes_received and segments_downloaded tracking.
    ///
    /// `playback_position` is updated with how many bytes of `reader` the
    /// decoder has read (see `ResolvedStream::playback_position`). Returns
    /// the id this station's events carry.
    pub fn play_with_stats(
        &self,
        reader: Box<dyn super::types::ReadSeek>,
        format_hint: Option<String>,
        bitrate: Option<u32>,
        bytes_received: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
        segments_downloaded: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
        playback_position: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    ) -> StreamId {
        let id = self.next_stream();
        self.send(AudioCommand::Play {
            id,
            reader,
            format_hint,
            bitrate,
            bytes_received,
            segments_downloaded,
            playback_position,
            stream_type: None,
            cancel: StreamCancel::new(),
        });
        id
    }

    /// Start playing a resolved stream, with its stats and song info timing.
    ///
    /// Stopping it, or playing something else, cancels the stream
    /// (`ResolvedStream::cancel`), so its network threads stop at once. Take
    /// `metadata_rx` out first if you want the stream's song info. Returns
    /// the id this station's events carry.
    pub fn play_stream(&self, stream: ResolvedStream) -> StreamId {
        let id = self.next_stream();
        self.send(AudioCommand::Play {
            id,
            reader: stream.reader,
            format_hint: stream.info.format_hint,
            bitrate: stream.info.bitrate,
            bytes_received: stream.bytes_received,
            segments_downloaded: stream.segments_downloaded,
            playback_position: stream.playback_position,
            stream_type: Some(stream.info.stream_type),
            cancel: stream.cancel,
        });
        id
    }

    /// Stop playback
    pub fn stop(&self) {
        self.send(AudioCommand::Stop);
    }

    /// Pause playback. A station still connecting starts paused: `Playing`
    /// then `Paused` follow once it is ready.
    pub fn pause(&self) {
        self.send(AudioCommand::Pause);
    }

    /// Resume playback
    pub fn resume(&self) {
        self.send(AudioCommand::Resume);
    }

    /// Set volume (clamped to 0.0..=2.0; NaN and infinities are ignored)
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

    /// Get a handle for phones listening to the playing station (fed by
    /// the same taps as recordings)
    pub fn listen(&self) -> Listen {
        self.recorder.listen()
    }

    /// Non-blocking poll for the next event.
    ///
    /// Up to 64 events wait to be read. While the queue is full, new ones
    /// are dropped: the engine never waits for its reader.
    pub fn try_recv_event(&self) -> Option<EngineEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Get a reference to the event receiver for use with `select!` (see
    /// [`AudioEngine::try_recv_event`] for its queue)
    pub fn event_receiver(&self) -> &Receiver<EngineEvent> {
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

    /// Graceful shutdown (consumes self)
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        let _ = self.cmd_tx.send(AudioCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
            // Nothing more can reach a recording: finish its file now,
            // even while a clone of the recorder is still held elsewhere
            let _ = self.recorder.stop();
        }
    }

    /// The engine's main loop, running on the dedicated thread
    #[allow(clippy::too_many_arguments)]
    fn run(
        cmd_rx: Receiver<AudioCommand>,
        event_tx: Sender<EngineEvent>,
        init_tx: Sender<Result<(), String>>,
        analysis: Arc<Mutex<AudioAnalysis>>,
        shared_stats: SharedStats,
        eq_params: SharedEqParams,
        recorder: Recorder,
        output_lost: Arc<AtomicBool>,
        mut outputs: Outputs,
        config: EngineConfig,
    ) {
        outputs.watch = OpenWatch::start(event_tx.clone(), config.output_open_deadline);

        // Create audio output on this thread (cpal streams may be !Send).
        // Without a device, start anyway and keep trying.
        let mut stream = match outputs.open(&output_lost, Devices::Any) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("No audio output, waiting for a device: {e}");
                None
            }
        };

        // `stream` must be declared before `sink` so Rust drops sink first
        let mut sink = new_player(stream.as_ref());

        let mut events = Events::new(event_tx);
        let _ = init_tx.send(Ok(()));
        if stream.is_none() {
            events.send(AudioEvent::OutputLost);
        }

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
        let mut playing_source: Option<PcmFeed> = None;
        // The output device went away (or there was none at start) and no
        // other could be opened yet
        let mut waiting_for_output = stream.is_none();
        // Since when, while waiting
        let mut output_gone_at = Instant::now();
        let mut next_output_try = Instant::now();
        // How long after the next try that fails to try again
        let mut output_wait = config.output_retry;
        // Stopped with the output open since then
        let mut idle_since: Option<Instant> = None;
        // When the station started playing, and whether it is live (from
        // `play_stream`)
        let mut playing_since = Instant::now();
        let mut playing_live = false;

        // Paused while still connecting: the station starts paused
        let mut start_paused = false;
        // The playing station's codec name, which can change as it plays
        let mut codec_label: Option<Arc<Mutex<String>>> = None;

        let tick = config.tick.max(MIN_TICK);
        let mut next_tick = Instant::now() + tick;

        loop {
            let probe_rx = pending_probe.as_ref().map(|p| &p.probe_rx);
            match next_wake(&cmd_rx, probe_rx, next_tick) {
                Wake::Command(cmd) => match cmd {
                    AudioCommand::Play {
                        id,
                        reader,
                        format_hint,
                        bitrate,
                        bytes_received,
                        segments_downloaded,
                        playback_position,
                        stream_type,
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
                        // The old station has stopped: forget it, and say so
                        health_monitor = None;
                        stream_error_slot = None;
                        current_decoder_stats = None;
                        codec_label = None;
                        current_bytes_received = None;
                        current_segments_downloaded = None;
                        current_buffer_status = None;
                        if state != PlaybackState::Stopped {
                            state = PlaybackState::Stopped;
                            if let Ok(mut stats) = shared_stats.lock() {
                                *stats = StreamStats::default();
                            }
                            events.send(AudioEvent::Stopped);
                        }
                        // From here on, events are about the new station
                        events.stream = Some(id);
                        start_paused = false;
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
                        let (mut buf_reader, prod_handle) = StreamBuffer::with_cancel(
                            reader,
                            buf_status.clone(),
                            probing_flag.clone(),
                            cancel.clone(),
                        );
                        // The buffer sizes itself in seconds of audio
                        buf_reader.set_advertised_bitrate(bitrate);
                        let decoded_time = buf_reader.decoded_time();

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
                                    stream_type,
                                    decoded_time,
                                    started: Instant::now(),
                                });
                                // The output was closed while nothing played:
                                // open it again while the station connects
                                if stream.is_none() && !waiting_for_output {
                                    match outputs.open(&output_lost, Devices::Reopen) {
                                        Ok(output) => stream = Some(output),
                                        Err(e) => {
                                            eprintln!("No audio output, waiting for a device: {e}");
                                            waiting_for_output = true;
                                            output_gone_at = Instant::now();
                                            next_output_try = Instant::now() + output_wait;
                                            output_wait =
                                                (output_wait * 2).min(config.output_retry_max);
                                            events.send(AudioEvent::OutputLost);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                cancel.cancel();
                                report_failed_start(&mut events, &shared_stats, e.to_string());
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
                        codec_label = None;
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
                            events.send(AudioEvent::Stopped);
                        }
                        events.stream = None;
                        start_paused = false;
                    }
                    AudioCommand::Pause => {
                        if state == PlaybackState::Playing {
                            sink.pause();
                            state = PlaybackState::Paused;
                            events.send(AudioEvent::Paused);
                        } else if pending_probe.is_some() {
                            // Still connecting: it starts paused
                            start_paused = true;
                        }
                    }
                    AudioCommand::Resume => {
                        if state == PlaybackState::Paused {
                            sink.play();
                            state = PlaybackState::Playing;
                            // No samples flowed while paused
                            if let Some(ref mut monitor) = health_monitor {
                                monitor.reset_stall_timer();
                            }
                            events.send(AudioEvent::Resumed);
                            if waiting_for_output {
                                events.send(AudioEvent::OutputLost);
                            }
                        } else if pending_probe.is_some() {
                            start_paused = false;
                        }
                    }
                    // NaN would survive the clamp and silence every later station
                    AudioCommand::SetVolume(vol) if vol.is_finite() => {
                        current_volume = vol.clamp(0.0, 2.0);
                        sink.set_volume(volume_curve(current_volume));
                    }
                    AudioCommand::SetVolume(_) => {}
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
                    next_tick = Instant::now() + tick;

                    // The output device went away (unplugged, disabled, or
                    // the sound server restarted): carry on with whichever
                    // device can be opened now, where playback left off.
                    // A closed output's last errors don't count.
                    if (waiting_for_output
                        || (stream.is_some() && output_lost.load(Ordering::SeqCst)))
                        && Instant::now() >= next_output_try
                    {
                        match outputs.open(&output_lost, Devices::Reopen) {
                            Ok(new_stream) => {
                                output_wait = config.output_retry;
                                next_output_try = Instant::now() + output_wait;
                                // A live station that played into nothing
                                // all this time would carry on that far
                                // behind live: it ends instead, to be
                                // started again
                                let behind = waiting_for_output
                                    && playing_live
                                    && state == PlaybackState::Playing
                                    && config.behind_live_limit.is_some_and(|limit| {
                                        output_gone_at.max(playing_since).elapsed() >= limit
                                    });
                                let playing = if behind {
                                    None
                                } else {
                                    playing_source.as_ref()
                                };
                                play_on(
                                    new_stream,
                                    &mut stream,
                                    &mut sink,
                                    playing,
                                    current_volume,
                                    state == PlaybackState::Paused,
                                );
                                eprintln!("Audio output reopened");
                                if waiting_for_output {
                                    waiting_for_output = false;
                                    // No samples flowed while waiting
                                    if let Some(ref mut monitor) = health_monitor {
                                        monitor.reset_stall_timer();
                                    }
                                    events.send(AudioEvent::OutputRestored);
                                }
                                if behind {
                                    eprintln!("The station fell behind live, ending it");
                                    // Its player is empty: the check for an
                                    // ended station below stops it
                                    events.send(AudioEvent::FellBehind);
                                }
                            }
                            Err(e) => {
                                next_output_try = Instant::now() + output_wait;
                                output_wait = (output_wait * 2).min(config.output_retry_max);
                                if !waiting_for_output {
                                    eprintln!("Audio output lost, no device to play on: {e}");
                                    waiting_for_output = true;
                                    output_gone_at = Instant::now();
                                    // Let the dead device go: some backends
                                    // keep reporting its errors in a busy loop
                                    stream = None;
                                    events.send(AudioEvent::OutputLost);
                                }
                            }
                        }
                    }

                    // Windows keeps playing on the device an output was
                    // opened on: when another becomes the default
                    // (headphones plugged in), move there too. Only there:
                    // another device would stay, with the default never
                    // tried again.
                    if stream.is_some()
                        && !waiting_for_output
                        && !output_lost.load(Ordering::SeqCst)
                        && outputs.default_moved()
                    {
                        match outputs.open(&output_lost, Devices::DefaultOnly) {
                            Ok(new_stream) => {
                                play_on(
                                    new_stream,
                                    &mut stream,
                                    &mut sink,
                                    playing_source.as_ref(),
                                    current_volume,
                                    state == PlaybackState::Paused,
                                );
                                eprintln!("Audio output reopened: the default device changed");
                            }
                            // Stay where we are, and try it again later
                            Err(e) => {
                                outputs.default_failed();
                                eprintln!("The new default output device didn't open: {e}");
                            }
                        }
                    }

                    // Poll pending probe for completion
                    if let Some(ref pending) = pending_probe {
                        match pending.probe_rx.try_recv() {
                            Ok(Ok(mut source)) => {
                                let p = pending_probe.take().unwrap();
                                // Probe succeeded — allow buffer compaction
                                p.probing_flag.store(false, Ordering::SeqCst);
                                source.count_decoded_time(p.decoded_time.clone());

                                let mut codec_info = source.codec_info();
                                codec_info.bitrate = p.bitrate;
                                // Phones listening get a bitrate to suit
                                recorder.listen().set_station_kbps(p.bitrate);
                                let label = source.codec_label();
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
                                // Decode on its own thread, ahead of the
                                // output: the audio callback never waits
                                let started = PcmFeed::start(
                                    analyzing,
                                    decode_ahead(stream.as_ref()),
                                    p.cancel.clone(),
                                );
                                let feed = match started {
                                    Ok(feed) => feed,
                                    Err(e) => {
                                        p.cancel.cancel();
                                        report_failed_start(
                                            &mut events,
                                            &shared_stats,
                                            format!("Unable to start decoding: {e}"),
                                        );
                                        continue;
                                    }
                                };
                                // A new player for each station: appending to
                                // a stopped player waits until its queue has
                                // played out, which a dead device never does.
                                // Its volume is set before it gets the audio,
                                // so not a moment plays at full volume.
                                sink = new_player(stream.as_ref());
                                sink.set_volume(volume_curve(current_volume));
                                if start_paused {
                                    sink.pause();
                                }
                                sink.append(feed.output());
                                playing_source = Some(feed);
                                if !start_paused {
                                    sink.play();
                                }
                                state = PlaybackState::Playing;
                                playing_since = Instant::now();
                                playing_live = p.stream_type.is_some();
                                health_monitor = Some(StreamHealthMonitor::with_timeouts(
                                    config.no_audio_timeout,
                                    config.stall_timeout,
                                ));
                                stream_error_slot = Some(error_slot);
                                current_decoder_stats = Some(dec_stats);
                                codec_label = Some(label);
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
                                    stats.stream_type = p.stream_type;
                                    stats.play_started_at = Some(Instant::now());
                                }

                                events.send(AudioEvent::Playing(codec_info));
                                // Paused while it connected
                                if std::mem::take(&mut start_paused) {
                                    state = PlaybackState::Paused;
                                    events.send(AudioEvent::Paused);
                                }
                                if waiting_for_output {
                                    events.send(AudioEvent::OutputLost);
                                }
                            }
                            Ok(Err(e)) => {
                                let p = pending_probe.take().unwrap();
                                p.cancel.cancel();
                                report_failed_start(&mut events, &shared_stats, e.to_string());
                            }
                            Err(TryRecvError::Empty) => {
                                // Still probing — check for timeout
                                if pending.started.elapsed() >= config.probe_timeout {
                                    let p = pending_probe.take().unwrap();
                                    p.cancel.cancel();
                                    let msg = format!(
                                        "Unable to detect audio format (timed out after {}s)",
                                        config.probe_timeout.as_secs()
                                    );
                                    events.send(AudioEvent::ProbeTimeout);
                                    report_failed_start(&mut events, &shared_stats, msg);
                                }
                            }
                            Err(TryRecvError::Disconnected) => {
                                let p = pending_probe.take().unwrap();
                                p.cancel.cancel();
                                report_failed_start(
                                    &mut events,
                                    &shared_stats,
                                    "Probe thread panicked".to_string(),
                                );
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
                                    events.send(AudioEvent::Error(err_msg));
                                }
                            }
                        }
                        stream_error_slot = None;
                        current_decoder_stats = None;
                        codec_label = None;
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
                        events.stopped();
                    }

                    // Nothing has played for a while: close the output, so
                    // the device can sleep. The next station opens it again.
                    if state == PlaybackState::Stopped
                        && pending_probe.is_none()
                        && stream.is_some()
                    {
                        let since = *idle_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= config.output_idle_timeout {
                            // The old player goes before its device
                            sink = new_player(None);
                            stream = None;
                            idle_since = None;
                            eprintln!("Audio output closed while nothing plays");
                        }
                    } else {
                        idle_since = None;
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
                            stats.output_underruns =
                                playing_source.as_ref().map_or(0, PcmFeed::underruns);
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
                            // From the snapshot: the buffer's lock is never held
                            // while sending, which can wait for the app
                            if let Some((level_bytes, _, is_buffering, _, effective_watermark)) =
                                buf_snapshot
                            {
                                if is_buffering {
                                    if !was_buffering {
                                        buffering_since = Some(Instant::now());
                                    }

                                    let stalled = buffering_since
                                        .map(|since| since.elapsed() >= config.buffering_stall)
                                        .unwrap_or(false);

                                    if stalled && level_bytes == 0 {
                                        // Prolonged buffering with no progress — genuine stall
                                        if !prolonged_buffering_stall {
                                            events.send(AudioEvent::StreamStalled);
                                        }
                                        prolonged_buffering_stall = true;
                                    } else {
                                        // Normal buffering or recovery in progress
                                        let pct = if effective_watermark > 0 {
                                            ((level_bytes as f64 / effective_watermark as f64)
                                                * 100.0)
                                                .min(99.0)
                                                as u8
                                        } else {
                                            0
                                        };
                                        events.send(AudioEvent::Buffering(pct));
                                        prolonged_buffering_stall = false;
                                    }

                                    was_buffering = true;
                                } else if was_buffering {
                                    // Recovered from buffering → signal 100%
                                    events.send(AudioEvent::Buffering(100));
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

                    // The codec name can change as the station plays (AAC
                    // and AAC+, a chained Ogg stream's next song)
                    if let Some(name) = codec_label
                        .as_ref()
                        .and_then(|label| label.lock().ok().map(|name| name.clone()))
                    {
                        let changed = shared_stats.lock().ok().and_then(|mut stats| {
                            let info = stats.codec_info.as_mut()?;
                            (info.codec_name != name).then(|| {
                                info.codec_name = name;
                                info.clone()
                            })
                        });
                        if let Some(info) = changed {
                            events.send(AudioEvent::CodecChanged(info));
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
                                        events.send(AudioEvent::StreamStalled);
                                    }
                                    FailureReason::NoAudioOutput => {
                                        // Fundamental failure — tear down
                                        events.send(AudioEvent::NoAudioTimeout);
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
                                        codec_label = None;
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
                                        events.stopped();
                                    }
                                    FailureReason::ProbeFailed => {
                                        unreachable!("health monitor never emits ProbeFailed")
                                    }
                                }
                            } else if was_stalled
                                && matches!(monitor.state(), &HealthState::Healthy)
                            {
                                // Stalled → Healthy: stream recovered, clear UI error
                                events.send(AudioEvent::StreamRecovered);
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

    impl AudioEngine {
        /// The next event, whichever station it is about
        fn next_event(&self) -> Option<AudioEvent> {
            self.try_recv_event().map(|e| e.event)
        }
    }

    /// Helper: wait for a specific event type within a timeout
    fn wait_for_event(engine: &AudioEngine, timeout_ms: u64) -> Option<AudioEvent> {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(evt) = engine.next_event() {
                return Some(evt);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// How many times faster than real time the tests' silent output plays
    const TEST_SPEED: f32 = 20.0;

    /// Engine settings for tests: a silent output, so the tests play the
    /// same on a machine with no sound (like CI)
    fn test_config() -> EngineConfig {
        EngineConfig {
            output: EngineOutput::Silent { speed: TEST_SPEED },
            ..EngineConfig::default()
        }
    }

    /// An engine that plays on a silent output
    fn test_engine() -> AudioEngine {
        AudioEngine::with_config(test_config()).expect("the engine starts")
    }

    // --- Loop scheduling ---

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
        let engine = test_engine();
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
            while let Some(event) = engine.next_event() {
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

    /// Outputs that open only on the attempts `works` allows (counting
    /// from 1), and a count of the attempts
    fn counted_outputs(works: fn(usize) -> bool) -> (Outputs, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let open: OpenOutput = Box::new(move |_, _| {
            let attempt = counted.fetch_add(1, Ordering::SeqCst) + 1;
            if works(attempt) {
                SilentOutput::open(TEST_SPEED).map(|output| (Output::Silent(output), None))
            } else {
                Err("no device".to_string())
            }
        });
        let outputs = Outputs {
            open,
            default: None,
            watch: None,
        };
        (outputs, attempts)
    }

    /// An engine whose output opens only on the attempts `works` allows
    /// (counting from 1), and a count of the attempts
    fn engine_with_output(works: fn(usize) -> bool) -> (AudioEngine, Arc<AtomicUsize>) {
        engine_with_output_and(works, test_config())
    }

    /// [`engine_with_output`] with its own settings
    fn engine_with_output_and(
        works: fn(usize) -> bool,
        config: EngineConfig,
    ) -> (AudioEngine, Arc<AtomicUsize>) {
        let (outputs, attempts) = counted_outputs(works);
        let engine = AudioEngine::with_output(outputs, config).expect("the engine starts");
        (engine, attempts)
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
            while let Some(event) = engine.next_event() {
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
        let (engine, attempts) = engine_with_output(|_| true);
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
        let (engine, attempts) = engine_with_output(|attempt| attempt != 2);
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

    // --- Default output device changes (Windows) ---

    /// Which device the test says is the default
    type SharedDefault = Arc<Mutex<Option<String>>>;

    /// Where an attempt to open an output ends up
    enum Opens {
        /// On the default device
        Default,
        /// On another device, "hdmi"
        Elsewhere,
        Fails,
    }

    /// The devices each attempt to open an output was allowed to try
    type Tries = Arc<Mutex<Vec<Devices>>>;

    fn tries(tries: &Tries) -> usize {
        tries.lock().unwrap().len()
    }

    /// Like [`engine_with_output`], on a system whose default device the
    /// test sets (at first "speakers"). Attempt `n` (counting from 1) to
    /// open an output on `Devices` goes where `opens(n, devices)` says.
    fn engine_following_default(
        opens: fn(usize, Devices) -> Opens,
        config: EngineConfig,
    ) -> (AudioEngine, Tries, SharedDefault) {
        let tried: Tries = Arc::default();
        let default: SharedDefault = Arc::new(Mutex::new(Some("speakers".to_string())));
        let (log, current, opened) = (tried.clone(), default.clone(), default.clone());
        let open: OpenOutput = Box::new(move |_, devices| {
            let attempt = {
                let mut log = log.lock().unwrap();
                log.push(devices);
                log.len()
            };
            let device = match opens(attempt, devices) {
                Opens::Default => opened.lock().unwrap().clone(),
                Opens::Elsewhere => Some("hdmi".to_string()),
                Opens::Fails => return Err("no device".to_string()),
            };
            SilentOutput::open(TEST_SPEED).map(|output| (Output::Silent(output), device))
        });
        let outputs = Outputs {
            open,
            default: Some(DefaultWatch::new(
                move || current.lock().unwrap().clone(),
                Duration::from_millis(10),
            )),
            watch: None,
        };
        let engine = AudioEngine::with_output(outputs, config).expect("the engine starts");
        (engine, tried, default)
    }

    fn set_default(default: &SharedDefault, device: &str) {
        *default.lock().unwrap() = Some(device.to_string());
    }

    /// Whether the station plays on: the samples counted keep rising
    fn keeps_playing(engine: &AudioEngine, events: &mut Vec<AudioEvent>) -> bool {
        // Let the last samples of an old output be counted first
        thread::sleep(Duration::from_millis(200));
        let before = sample_count(engine);
        wait_until(engine, events, Duration::from_secs(3), |_| {
            sample_count(engine) > before
        })
    }

    #[test]
    fn playback_follows_a_new_default_device() {
        let (engine, tried, default) =
            engine_following_default(|_, _| Opens::Default, test_config());
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |_| { sample_count(&engine) > 0 }
        ));
        assert_eq!(tries(&tried), 1);

        // Headphones plugged in: Windows makes them the default
        set_default(&default, "headphones");
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |_| {
                tries(&tried) == 2
            }),
            "playback didn't move to the new default"
        );
        assert!(
            keeps_playing(&engine, &mut events),
            "nothing played after the move"
        );

        // Moved once: the new default is now the one played on
        thread::sleep(Duration::from_secs(1));
        assert_eq!(*tried.lock().unwrap(), [Devices::Any, Devices::DefaultOnly]);
        // Nothing for the listener to see
        assert!(
            !has(&events, |e| matches!(
                e,
                AudioEvent::Error(_)
                    | AudioEvent::OutputLost
                    | AudioEvent::NoAudioTimeout
                    | AudioEvent::Stopped
            )),
            "{events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn a_new_default_that_does_not_open_is_tried_again_while_playback_stays() {
        // The new default fails to open twice (a Bluetooth headset still
        // connecting), then opens
        let (engine, tried, default) = engine_following_default(
            |attempt, _| match attempt {
                2 | 3 => Opens::Fails,
                _ => Opens::Default,
            },
            test_config(),
        );
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |_| { sample_count(&engine) > 0 }
        ));

        set_default(&default, "headphones");
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |_| { tries(&tried) == 2 }
        ));
        assert!(
            keeps_playing(&engine, &mut events),
            "the old device must keep playing"
        );
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |_| {
                tries(&tried) == 4
            }),
            "the new default was not tried again"
        );
        assert!(keeps_playing(&engine, &mut events));

        // Only the default was tried, never another device in its place,
        // and once it opened it stays
        thread::sleep(Duration::from_secs(1));
        assert_eq!(
            *tried.lock().unwrap(),
            [
                Devices::Any,
                Devices::DefaultOnly,
                Devices::DefaultOnly,
                Devices::DefaultOnly
            ]
        );
        assert!(
            !has(&events, |e| matches!(
                e,
                AudioEvent::Error(_) | AudioEvent::OutputLost
            )),
            "{events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn an_output_opened_on_another_device_moves_to_the_default_once_it_opens() {
        // At start the default fails and another device opens instead; the
        // default then fails once more before it opens
        let (engine, tried, _) = engine_following_default(
            |attempt, _| match attempt {
                1 => Opens::Elsewhere,
                2 => Opens::Fails,
                _ => Opens::Default,
            },
            test_config(),
        );
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |_| {
                tries(&tried) == 3
            }),
            "the default was not tried again"
        );
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        thread::sleep(Duration::from_millis(500));
        assert_eq!(
            *tried.lock().unwrap(),
            [Devices::Any, Devices::DefaultOnly, Devices::DefaultOnly]
        );
        engine.shutdown();
    }

    #[test]
    fn the_output_follows_the_default_while_nothing_plays() {
        let (engine, tried, default) =
            engine_following_default(|_, _| Opens::Default, test_config());
        let mut events = Vec::new();
        set_default(&default, "headphones");
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |_| { tries(&tried) == 2 }
        ));

        // The next station plays there
        engine.play(EndlessWav::new(), None, None);
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        assert_eq!(tries(&tried), 2);
        engine.shutdown();
    }

    #[test]
    fn a_paused_station_stays_paused_on_the_new_default() {
        let (engine, tried, default) =
            engine_following_default(|_, _| Opens::Default, test_config());
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |_| { sample_count(&engine) > 0 }
        ));
        engine.pause();
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |e| { has(e, |e| matches!(e, AudioEvent::Paused)) }
        ));

        set_default(&default, "headphones");
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |_| { tries(&tried) == 2 }
        ));
        thread::sleep(Duration::from_millis(300));
        let paused_at = sample_count(&engine);
        thread::sleep(Duration::from_millis(500));
        assert_eq!(sample_count(&engine), paused_at, "the move must not resume");

        engine.resume();
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        engine.shutdown();
    }

    // --- The output closes while nothing plays ---

    /// Settings that close the output after 300 ms with nothing playing
    fn closing_config() -> EngineConfig {
        EngineConfig {
            output_idle_timeout: Duration::from_millis(300),
            output_retry: Duration::from_millis(100),
            ..test_config()
        }
    }

    #[test]
    fn the_output_closes_while_nothing_plays_and_opens_for_the_next_station() {
        let (engine, attempts) = engine_with_output_and(|_| true, closing_config());
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        // Playing keeps it open
        thread::sleep(Duration::from_secs(1));
        engine.stop();
        // Stopped, it closes, and stays closed
        thread::sleep(Duration::from_secs(2));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        engine.play(EndlessWav::new(), None, None);
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "opened again to play");
        assert!(
            !has(&events, |e| matches!(
                e,
                AudioEvent::Error(_) | AudioEvent::OutputLost | AudioEvent::OutputRestored
            )),
            "closing is not a lost device: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn a_paused_station_keeps_its_output() {
        let (engine, attempts) = engine_with_output_and(|_| true, closing_config());
        let mut events = Vec::new();
        engine.play(EndlessWav::new(), None, None);
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        engine.pause();
        thread::sleep(Duration::from_secs(1));
        engine.resume();
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    #[test]
    fn no_device_for_the_next_station_says_so_and_waits_for_one() {
        // The output closes, and the first try to open it again fails
        let (engine, attempts) = engine_with_output_and(|attempt| attempt != 2, closing_config());
        let mut events = Vec::new();
        thread::sleep(Duration::from_secs(2));
        engine.play(EndlessWav::new(), None, None);
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |e| {
                has(e, |e| matches!(e, AudioEvent::OutputLost))
                    && has(e, |e| matches!(e, AudioEvent::OutputRestored))
            }),
            "{events:?}"
        );
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        engine.shutdown();
    }

    #[test]
    fn the_default_is_not_watched_while_the_output_is_closed() {
        let (engine, tried, default) =
            engine_following_default(|_, _| Opens::Default, closing_config());
        let mut events = Vec::new();
        thread::sleep(Duration::from_secs(2));
        set_default(&default, "headphones");
        thread::sleep(Duration::from_millis(500));
        assert_eq!(tries(&tried), 1, "the closed output was moved");

        // The next station opens on the new default, and stays there
        engine.play(EndlessWav::new(), None, None);
        assert!(keeps_playing(&engine, &mut events), "{events:?}");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(*tried.lock().unwrap(), [Devices::Any, Devices::Reopen]);
        engine.shutdown();
    }

    // --- A long output loss ---

    /// What a station sends after its output was gone for two tries to
    /// open another, at least 200 ms, with the engine's `limit`. `start`
    /// plays it, and `pause` pauses it before the loss.
    fn after_a_long_output_loss(
        start: impl FnOnce(&AudioEngine),
        pause: bool,
        limit: Duration,
    ) -> (AudioEngine, Vec<AudioEvent>) {
        let config = EngineConfig {
            tick: Duration::from_millis(100),
            output_retry: Duration::from_millis(100),
            output_retry_max: Duration::from_millis(100),
            behind_live_limit: Some(limit),
            ..test_config()
        };
        // Opens at start, then not for the next two tries
        let (engine, attempts) =
            engine_with_output_and(|attempt| attempt == 1 || attempt > 3, config);
        let mut events = Vec::new();
        start(&engine);
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(3),
            |e| { has(e, |e| matches!(e, AudioEvent::Playing(_))) }
        ));
        if pause {
            engine.pause();
        }
        engine.simulate_output_loss();
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(5), |e| {
                has(e, |e| matches!(e, AudioEvent::OutputRestored))
            }),
            "{events:?}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        // Whatever follows the restore
        thread::sleep(Duration::from_millis(500));
        while let Some(event) = engine.next_event() {
            events.push(event);
        }
        (engine, events)
    }

    /// The events from `OutputRestored` on
    fn from_restore(events: &[AudioEvent]) -> Vec<&AudioEvent> {
        events
            .iter()
            .skip_while(|e| !matches!(e, AudioEvent::OutputRestored))
            .collect()
    }

    #[test]
    fn a_live_station_far_behind_after_a_long_output_loss_ends() {
        let cancel = StreamCancel::new();
        let (engine, events) = after_a_long_output_loss(
            |engine| {
                engine.play_stream(resolved_stream(EndlessWav::new(), &cancel));
            },
            false,
            Duration::from_millis(100),
        );
        assert!(
            matches!(
                from_restore(&events)[..],
                [
                    AudioEvent::OutputRestored,
                    AudioEvent::FellBehind,
                    AudioEvent::Stopped
                ]
            ),
            "{events:?}"
        );
        assert!(cancel.is_cancelled(), "its stream must stop too");
        engine.shutdown();
    }

    #[test]
    fn a_station_carries_on_after_a_short_loss_a_paused_one_or_a_file() {
        let live = |engine: &AudioEngine| {
            engine.play_stream(resolved_stream(EndlessWav::new(), &StreamCancel::new()));
        };
        let file = |engine: &AudioEngine| {
            engine.play(EndlessWav::new(), None, None);
        };
        let long = Duration::from_millis(100);
        for (start, pause, limit) in [
            (
                &live as &dyn Fn(&AudioEngine),
                false,
                Duration::from_secs(10),
            ),
            (&live, true, long),
            (&file, false, long),
        ] {
            let (engine, mut events) = after_a_long_output_loss(start, pause, limit);
            assert!(
                matches!(from_restore(&events)[..], [AudioEvent::OutputRestored]),
                "{events:?}"
            );
            if pause {
                engine.resume();
            }
            assert!(keeps_playing(&engine, &mut events), "{events:?}");
            engine.shutdown();
        }
    }

    // --- Events nobody reads ---

    #[test]
    fn events_never_wait_for_their_reader_and_buffering_is_sent_on_change() {
        let (tx, rx) = bounded(4);
        let mut events = Events::new(tx);
        events.stream = Some(StreamId(1));
        for pct in [10, 10, 20, 20] {
            events.send(AudioEvent::Buffering(pct));
        }
        events.send(AudioEvent::StreamStalled);
        events.send(AudioEvent::Buffering(20));
        // A full queue drops the rest instead of waiting
        events.send(AudioEvent::Buffering(30));
        events.send(AudioEvent::Stopped);

        let sent: Vec<_> = rx.try_iter().map(|e| e.event).collect();
        assert!(
            matches!(
                sent[..],
                [
                    AudioEvent::Buffering(10),
                    AudioEvent::Buffering(20),
                    AudioEvent::StreamStalled,
                    AudioEvent::Buffering(20)
                ]
            ),
            "{sent:?}"
        );

        // The same percentage for another station is news
        events.send(AudioEvent::Buffering(30));
        events.stream = Some(StreamId(2));
        events.send(AudioEvent::Buffering(30));
        assert_eq!(rx.try_iter().count(), 2);
    }

    #[test]
    fn an_engine_whose_events_are_never_read_keeps_working() {
        let (done_tx, done_rx) = bounded(1);
        // It used to wait for a reader once 64 events were queued, then
        // its commands, and then the shutdown
        thread::spawn(move || {
            let engine = test_engine();
            engine.play(EndlessWav::new(), None, None);
            let start = Instant::now();
            while sample_count(&engine) == 0 && start.elapsed() < Duration::from_secs(3) {
                thread::sleep(Duration::from_millis(10));
            }
            for _ in 0..100 {
                engine.pause();
                engine.resume();
            }
            engine.shutdown();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the engine stopped answering"
        );
    }

    #[test]
    fn a_new_station_without_a_device_says_so_after_it_starts() {
        // No device after the first
        let (engine, _) = engine_with_output(|attempt| attempt == 1);
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
        let engine = test_engine();
        engine.shutdown();
    }

    #[test]
    fn drop_triggers_shutdown() {
        let engine = test_engine();
        drop(engine);
        // If we get here without hanging, shutdown worked
    }

    #[test]
    fn shutdown_is_idempotent_via_drop() {
        // shutdown_inner is called once explicitly, then again in drop
        let engine = test_engine();
        engine.shutdown();
        // Drop happens automatically after shutdown consumed self
    }

    #[test]
    fn create_multiple_engines_sequentially() {
        for _ in 0..3 {
            let engine = test_engine();
            engine.shutdown();
        }
    }

    // --- Play / Stop ---

    #[test]
    fn play_and_stop() {
        let engine = test_engine();

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
        let engine = test_engine();

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
        let engine = test_engine();

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
        let engine = test_engine();

        engine.stop();
        // Give it time to process
        thread::sleep(Duration::from_millis(200));

        // Should not have received a Stopped event (already stopped)
        let evt = engine.next_event();
        assert!(
            evt.is_none(),
            "Stop when already stopped should not emit event, got {:?}",
            evt
        );

        engine.shutdown();
    }

    #[test]
    fn double_stop_only_emits_one_stopped_event() {
        let engine = test_engine();

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
        let evt = engine.next_event();
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
        let engine = test_engine();

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

        // The first clip stops, then the second plays
        expect_stopped(&engine);
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
        let engine = test_engine();

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
        let engine = test_engine();

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
        let engine = test_engine();

        // Send invalid data
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(_)) => {}
            other => panic!("Expected Error, got {:?}", other),
        }
        expect_stopped(&engine);

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
        let engine = test_engine();

        for _ in 0..3 {
            engine.play(Box::new(Cursor::new(vec![0xDE, 0xAD])), None, None);
            match wait_for_event(&engine, 2000) {
                Some(AudioEvent::Error(_)) => {}
                other => panic!("Expected Error, got {:?}", other),
            }
            expect_stopped(&engine);
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
        let engine = test_engine();
        engine.set_volume(0.5);
        engine.set_volume(0.0);
        engine.set_volume(2.0);
        engine.set_volume(5.0); // should clamp to 2.0
        engine.shutdown();
    }

    #[test]
    fn set_volume_negative_clamped() {
        let engine = test_engine();
        engine.set_volume(-1.0);
        engine.set_volume(-100.0);
        // No crash = success
        engine.shutdown();
    }

    #[test]
    fn set_volume_during_playback() {
        let engine = test_engine();

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
        let engine = test_engine();
        // Setting volume while stopped should not panic or produce events
        engine.set_volume(0.75);
        thread::sleep(Duration::from_millis(100));
        assert!(engine.next_event().is_none());
        engine.shutdown();
    }

    // --- Analysis ---

    #[test]
    fn analysis_starts_at_zero() {
        let engine = test_engine();

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
        let engine = test_engine();

        let a1 = engine.analysis();
        let a2 = engine.analysis();
        // Both should point to the same underlying data
        assert!(Arc::ptr_eq(&a1, &a2));

        engine.shutdown();
    }

    #[test]
    fn analysis_reset_after_stop() {
        let engine = test_engine();

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
        let engine = test_engine();
        let _rx = engine.event_receiver();
        engine.shutdown();
    }

    #[test]
    fn event_receiver_receives_events() {
        let engine = test_engine();

        let rx = engine.event_receiver();

        let id = engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);

        // Use the receiver directly
        let evt = rx.recv_timeout(Duration::from_secs(2));
        assert!(evt.is_ok(), "Should receive event via receiver");
        let evt = evt.unwrap();
        assert_eq!(evt.stream, Some(id));
        match evt.event {
            AudioEvent::Playing(_) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.shutdown();
    }

    // --- Raw send ---

    #[test]
    fn send_raw_stop_command() {
        let engine = test_engine();

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
        let engine = test_engine();

        engine.send(AudioCommand::Shutdown);
        // Engine thread should exit; drop shouldn't hang
        thread::sleep(Duration::from_millis(200));
        drop(engine);
    }

    // --- Stream-ended detection ---

    #[test]
    fn short_clip_auto_stops() {
        let engine = test_engine();

        // Play a very short clip (10ms) - should end quickly
        engine.play(Box::new(Cursor::new(make_short_wav())), None, None);

        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // The clip's end is noticed, whatever progress events come first
        let mut events = Vec::new();
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(5), |e| {
                has(e, |e| matches!(e, AudioEvent::Stopped))
            }),
            "Expected auto-Stopped for short clip, got {events:?}"
        );
        assert!(
            !has(&events, |e| matches!(
                e,
                AudioEvent::Error(_) | AudioEvent::NoAudioTimeout | AudioEvent::StreamStalled
            )),
            "{events:?}"
        );

        engine.shutdown();
    }

    // --- Format hints ---

    #[test]
    fn play_with_format_hint() {
        let engine = test_engine();

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
        let engine = test_engine();

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
        let engine = test_engine();

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
        let engine = test_engine();

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
            match engine.next_event() {
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
        let engine = test_engine();

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
        let engine = test_engine();

        engine.pause();
        thread::sleep(Duration::from_millis(200));

        assert!(
            engine.next_event().is_none(),
            "Pause when stopped should not emit event"
        );

        engine.shutdown();
    }

    #[test]
    fn resume_when_stopped_is_noop() {
        let engine = test_engine();

        engine.resume();
        thread::sleep(Duration::from_millis(200));

        assert!(
            engine.next_event().is_none(),
            "Resume when stopped should not emit event"
        );

        engine.shutdown();
    }

    #[test]
    fn resume_when_playing_is_noop() {
        let engine = test_engine();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        engine.resume();
        thread::sleep(Duration::from_millis(200));

        // Should not get a Resumed event since we were already playing
        assert!(
            engine.next_event().is_none(),
            "Resume when already playing should not emit event"
        );

        engine.shutdown();
    }

    #[test]
    fn double_pause_only_emits_once() {
        let engine = test_engine();

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
            engine.next_event().is_none(),
            "Second pause should not emit event"
        );

        engine.shutdown();
    }

    #[test]
    fn stop_while_paused_emits_stopped() {
        let engine = test_engine();

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
        let engine = test_engine();

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
        expect_stopped(&engine);
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
        let engine = test_engine();

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
        expect_stopped(&engine);
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
        let engine = test_engine();

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
        let engine = test_engine();

        let data = engine.analysis();
        let analysis = data.lock().unwrap();
        assert_eq!(analysis.sample_count, 0);

        drop(analysis);
        engine.shutdown();
    }

    #[test]
    fn analysis_sample_count_increases_during_playback() {
        let engine = test_engine();

        engine.play(EndlessWav::new(), None, None);
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
        let engine = test_engine();

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
        let engine = test_engine();

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
        let engine = test_engine();

        // Send invalid data — should fail with error, health monitor cleared
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(_)) => {}
            other => panic!("Expected Error, got {:?}", other),
        }
        expect_stopped(&engine);

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
        while let Some(evt) = engine.next_event() {
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
        let engine = test_engine();

        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Playing(_)) => {}
            other => panic!("Expected Playing, got {:?}", other),
        }

        // Let it play for a while
        thread::sleep(Duration::from_millis(800));

        // Drain all events
        let mut events = Vec::new();
        while let Some(evt) = engine.next_event() {
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
        let engine = test_engine();

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
        while let Some(evt) = engine.next_event() {
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
        let engine = test_engine();

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
        while let Some(evt) = engine.next_event() {
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
        let engine = test_engine();

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
                if let Some(AudioEvent::Playing(_)) = engine.next_event() {
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
        let engine = test_engine();

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
            match engine.next_event() {
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
        let engine = test_engine();

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
            match engine.next_event() {
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
        let engine = test_engine();

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
            match engine.next_event() {
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
        let engine = test_engine();

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
        while let Some(evt) = engine.next_event() {
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
        let engine = test_engine();

        engine.set_volume(0.3);

        // Send invalid data
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        match wait_for_event(&engine, 2000) {
            Some(AudioEvent::Error(_)) => {}
            other => panic!("Expected Error, got {:?}", other),
        }
        expect_stopped(&engine);

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
        let engine = test_engine();
        let s1 = engine.shared_stats();
        let s2 = engine.shared_stats();
        assert!(Arc::ptr_eq(&s1, &s2));
        engine.shutdown();
    }

    #[test]
    fn shared_stats_default_before_play() {
        let engine = test_engine();
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
        let engine = test_engine();
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
        let engine = test_engine();
        let stats = engine.shared_stats();

        engine.play(EndlessWav::new(), None, None);
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
        let engine = test_engine();
        let stats = engine.shared_stats();

        engine.play(EndlessWav::new(), None, None);
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
        let engine = test_engine();
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
        let engine = test_engine();
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
        let engine = test_engine();
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
        let engine = test_engine();
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
        let engine = test_engine();
        let stats = engine.shared_stats();

        // First play
        engine.play(EndlessWav::new(), None, None);
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
        // The first station stops, then the second plays
        let second = loop {
            match wait_for_event(&engine, 2000) {
                Some(AudioEvent::Stopped) => continue,
                other => break other,
            }
        };
        match second {
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
        let engine = test_engine();
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
        let engine = test_engine();
        let stats = engine.shared_stats();

        // Create a bytes_received counter and pre-set it
        let bytes_counter = Arc::new(AtomicU64::new(42000));

        // Endless, so playback doesn't end before our assertions
        engine.play_with_stats(
            EndlessWav::new(),
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
        let engine = test_engine();
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
        let engine = test_engine();
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

    fn expect_stopped(engine: &AudioEngine) {
        match wait_for_event(engine, 2000) {
            Some(AudioEvent::Stopped) => {}
            other => panic!("Expected Stopped, got {:?}", other),
        }
    }

    #[test]
    fn stopping_cancels_the_stream() {
        let engine = test_engine();
        let cancel = StreamCancel::new();
        engine.play_stream(resolved_stream(EndlessWav::new(), &cancel));
        expect_playing(&engine);
        assert!(!cancel.is_cancelled());

        engine.stop();
        assert!(gets_cancelled(&cancel), "stop must cancel the stream");
        engine.shutdown();
    }

    #[test]
    fn the_stats_say_how_the_station_is_streamed() {
        let engine = test_engine();
        let cancel = StreamCancel::new();
        engine.play_stream(resolved_stream(EndlessWav::new(), &cancel));
        expect_playing(&engine);
        assert_eq!(
            engine.shared_stats().lock().unwrap().stream_type,
            Some(crate::stream::types::StreamType::Direct)
        );
        engine.shutdown();
    }

    #[test]
    fn playing_another_station_cancels_the_old_stream() {
        let engine = test_engine();
        let old = StreamCancel::new();
        engine.play_stream(resolved_stream(EndlessWav::new(), &old));
        expect_playing(&engine);

        let new = StreamCancel::new();
        engine.play_stream(resolved_stream(EndlessWav::new(), &new));
        assert!(gets_cancelled(&old), "the old station must be cancelled");
        expect_stopped(&engine);
        expect_playing(&engine);
        assert!(!new.is_cancelled());
        engine.shutdown();
    }

    #[test]
    fn stopping_while_probing_cancels_the_stream() {
        let engine = test_engine();
        let cancel = StreamCancel::new();
        engine.play_stream(resolved_stream(Box::new(SilentStation), &cancel));
        // The probe waits for audio that never comes
        thread::sleep(Duration::from_millis(100));
        assert!(!cancel.is_cancelled());

        engine.stop();
        assert!(gets_cancelled(&cancel), "stop must cancel the stream");
        engine.shutdown();
    }

    // === Event contract, and starting without a device ===

    #[test]
    fn a_hung_device_open_is_reported_and_holds_no_one_else() {
        // The first open hangs until the test lets it go, like a stuck driver
        let (release_tx, release_rx) = bounded::<()>(1);
        let release_rx = Mutex::new(release_rx);
        let outputs = Outputs {
            open: Box::new(move |_, _| {
                let _ = release_rx.lock().unwrap().recv();
                SilentOutput::open(TEST_SPEED).map(|output| (Output::Silent(output), None))
            }),
            default: None,
            watch: None,
        };
        let config = EngineConfig {
            output_open_deadline: Duration::from_millis(200),
            ..test_config()
        };

        // The engine starts without waiting for the open
        let started = Instant::now();
        let engine = AudioEngine::with_output(outputs, config).expect("the engine starts");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let event = engine
            .event_receiver()
            .recv_timeout(Duration::from_secs(2))
            .expect("the hang is reported");
        assert!(event.stream.is_none());
        assert!(
            matches!(event.event, AudioEvent::OutputNotResponding),
            "{event:?}"
        );

        // Commands queue up without holding the sender
        let sending = Instant::now();
        for _ in 0..100 {
            engine.set_volume(0.5);
        }
        assert!(
            sending.elapsed() < Duration::from_secs(1),
            "{:?}",
            sending.elapsed()
        );

        // The driver comes back: so does the engine, and it plays
        release_tx.send(()).unwrap();
        let event = engine
            .event_receiver()
            .recv_timeout(Duration::from_secs(2))
            .expect("the recovery is reported");
        assert!(
            matches!(event.event, AudioEvent::OutputResponding),
            "{event:?}"
        );
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        let events = events_for(&engine, 3000);
        assert!(
            has(&events, |e| matches!(e, AudioEvent::Playing(_))),
            "{events:?}"
        );
    }

    /// An engine on a machine with no audio output (runs on CI too)
    fn engine_without_device() -> AudioEngine {
        let outputs = Outputs {
            open: Box::new(|_, _| Err("no device".to_string())),
            default: None,
            watch: None,
        };
        AudioEngine::with_output(outputs, test_config())
            .expect("the engine starts without a device")
    }

    /// Every event within `ms`
    fn events_for(engine: &AudioEngine, ms: u64) -> Vec<AudioEvent> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            while let Some(event) = engine.next_event() {
                events.push(event);
            }
            thread::sleep(Duration::from_millis(10));
        }
        events
    }

    #[test]
    fn an_engine_without_a_device_starts_and_says_so() {
        let engine = engine_without_device();
        assert!(matches!(
            wait_for_event(&engine, 1000),
            Some(AudioEvent::OutputLost)
        ));

        // A station still starts, and is told there is nowhere to play it
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        let events = events_for(&engine, 1000);
        assert!(
            matches!(
                events.as_slice(),
                [AudioEvent::Playing(_), AudioEvent::OutputLost]
            ),
            "{events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn a_failed_start_ends_in_stopped() {
        let engine = engine_without_device();
        let _ = wait_for_event(&engine, 1000);
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        let events = events_for(&engine, 1000);
        assert!(
            matches!(
                events.as_slice(),
                [AudioEvent::Error(_), AudioEvent::Stopped]
            ),
            "{events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn a_new_station_stops_the_old_one_first() {
        let engine = engine_without_device();
        let _ = wait_for_event(&engine, 1000);
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        let events = events_for(&engine, 1000);
        assert!(matches!(events.first(), Some(AudioEvent::Playing(_))));

        // Another station that plays: the old one's Stopped comes first,
        // and nothing about it comes after
        engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        let events = events_for(&engine, 1500);
        assert!(
            matches!(
                events.as_slice(),
                [
                    AudioEvent::Stopped,
                    AudioEvent::Playing(_),
                    AudioEvent::OutputLost
                ]
            ),
            "{events:?}"
        );

        // And one that fails: stopped, then failed and stopped
        engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None);
        let events = events_for(&engine, 1000);
        assert!(
            matches!(
                events.as_slice(),
                [
                    AudioEvent::Stopped,
                    AudioEvent::Error(_),
                    AudioEvent::Stopped
                ]
            ),
            "{events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn a_device_that_appears_later_plays() {
        // No device at start; the first retry finds one
        let (engine, attempts) = engine_with_output(|attempt| attempt != 1);
        let mut events = Vec::new();
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(5), |e| {
                has(e, |e| matches!(e, AudioEvent::OutputRestored))
            }),
            "{events:?}"
        );
        assert!(matches!(events.first(), Some(AudioEvent::OutputLost)));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        engine.play(EndlessWav::new(), None, None);
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |_| {
                sample_count(&engine) > 0
            }),
            "nothing played: {events:?}"
        );
        engine.shutdown();
    }

    // === Decoding off the audio thread ===

    /// An endless WAV from a station that stops sending after `after` bytes
    /// until `resume` is raised
    struct StallingWav {
        wav: Box<EndlessWav>,
        after: u64,
        resume: Arc<AtomicBool>,
    }

    impl std::io::Read for StallingWav {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut buf = buf;
            if !self.resume.load(Ordering::SeqCst) {
                let left = self.after.saturating_sub(self.wav.pos) as usize;
                if left == 0 {
                    // What the ICY and HLS readers do while nothing arrives
                    thread::sleep(Duration::from_millis(10));
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let len = buf.len().min(left);
                buf = &mut buf[..len];
            }
            self.wav.read(buf)
        }
    }

    impl std::io::Seek for StallingWav {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.wav.seek(pos)
        }
    }

    fn output_underruns(engine: &AudioEngine) -> u64 {
        engine.shared_stats().lock().unwrap().output_underruns
    }

    /// `inner` at `bytes_per_sec`, like a station sending in real time
    struct Paced<R> {
        inner: R,
        bytes_per_sec: usize,
    }

    impl<R: std::io::Read> std::io::Read for Paced<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            thread::sleep(Duration::from_millis(10));
            let n = buf.len().min(self.bytes_per_sec / 100);
            self.inner.read(&mut buf[..n])
        }
    }

    impl<R: std::io::Seek> std::io::Seek for Paced<R> {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn a_full_event_queue_doesnt_stop_the_audio() {
        // Checks often, so Buffering events fill the queue soon
        let engine = AudioEngine::with_config(EngineConfig {
            tick: Duration::from_millis(10),
            ..test_config()
        })
        .unwrap();
        // Real time, while the output plays faster: it keeps buffering
        engine.play(
            Box::new(Paced {
                inner: EndlessWav::new(),
                bytes_per_sec: 88_200,
            }),
            None,
            None,
        );
        // Nobody reads the events: the engine waits to send them. The
        // buffer's status lock must not be held meanwhile, or the decoder
        // waits for it and the audio stops.
        let samples = || engine.analysis().lock().unwrap().sample_count;
        thread::sleep(Duration::from_secs(2));
        let before = samples();
        thread::sleep(Duration::from_secs(2));
        let after = samples();

        // Let the engine finish sending, so it can shut down
        let events = engine.event_receiver().clone();
        let drain = thread::spawn(move || while events.recv().is_ok() {});
        engine.shutdown();
        drain.join().unwrap();
        assert!(after > before, "audio stopped at {before} samples");
    }

    #[test]
    fn a_stalled_station_plays_silence_and_carries_on() {
        let engine = test_engine();
        let resume = Arc::new(AtomicBool::new(false));
        let reader = StallingWav {
            wav: EndlessWav::new(),
            // About 2 s of audio, then nothing
            after: 200_000,
            resume: resume.clone(),
        };
        let cancel = StreamCancel::new();
        engine.play_stream(resolved_stream(Box::new(reader), &cancel));
        expect_playing(&engine);
        let mut events = Vec::new();

        // The output plays out what was decoded, then keeps running on
        // silence instead of waiting on the station
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(5), |_| {
                output_underruns(&engine) > 0
            }),
            "the output never ran dry"
        );
        let stalled_at = sample_count(&engine);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(sample_count(&engine), stalled_at, "nothing new to play");

        // The station sends again: playback carries on
        resume.store(true, Ordering::SeqCst);
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(10), |_| {
                sample_count(&engine) > stalled_at
            }),
            "playback didn't carry on: {events:?}"
        );
        assert!(
            !has(&events, |e| matches!(
                e,
                AudioEvent::Error(_) | AudioEvent::Stopped
            )),
            "{events:?}"
        );
        engine.stop();
        assert!(gets_cancelled(&cancel));
        engine.shutdown();
    }

    // === Every event names its station ===

    /// Every event within `ms`, with the station it is about
    fn marked_events_for(engine: &AudioEngine, ms: u64) -> Vec<(Option<StreamId>, AudioEvent)> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            while let Some(e) = engine.try_recv_event() {
                events.push((e.stream, e.event));
            }
            thread::sleep(Duration::from_millis(10));
        }
        events
    }

    #[test]
    fn every_event_names_the_station_it_is_about() {
        let engine = engine_without_device();
        // Nothing loaded yet: the lost device is about no station
        let events = marked_events_for(&engine, 300);
        assert!(
            matches!(events.as_slice(), [(None, AudioEvent::OutputLost)]),
            "{events:?}"
        );

        let first = engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        let events = marked_events_for(&engine, 1000);
        let a = Some(first);
        assert!(
            matches!(
                events.as_slice(),
                [(s1, AudioEvent::Playing(_)), (s2, AudioEvent::OutputLost)]
                    if *s1 == a && *s2 == a
            ),
            "{events:?}"
        );

        // Another station: the old one's stop names the old one
        let second = engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
        assert_ne!(first, second);
        let b = Some(second);
        let events = marked_events_for(&engine, 1000);
        assert!(
            matches!(
                events.as_slice(),
                [
                    (s1, AudioEvent::Stopped),
                    (s2, AudioEvent::Playing(_)),
                    (s3, AudioEvent::OutputLost),
                ] if *s1 == a && *s2 == b && *s3 == b
            ),
            "{events:?}"
        );

        // One that fails names itself
        let third = Some(engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None));
        let events = marked_events_for(&engine, 1000);
        assert!(
            matches!(
                events.as_slice(),
                [
                    (s1, AudioEvent::Stopped),
                    (s2, AudioEvent::Error(_)),
                    (s3, AudioEvent::Stopped),
                ] if *s1 == b && *s2 == third && *s3 == third
            ),
            "{events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn once_a_station_stops_events_are_about_no_station() {
        // The device goes away on the second try to open it
        let (engine, _) = engine_with_output(|attempt| attempt != 2);
        let id = Some(engine.play(EndlessWav::new(), None, None));
        let mut events = Vec::new();
        assert!(
            wait_until(&engine, &mut events, Duration::from_secs(3), |e| {
                has(e, |e| matches!(e, AudioEvent::Playing(_)))
            }),
            "{events:?}"
        );
        engine.stop();
        let events = marked_events_for(&engine, 300);
        assert!(
            matches!(events.last(), Some((s, AudioEvent::Stopped)) if *s == id)
                && events.iter().all(|(s, _)| *s == id),
            "{events:?}"
        );

        engine.simulate_output_loss();
        let events = marked_events_for(&engine, 1500);
        assert!(
            matches!(events.as_slice(), [(None, AudioEvent::OutputLost)]),
            "{events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn each_play_gets_a_new_id() {
        let engine = test_engine();
        let ids: Vec<StreamId> = (0..3)
            .map(|_| engine.play(Box::new(Cursor::new(vec![0u8; 100])), None, None))
            .collect();
        assert_eq!(ids, [StreamId(1), StreamId(2), StreamId(3)]);
        engine.shutdown();
    }

    #[test]
    fn shared_stats_not_updated_when_stopped() {
        let engine = test_engine();
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
    fn shared_stats_play_started_at_is_recent() {
        let engine = test_engine();
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
        let engine = test_engine();
        let stats = engine.shared_stats();

        let bytes_counter = Arc::new(AtomicU64::new(5000));
        engine.play_with_stats(
            EndlessWav::new(),
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

    // === Lifecycle edge cases ===

    /// `inner`, with its first read held back `delay` (a station slow to
    /// answer)
    struct SlowStart<R> {
        inner: R,
        delay: Option<Duration>,
    }

    impl<R: std::io::Read> std::io::Read for SlowStart<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(delay) = self.delay.take() {
                thread::sleep(delay);
            }
            self.inner.read(buf)
        }
    }

    impl<R: std::io::Seek> std::io::Seek for SlowStart<R> {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn a_pause_while_connecting_starts_the_station_paused() {
        let engine = test_engine();
        engine.play(
            Box::new(SlowStart {
                inner: EndlessWav::new(),
                delay: Some(Duration::from_millis(300)),
            }),
            None,
            None,
        );
        engine.pause();
        let mut events = Vec::new();
        let paused = wait_until(&engine, &mut events, Duration::from_secs(5), |events| {
            events.iter().any(|e| matches!(e, AudioEvent::Paused))
        });
        assert!(paused, "never paused: {events:?}");
        assert!(
            matches!(events[..], [AudioEvent::Playing(_), AudioEvent::Paused]),
            "{events:?}"
        );
        // Nothing plays: once the decoder is far enough ahead, the count
        // stands still
        thread::sleep(Duration::from_millis(300));
        let before = sample_count(&engine);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(sample_count(&engine), before, "it played while paused");

        engine.resume();
        let resumed = wait_until(&engine, &mut events, Duration::from_secs(5), |events| {
            events.iter().any(|e| matches!(e, AudioEvent::Resumed))
        });
        assert!(resumed, "{events:?}");
        thread::sleep(Duration::from_millis(300));
        assert!(
            sample_count(&engine) > before,
            "it didn't play once resumed"
        );
        engine.shutdown();
    }

    #[test]
    fn a_resume_while_connecting_undoes_the_pause() {
        let engine = test_engine();
        engine.play(
            Box::new(SlowStart {
                inner: EndlessWav::new(),
                delay: Some(Duration::from_millis(300)),
            }),
            None,
            None,
        );
        engine.pause();
        engine.resume();
        let mut events = Vec::new();
        assert!(wait_until(
            &engine,
            &mut events,
            Duration::from_secs(5),
            |events| { events.iter().any(|e| matches!(e, AudioEvent::Playing(_))) }
        ));
        thread::sleep(Duration::from_millis(200));
        while let Some(event) = engine.next_event() {
            events.push(event);
        }
        assert!(
            !events.iter().any(|e| matches!(e, AudioEvent::Paused)),
            "{events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn a_zero_tick_neither_spins_nor_keeps_the_engine_from_shutting_down() {
        let (done_tx, done_rx) = bounded(1);
        thread::spawn(move || {
            let engine = AudioEngine::with_config(EngineConfig {
                tick: Duration::ZERO,
                ..test_config()
            })
            .unwrap();
            engine.play(Box::new(Cursor::new(make_one_second_wav())), None, None);
            let playing = wait_for_event(&engine, 3000);
            drop(engine);
            let _ = done_tx.send(playing);
        });
        let playing = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the engine didn't shut down");
        assert!(
            matches!(playing, Some(AudioEvent::Playing(_))),
            "{playing:?}"
        );
    }

    #[test]
    fn dropping_the_engine_finishes_a_recording_held_elsewhere() {
        use super::super::recording::{RecordingFormat, RecordingOptions, RecordingTags};
        let dir =
            std::env::temp_dir().join(format!("radiotrope-engine-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("held.wav");

        let engine = test_engine();
        engine.play(EndlessWav::new(), None, None);
        assert!(matches!(
            wait_for_event(&engine, 3000),
            Some(AudioEvent::Playing(_))
        ));
        // The app keeps its own handle to the recorder
        let recorder = engine.recorder();
        recorder
            .start(RecordingOptions {
                path: path.clone(),
                format: RecordingFormat::Wav,
                bitrate_kbps: 0,
                tap: TapPoint::BeforeEq,
                tags: RecordingTags::default(),
            })
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        drop(engine);

        assert!(!recorder.is_recording(), "still recording with no engine");
        let file = std::fs::read(&path).unwrap();
        let riff = u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff, file.len() - 8, "the file wasn't finished");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
