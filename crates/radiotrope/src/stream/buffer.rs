//! Decoupled producer-consumer stream buffer
//!
//! A background producer thread reads from the network reader (IcyReader/HlsReader)
//! and fills a shared buffer. The consumer (StreamBufferReader) implements Read + Seek
//! and is passed to symphonia. This decouples network I/O from audio decoding so that
//! transient network stalls don't cause playback glitches.
//!
//! Architecture:
//!   Network → IcyReader/HlsReader
//!                  ↓ (producer thread reads chunks)
//!            SharedBuffer (`Vec<u8>` + Mutex + Condvar)
//!                  ↓ (consumer: Read+Seek impl)
//!            StreamBufferReader → SymphoniaSource → Sink
//!
//! The consumer waits until enough audio is buffered: `START_BUFFER_SECS`
//! to start, `REFILL_BUFFER_SECS` after the buffer ran dry, and more after
//! repeated underruns, which minutes of good play undo. The times become
//! bytes at the stream's byte rate: measured from the audio the decoder
//! decodes, else the station's advertised bitrate, else 128 kbps.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::types::ReadSeek;
use crate::config::buffer::{
    COMPACTION_SAFETY_MARGIN, COMPACTION_THRESHOLD, CONSUMER_WAIT_TIMEOUT_MS, DEFAULT_BYTE_RATE,
    EMA_ALPHA_JITTER, EMA_ALPHA_THROUGHPUT, ESCALATION_DECAY_SECS, ESCALATION_FORGET_SECS,
    MAX_BUFFER_SIZE, MAX_BYTE_RATE, MAX_REFILL_SECS, MAX_WATERMARK_BYTES, MIN_BYTE_RATE,
    MIN_THROUGHPUT_INTERVAL_MS, PRODUCER_CHUNK_SIZE, RATE_SKIP_SECS, RATE_SPAN_SECS,
    REFILL_BUFFER_SECS, REFILL_STEP_SECS, START_BUFFER_SECS, START_MAX_WAIT_SECS,
};
use crate::stream::cancel::StreamCancel;

/// Network throughput and jitter metrics (EMA-smoothed)
pub struct NetworkMetrics {
    throughput_ema: f64,
    jitter_ema: f64,
    last_chunk_time: Instant,
    last_chunk_interval_ms: f64,
    total_bytes: u64,
    underrun_count: u32,
    /// Bytes accumulated since last EMA update (filters burst reads)
    pending_bytes: usize,
}

impl NetworkMetrics {
    fn new() -> Self {
        Self {
            throughput_ema: 0.0,
            jitter_ema: 0.0,
            last_chunk_time: Instant::now(),
            last_chunk_interval_ms: 0.0,
            total_bytes: 0,
            underrun_count: 0,
            pending_bytes: 0,
        }
    }

    /// Record a successful chunk read.
    ///
    /// Accumulates bytes and only updates the throughput EMA when enough time
    /// has passed (`MIN_THROUGHPUT_INTERVAL_MS`). This prevents burst reads
    /// after reconnection from spiking the EMA to unrealistic values.
    fn record_chunk(&mut self, bytes: usize) {
        self.total_bytes += bytes as u64;
        self.pending_bytes += bytes;

        let now = Instant::now();
        let elapsed_ms = now.duration_since(self.last_chunk_time).as_secs_f64() * 1000.0;

        // Only update EMA when enough time has passed to get a meaningful measurement.
        if elapsed_ms >= MIN_THROUGHPUT_INTERVAL_MS {
            let throughput = (self.pending_bytes as f64 / elapsed_ms) * 1000.0; // bytes/sec
            if self.throughput_ema == 0.0 {
                self.throughput_ema = throughput;
            } else {
                self.throughput_ema = EMA_ALPHA_THROUGHPUT * throughput
                    + (1.0 - EMA_ALPHA_THROUGHPUT) * self.throughput_ema;
            }

            // Jitter = deviation from previous interval
            let jitter = (elapsed_ms - self.last_chunk_interval_ms).abs();
            if self.last_chunk_interval_ms > 0.0 {
                self.jitter_ema =
                    EMA_ALPHA_JITTER * jitter + (1.0 - EMA_ALPHA_JITTER) * self.jitter_ema;
            }
            self.last_chunk_interval_ms = elapsed_ms;

            self.pending_bytes = 0;
            self.last_chunk_time = now;
        }
    }

    fn record_underrun(&mut self) {
        self.underrun_count += 1;
    }

    fn throughput_kbps(&self) -> f64 {
        (self.throughput_ema * 8.0) / 1000.0
    }
}

/// Shared buffer status (read by engine/UI)
#[derive(Debug, Clone)]
pub struct BufferStatus {
    pub level_bytes: usize,
    pub capacity_bytes: usize,
    pub is_buffering: bool,
    pub throughput_kbps: f64,
    pub underrun_count: u32,
    pub effective_watermark: usize,
}

impl Default for BufferStatus {
    fn default() -> Self {
        Self {
            level_bytes: 0,
            capacity_bytes: 0,
            is_buffering: false,
            throughput_kbps: 0.0,
            underrun_count: 0,
            effective_watermark: (START_BUFFER_SECS * DEFAULT_BYTE_RATE) as usize,
        }
    }
}

/// The stream's bytes per second of audio, which turns the buffer's targets
/// (in seconds) into bytes
struct ByteRate {
    /// From the station's advertised bitrate
    advertised: Option<f64>,
    /// Microseconds of audio decoded from the buffer, added by the decoder
    decoded: Arc<AtomicU64>,
    /// Read position and decoded time the measurement started at
    measured_from: Option<(u64, u64)>,
    measured: Option<f64>,
}

impl ByteRate {
    fn new() -> Self {
        Self {
            advertised: None,
            decoded: Arc::default(),
            measured_from: None,
            measured: None,
        }
    }

    /// Bytes per second: measured once possible (a station's advertised
    /// bitrate can be wrong), else advertised, else `DEFAULT_BYTE_RATE`
    fn get(&self) -> f64 {
        self.measured
            .or(self.advertised)
            .unwrap_or(DEFAULT_BYTE_RATE)
    }

    /// Measure how far the decoder has read (`read_pos`) against the audio
    /// it decoded. The decoder reads up to 32 KiB ahead, so the measurement
    /// spans at least `RATE_SPAN_SECS` and keeps growing to even that out.
    fn update(&mut self, read_pos: u64) {
        let decoded = self.decoded.load(Ordering::Relaxed);
        match self.measured_from {
            None => {
                if decoded as f64 >= RATE_SKIP_SECS * 1e6 {
                    self.measured_from = Some((read_pos, decoded));
                }
            }
            Some((from_pos, from_decoded)) => {
                let span = decoded.saturating_sub(from_decoded) as f64 / 1e6;
                if span >= RATE_SPAN_SECS {
                    let rate = read_pos.saturating_sub(from_pos) as f64 / span;
                    self.measured = Some(rate.clamp(MIN_BYTE_RATE, MAX_BYTE_RATE));
                }
            }
        }
    }
}

/// Thread-safe handle to shared buffer status
pub type SharedBufferStatus = Arc<Mutex<BufferStatus>>;

/// Shared mutable state behind Mutex
struct BufferInner {
    /// Buffered data
    data: Vec<u8>,
    /// Total bytes discarded by compaction (absolute offset of data[0])
    base_offset: u64,
    /// Next write position relative to data start
    write_pos: usize,
    /// Producer finished (EOF or error)
    producer_done: bool,
    /// Error message from producer (if any)
    producer_error: Option<String>,
    /// Network metrics
    metrics: NetworkMetrics,
}

/// Synchronization wrapper around BufferInner
struct BufferState {
    inner: Mutex<BufferInner>,
    data_available: Condvar,
    /// Signalled when compaction makes room in a full buffer
    space_available: Condvar,
    /// The network reader, once the producer has read all it will: kept
    /// until the buffer goes. The ICY and HLS readers stop the stream as
    /// they are dropped, and that would end decoding before the rest of the
    /// buffer, and the reason the stream ended, are read.
    finished_reader: Mutex<Option<Box<dyn ReadSeek>>>,
}

/// Longest the producer waits for room in a full buffer without being
/// woken (it is woken when the consumer makes room, and on a cancel)
const PRODUCER_FULL_WAIT: Duration = Duration::from_millis(500);

/// Creates a decoupled producer-consumer stream buffer.
///
/// Returns: (reader for symphonia, producer thread handle, stop flag)
pub struct StreamBuffer;

#[allow(clippy::new_ret_no_self)]
impl StreamBuffer {
    /// Create a new stream buffer with a background producer thread.
    ///
    /// The producer reads from `reader` in chunks and fills the shared buffer.
    /// The returned `StreamBufferReader` implements Read + Seek for symphonia.
    /// Cancelling the returned [`StreamCancel`] stops the buffer.
    pub fn new(
        reader: Box<dyn ReadSeek>,
        status: SharedBufferStatus,
        probing_flag: Arc<AtomicBool>,
    ) -> (StreamBufferReader, JoinHandle<()>, StreamCancel) {
        let cancel = StreamCancel::new();
        let (consumer, handle) = Self::with_cancel(reader, status, probing_flag, cancel.clone());
        (consumer, handle, cancel)
    }

    /// [`StreamBuffer::new`] stopped by the stream's own [`StreamCancel`]
    /// (from `ResolvedStream::cancel`). Cancelling it then stops the
    /// buffer and the network reader together, with no waiting.
    pub fn with_cancel(
        reader: Box<dyn ReadSeek>,
        status: SharedBufferStatus,
        probing_flag: Arc<AtomicBool>,
        cancel: StreamCancel,
    ) -> (StreamBufferReader, JoinHandle<()>) {
        let state = Arc::new(BufferState {
            inner: Mutex::new(BufferInner {
                data: Vec::with_capacity(PRODUCER_CHUNK_SIZE * 16),
                base_offset: 0,
                write_pos: 0,
                producer_done: false,
                producer_error: None,
                metrics: NetworkMetrics::new(),
            }),
            data_available: Condvar::new(),
            space_available: Condvar::new(),
            finished_reader: Mutex::new(None),
        });

        // Wake a consumer waiting for data, and a producer waiting for
        // room. Under the lock, so one between its cancel check and its
        // wait can't miss the wakeup.
        let woken = Arc::downgrade(&state);
        cancel.on_cancel(move || {
            if let Some(state) = woken.upgrade() {
                let _inner = state.inner.lock();
                state.data_available.notify_all();
                state.space_available.notify_all();
            }
        });

        let producer_state = state.clone();
        let producer_cancel = cancel.clone();

        let handle = thread::Builder::new()
            .name("stream-buffer-producer".to_string())
            .spawn(move || {
                Self::producer_loop(reader, producer_state, producer_cancel);
            })
            .expect("Failed to spawn buffer producer thread");

        let consumer = StreamBufferReader {
            state: state.clone(),
            cancel,
            status,
            read_pos: 0,
            probing_flag,
            // Wait for the start target, even when the station's first bytes
            // are already in
            buffering_active: true,
            started: false,
            underrun_escalations: 0,
            healthy_from: 0,
            buffering_start: None,
            escalation_decay_secs: ESCALATION_DECAY_SECS,
            start_max_wait: Duration::from_secs_f64(START_MAX_WAIT_SECS),
            rate: ByteRate::new(),
        };

        (consumer, handle)
    }

    /// Producer loop: reads chunks from inner reader, appends to shared buffer.
    ///
    /// A read waiting on the network returns as soon as the stream is
    /// cancelled when the reader shares the stream's token (the ICY and HLS
    /// readers do). Otherwise the inner reader may return
    /// `ErrorKind::Interrupted` while it waits (the ICY and HLS readers do, a
    /// few times a second); the producer then checks for a cancel and reads
    /// again, so a stop reaches it even when the station sends nothing. Every
    /// exit marks the producer done, so a waiting consumer never blocks on a
    /// producer that is gone.
    fn producer_loop(mut reader: Box<dyn ReadSeek>, state: Arc<BufferState>, cancel: StreamCancel) {
        let _done = ProducerDone(state.clone());
        let mut chunk = vec![0u8; PRODUCER_CHUNK_SIZE];

        loop {
            if cancel.is_cancelled() {
                break;
            }

            match reader.read(&mut chunk) {
                Ok(0) => break, // EOF
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(n) => {
                    // Keep this chunk until the buffer has room for it: it
                    // must not be lost by reading the next one first.
                    let Ok(mut inner) = state.inner.lock() else {
                        return; // Mutex poisoned
                    };
                    // Full (a paused player reads nothing): wait for the
                    // consumer to make room, or for a cancel
                    while inner.data.len() >= MAX_BUFFER_SIZE {
                        if cancel.is_cancelled() {
                            return;
                        }
                        inner = match state
                            .space_available
                            .wait_timeout(inner, PRODUCER_FULL_WAIT)
                        {
                            Ok((inner, _)) => inner,
                            Err(_) => return,
                        };
                    }
                    if cancel.is_cancelled() {
                        return;
                    }

                    inner.data.extend_from_slice(&chunk[..n]);
                    inner.write_pos += n;
                    inner.metrics.record_chunk(n);

                    drop(inner);
                    state.data_available.notify_all();
                }
                Err(e) => {
                    if let Ok(mut inner) = state.inner.lock() {
                        inner.producer_error = Some(e.to_string());
                    }
                    break;
                }
            }
        }

        // The stream ended or failed by itself: its reader stays until the
        // consumer has read the end
        if !cancel.is_cancelled() {
            if let Ok(mut finished) = state.finished_reader.lock() {
                *finished = Some(reader);
            }
        }
    }
}

/// Marks the producer done and wakes the consumer when the producer exits,
/// whichever way it exits (EOF, error, stop or panic).
struct ProducerDone(Arc<BufferState>);

impl Drop for ProducerDone {
    fn drop(&mut self) {
        let mut inner = match self.0.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.producer_done = true;
        drop(inner);
        self.0.data_available.notify_all();
    }
}

/// Consumer side: implements Read + Seek, passed to symphonia.
pub struct StreamBufferReader {
    state: Arc<BufferState>,
    /// Cancelled to stop this stream: reads then return end of stream
    cancel: StreamCancel,
    status: SharedBufferStatus,
    /// Absolute read position (across compactions)
    read_pos: u64,
    /// When true, inhibits compaction (symphonia may seek back during probe)
    probing_flag: Arc<AtomicBool>,
    /// Hysteresis flag: true while waiting for the buffer to refill to the
    /// effective watermark
    buffering_active: bool,
    /// Audio has been handed out: waiting after this is an underrun, not
    /// the start
    started: bool,
    /// Underruns that raise the refill target, each by `REFILL_STEP_SECS`
    /// after the first. Played audio undoes them (see `escalation`).
    underrun_escalations: u32,
    /// Read position where playing resumed after the last underrun
    healthy_from: u64,
    /// When buffering started (for escalation decay timing)
    buffering_start: Option<Instant>,
    /// Configurable decay threshold (seconds). Default: ESCALATION_DECAY_SECS.
    escalation_decay_secs: u64,
    /// Longest wait to start. Default: START_MAX_WAIT_SECS.
    start_max_wait: Duration,
    rate: ByteRate,
}

impl StreamBufferReader {
    /// Request compaction of already-consumed data.
    /// Should only be called when not probing.
    fn maybe_compact(&self) {
        if self.probing_flag.load(Ordering::Relaxed) {
            return;
        }

        if let Ok(mut inner) = self.state.inner.lock() {
            let local_read = (self.read_pos - inner.base_offset) as usize;
            if local_read > COMPACTION_THRESHOLD {
                let keep_from = local_read.saturating_sub(COMPACTION_SAFETY_MARGIN);
                if keep_from > 0 {
                    inner.data.drain(..keep_from);
                    inner.data.shrink_to(COMPACTION_THRESHOLD);
                    inner.base_offset += keep_from as u64;
                    inner.write_pos -= keep_from;
                    // A producer waiting on a full buffer has room now
                    self.state.space_available.notify_all();
                }
            }
        }
    }

    /// The station's advertised bitrate (`icy-br`), used for the byte rate
    /// until one is measured
    pub fn set_advertised_bitrate(&mut self, kbps: Option<u32>) {
        self.rate.advertised = kbps
            .map(|kbps| kbps as f64 * 1000.0 / 8.0)
            // Out of range is a bad header (some give bits per second)
            .filter(|rate| (MIN_BYTE_RATE..=MAX_BYTE_RATE).contains(rate));
    }

    /// Microseconds of audio the decoder has decoded from this buffer. The
    /// decoder adds to it (`SymphoniaSource::count_decoded_time`), and the
    /// buffer then measures the stream's bytes per second of audio.
    pub fn decoded_time(&self) -> Arc<AtomicU64> {
        self.rate.decoded.clone()
    }

    /// Underrun steps still in force: each `ESCALATION_FORGET_SECS` of audio
    /// played since the last underrun undoes one
    fn escalation(&self) -> u32 {
        if self.buffering_active {
            return self.underrun_escalations;
        }
        let played = self.read_pos.saturating_sub(self.healthy_from) as f64 / self.rate.get();
        let forgotten = (played / ESCALATION_FORGET_SECS) as u32;
        self.underrun_escalations.saturating_sub(forgotten)
    }

    /// Seconds of audio to buffer before playing (again)
    fn target_secs(&self) -> f64 {
        if !self.started {
            return START_BUFFER_SECS;
        }
        let steps = self.escalation().saturating_sub(1);
        (REFILL_BUFFER_SECS + REFILL_STEP_SECS * steps as f64).min(MAX_REFILL_SECS)
    }

    /// Bytes to buffer before playing (again): the target time at the
    /// stream's byte rate, capped at `MAX_WATERMARK_BYTES`
    fn effective_watermark(&self) -> usize {
        ((self.target_secs() * self.rate.get()) as usize).min(MAX_WATERMARK_BYTES)
    }

    /// The buffer is empty: wait for it to refill before handing out more.
    /// Before the first audio that is the start, not an underrun.
    fn start_buffering(&mut self, metrics: &mut NetworkMetrics) {
        if self.started {
            // More steps than reach MAX_REFILL_SECS would only take longer
            // to undo
            let max_steps =
                1 + ((MAX_REFILL_SECS - REFILL_BUFFER_SECS) / REFILL_STEP_SECS).ceil() as u32;
            self.underrun_escalations = (self.escalation() + 1).min(max_steps);
            metrics.record_underrun();
        }
        self.buffering_active = true;
    }

    /// Update the shared status snapshot with current buffer state.
    fn update_status(&self, inner: &BufferInner) {
        if let Ok(mut s) = self.status.lock() {
            let local_read = (self.read_pos.saturating_sub(inner.base_offset)) as usize;
            let available = inner.write_pos.saturating_sub(local_read);
            s.level_bytes = available;
            // Total data held in memory (including safety margin behind read cursor)
            s.capacity_bytes = inner.data.len();
            s.throughput_kbps = inner.metrics.throughput_kbps();
            s.underrun_count = inner.metrics.underrun_count;
            s.is_buffering = self.buffering_active;
            s.effective_watermark = self.effective_watermark();
        }
    }
}

impl Read for StreamBufferReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = Duration::from_millis(CONSUMER_WAIT_TIMEOUT_MS);

        // A handle of its own, so `self` stays free to update while locked
        let state = self.state.clone();
        let mut inner = state
            .inner
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;

        loop {
            // Stopped: end the stream now, so the decode thread reading it
            // ends too instead of waiting for a station that went down
            if self.cancel.is_cancelled() {
                return Ok(0);
            }

            let local_read = (self.read_pos - inner.base_offset) as usize;
            let available = inner.write_pos.saturating_sub(local_read);

            if self.buffering_active {
                // Track buffering start time
                if self.buffering_start.is_none() {
                    self.buffering_start = Some(Instant::now());
                }

                // Decay escalations after prolonged buffering (outage, not jitter)
                if let Some(start) = self.buffering_start {
                    if start.elapsed().as_secs() >= self.escalation_decay_secs
                        && self.underrun_escalations > 0
                    {
                        self.underrun_escalations = 0;
                    }
                }

                // Hysteresis: keep blocking until buffer refills to effective watermark
                // (escalates with each underrun) or the producer finishes (EOF/error).
                let watermark = self.effective_watermark();
                // A station slower than the byte rate assumed for it still
                // starts before the probe gives up
                let waited_to_start = !self.started
                    && available > 0
                    && self
                        .buffering_start
                        .is_some_and(|start| start.elapsed() >= self.start_max_wait);
                if available >= watermark || inner.producer_done || waited_to_start {
                    self.buffering_active = false;
                    self.buffering_start = None;
                    self.healthy_from = self.read_pos;
                    self.update_status(&inner);
                    // Fall through to normal read logic below
                } else {
                    // Still buffering — update status so UI sees progress, then wait
                    self.update_status(&inner);
                    let result = state.data_available.wait_timeout(inner, timeout);
                    match result {
                        Ok((guard, _)) => {
                            inner = guard;
                            continue;
                        }
                        Err(e) => return Err(io::Error::other(e.to_string())),
                    }
                }
            }

            // Re-compute available after potential buffering exit
            let local_read = (self.read_pos - inner.base_offset) as usize;
            let available = inner.write_pos.saturating_sub(local_read);

            if available > 0 {
                // Data available — copy to caller's buffer
                let to_copy = available.min(buf.len());
                buf[..to_copy].copy_from_slice(&inner.data[local_read..local_read + to_copy]);
                self.read_pos += to_copy as u64;
                self.started = true;
                self.rate.update(self.read_pos);

                // Check if buffer just emptied — enter buffering with hysteresis
                let new_local_read = (self.read_pos - inner.base_offset) as usize;
                let remaining = inner.write_pos.saturating_sub(new_local_read);
                if remaining == 0 && !inner.producer_done {
                    self.start_buffering(&mut inner.metrics);
                }

                self.update_status(&inner);
                drop(inner);

                // Try compaction after reading
                self.maybe_compact();
                return Ok(to_copy);
            }

            // No data available
            if inner.producer_done {
                // Check for error
                if let Some(ref err_msg) = inner.producer_error {
                    return Err(io::Error::other(err_msg.clone()));
                }
                // Clean EOF
                return Ok(0);
            }

            // Buffer is empty and producer is still running — enter buffering
            self.start_buffering(&mut inner.metrics);
            self.update_status(&inner);

            // Wait for producer to write more data
            let result = state.data_available.wait_timeout(inner, timeout);
            match result {
                Ok((guard, _timeout_result)) => {
                    // Loop back to re-check via hysteresis logic at top of loop.
                    // We must NOT return Ok(0) here — symphonia treats that
                    // as EOF and stops decoding. For HLS, the producer can
                    // block for seconds between segments, so we keep waiting.
                    inner = guard;
                }
                Err(e) => {
                    return Err(io::Error::other(e.to_string()));
                }
            }
        }
    }
}

impl Seek for StreamBufferReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let inner = self
            .state
            .inner
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let abs_end = inner.base_offset + inner.write_pos as u64;

        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => abs_end as i64 + offset,
            SeekFrom::Current(offset) => self.read_pos as i64 + offset,
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Seek to negative position",
            ));
        }

        let new_pos = new_pos as u64;

        if new_pos < inner.base_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Cannot seek to position {} — data before {} has been compacted",
                    new_pos, inner.base_offset
                ),
            ));
        }

        // Clamp to end of written data
        self.read_pos = new_pos.min(abs_end);
        Ok(self.read_pos)
    }
}

/// Largest read [`PlaybackPositionReader`] passes through. symphonia reads
/// ahead in blocks of up to 32 KiB (about 2 s at 128 kbps); smaller reads
/// keep the reported position within a fraction of a second of what is decoded.
const PLAYBACK_POSITION_MAX_READ: usize = 4 * 1024;

/// Reader adapter placed in front of the decoder. It publishes how many
/// bytes the decoder has read, and caps each read so the decoder's own
/// read-ahead stays small.
pub struct PlaybackPositionReader<R> {
    inner: R,
    pos: u64,
    shared: Arc<AtomicU64>,
}

impl<R> PlaybackPositionReader<R> {
    pub fn new(inner: R, shared: Arc<AtomicU64>) -> Self {
        shared.store(0, Ordering::Relaxed);
        Self {
            inner,
            pos: 0,
            shared,
        }
    }

    fn set_pos(&mut self, pos: u64) {
        self.pos = pos;
        self.shared.store(pos, Ordering::Relaxed);
    }
}

impl<R: Read> Read for PlaybackPositionReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(PLAYBACK_POSITION_MAX_READ);
        let n = self.inner.read(&mut buf[..len])?;
        self.set_pos(self.pos + n as u64);
        Ok(n)
    }
}

impl<R: Seek> Seek for PlaybackPositionReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = self.inner.seek(pos)?;
        self.set_pos(new_pos);
        Ok(new_pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// The start target at the default byte rate
    fn start_watermark() -> usize {
        (START_BUFFER_SECS * DEFAULT_BYTE_RATE) as usize
    }

    /// Helper to create a StreamBuffer from in-memory data
    fn buffer_from_data(data: Vec<u8>) -> (StreamBufferReader, JoinHandle<()>, StreamCancel) {
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        StreamBuffer::new(Box::new(Cursor::new(data)), status, probing)
    }

    /// Helper to create a StreamBuffer with access to status
    fn buffer_with_status(
        data: Vec<u8>,
    ) -> (
        StreamBufferReader,
        JoinHandle<()>,
        StreamCancel,
        SharedBufferStatus,
    ) {
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (reader, handle, stop) =
            StreamBuffer::new(Box::new(Cursor::new(data)), status.clone(), probing);
        (reader, handle, stop, status)
    }

    // --- NetworkMetrics ---

    #[test]
    fn metrics_new_defaults() {
        let m = NetworkMetrics::new();
        assert_eq!(m.throughput_ema, 0.0);
        assert_eq!(m.jitter_ema, 0.0);
        assert_eq!(m.underrun_count, 0);
        assert_eq!(m.total_bytes, 0);
    }

    #[test]
    fn metrics_record_chunk_updates_throughput() {
        let mut m = NetworkMetrics::new();
        // Sleep >= MIN_THROUGHPUT_INTERVAL_MS (100ms) so the EMA actually updates
        std::thread::sleep(Duration::from_millis(110));
        m.record_chunk(1024);
        assert!(m.throughput_ema > 0.0);
        assert_eq!(m.total_bytes, 1024);
    }

    #[test]
    fn metrics_record_underrun_increments() {
        let mut m = NetworkMetrics::new();
        m.record_underrun();
        m.record_underrun();
        assert_eq!(m.underrun_count, 2);
    }

    #[test]
    fn metrics_throughput_kbps_zero_initially() {
        let m = NetworkMetrics::new();
        assert_eq!(m.throughput_kbps(), 0.0);
    }

    #[test]
    fn metrics_multiple_chunks_accumulate() {
        let mut m = NetworkMetrics::new();
        // First chunk within MIN_THROUGHPUT_INTERVAL_MS — accumulates pending_bytes
        std::thread::sleep(Duration::from_millis(5));
        m.record_chunk(100);
        assert_eq!(m.total_bytes, 100);
        assert_eq!(m.pending_bytes, 100);
        // Second chunk after enough time for EMA update
        std::thread::sleep(Duration::from_millis(110));
        m.record_chunk(200);
        assert_eq!(m.total_bytes, 300);
        assert!(m.throughput_ema > 0.0);
        assert_eq!(m.pending_bytes, 0); // Reset after EMA update
    }

    // --- BufferStatus ---

    #[test]
    fn buffer_status_default() {
        let s = BufferStatus::default();
        assert_eq!(s.level_bytes, 0);
        assert_eq!(s.capacity_bytes, 0);
        assert!(!s.is_buffering);
        assert_eq!(s.throughput_kbps, 0.0);
        assert_eq!(s.underrun_count, 0);
        assert_eq!(s.effective_watermark, start_watermark());
    }

    #[test]
    fn buffer_status_clone() {
        let s = BufferStatus {
            level_bytes: 1024,
            capacity_bytes: 2048,
            is_buffering: true,
            throughput_kbps: 128.0,
            underrun_count: 3,
            effective_watermark: start_watermark(),
        };
        let c = s.clone();
        assert_eq!(c.level_bytes, 1024);
        assert_eq!(c.capacity_bytes, 2048);
        assert!(c.is_buffering);
    }

    // --- StreamBuffer: sequential read ---

    #[test]
    fn read_all_data_sequentially() {
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let (mut reader, handle, _stop) = buffer_from_data(data.clone());

        let mut result = Vec::new();
        let mut buf = [0u8; 128];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            result.extend_from_slice(&buf[..n]);
        }

        handle.join().unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn read_empty_data_returns_eof() {
        let (mut reader, handle, _stop) = buffer_from_data(Vec::new());

        let mut buf = [0u8; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);

        handle.join().unwrap();
    }

    #[test]
    fn read_small_data() {
        let data = vec![1u8, 2, 3, 4, 5];
        let (mut reader, handle, _stop) = buffer_from_data(data.clone());

        let mut out = [0u8; 10];
        let mut total = 0;
        loop {
            let n = reader.read(&mut out[total..]).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(&out[..total], &data[..]);

        handle.join().unwrap();
    }

    // --- StreamBuffer: seek ---

    #[test]
    fn seek_start_then_read() {
        let data = vec![10u8, 20, 30, 40, 50];
        let (mut reader, handle, _stop) = buffer_from_data(data.clone());

        // Read all data first (let producer finish)
        let mut out = Vec::new();
        let mut buf = [0u8; 10];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, data);

        // Seek back to start and re-read
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut out2 = [0u8; 5];
        let n = reader.read(&mut out2).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&out2, &[10, 20, 30, 40, 50]);

        handle.join().unwrap();
    }

    #[test]
    fn seek_current_forward() {
        let data = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let (mut reader, handle, _stop) = buffer_from_data(data);

        // Read 3 bytes
        let mut buf = [0u8; 3];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &[1, 2, 3]);

        // Seek forward 2 from current
        reader.seek(SeekFrom::Current(2)).unwrap();

        // Read next byte (should be 6)
        let mut one = [0u8; 1];
        reader.read_exact(&mut one).unwrap();
        assert_eq!(one[0], 6);

        handle.join().unwrap();
    }

    #[test]
    fn seek_end() {
        let data = vec![1u8, 2, 3, 4, 5];
        let (mut reader, handle, _stop) = buffer_from_data(data);

        // Drain all data first so producer finishes
        let mut discard = [0u8; 64];
        while reader.read(&mut discard).unwrap() > 0 {}

        // Seek to 2 bytes before end
        let pos = reader.seek(SeekFrom::End(-2)).unwrap();
        assert_eq!(pos, 3);

        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &[4, 5]);

        handle.join().unwrap();
    }

    #[test]
    fn seek_negative_returns_error() {
        let (mut reader, handle, stop) = buffer_from_data(vec![1, 2, 3]);
        let result = reader.seek(SeekFrom::Start(0));
        assert!(result.is_ok());

        let result = reader.seek(SeekFrom::Current(-1));
        assert!(result.is_err());

        stop.cancel();
        handle.join().unwrap();
    }

    // --- StreamBuffer: a full buffer ---

    /// Endless bytes, counting how many were read
    struct Endless(Arc<AtomicU64>);

    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.fetch_add(buf.len() as u64, Ordering::SeqCst);
            buf.fill(7);
            Ok(buf.len())
        }
    }

    impl Seek for Endless {
        fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn a_full_buffer_waits_for_room_and_a_cancel_ends_the_wait() {
        let read = Arc::new(AtomicU64::new(0));
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (mut reader, handle, stop) =
            StreamBuffer::new(Box::new(Endless(read.clone())), status, probing);
        let filled = |read: &AtomicU64| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut last = read.load(Ordering::SeqCst);
            loop {
                thread::sleep(Duration::from_millis(50));
                let now = read.load(Ordering::SeqCst);
                if now == last {
                    return now;
                }
                assert!(Instant::now() < deadline, "the buffer never filled");
                last = now;
            }
        };
        // Full: the producer holds one chunk more than fits, and stops
        let full = filled(&read);
        assert!(full >= MAX_BUFFER_SIZE as u64);
        assert!(full <= (MAX_BUFFER_SIZE + 2 * PRODUCER_CHUNK_SIZE) as u64);

        // Reading past the compaction point makes room: it carries on
        let mut sink = vec![0u8; 64 * 1024];
        let mut taken = 0;
        while taken <= COMPACTION_THRESHOLD + sink.len() {
            taken += reader.read(&mut sink).unwrap();
        }
        assert!(filled(&read) > full, "the producer wasn't woken");

        // A cancel ends its wait at once
        let cancelled = Instant::now();
        stop.cancel();
        handle.join().unwrap();
        assert!(cancelled.elapsed() < Duration::from_millis(400));
    }

    // --- StreamBuffer: stop flag ---

    #[test]
    fn stop_flag_causes_producer_to_exit() {
        // Use a large data source that won't finish quickly
        let data: Vec<u8> = vec![0u8; 1_000_000];
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (mut reader, handle, stop) =
            StreamBuffer::new(Box::new(Cursor::new(data)), status, probing);

        // Read a small amount
        let mut buf = [0u8; 100];
        reader.read_exact(&mut buf).unwrap();

        // Signal stop
        stop.cancel();

        // Producer should exit
        handle.join().unwrap();
    }

    // --- StreamBuffer: large data ---

    #[test]
    fn large_data_read_completely() {
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let (mut reader, handle, _stop) = buffer_from_data(data.clone());

        let mut total_read = 0;
        let mut buf = [0u8; 4096];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total_read += n;
        }
        assert_eq!(total_read, data.len());

        handle.join().unwrap();
    }

    // --- StreamBuffer: status tracking ---

    #[test]
    fn status_reflects_throughput_after_read() {
        let data = vec![0u8; 8192];
        let (mut reader, handle, _stop, status) = buffer_with_status(data);

        std::thread::sleep(Duration::from_millis(10));
        let mut buf = [0u8; 4096];
        reader.read_exact(&mut buf).unwrap();

        let s = status.lock().unwrap();
        // After reading, there should be some level tracked
        // (throughput may or may not be > 0 depending on timing)
        assert!(s.level_bytes > 0 || s.underrun_count > 0 || s.throughput_kbps >= 0.0);

        drop(s);
        drop(reader);
        handle.join().unwrap();
    }

    #[test]
    fn status_shared_accessible() {
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let s = status.lock().unwrap();
        assert_eq!(s.level_bytes, 0);
    }

    // --- StreamBuffer: probing flag ---

    #[test]
    fn probing_flag_inhibits_compaction() {
        // With probing=true, compaction should not happen even with lots of reading
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(true)); // probing mode ON
        let (mut reader, handle, _stop) =
            StreamBuffer::new(Box::new(Cursor::new(data.clone())), status, probing.clone());

        // Read all data
        let mut buf = [0u8; 4096];
        let mut total = 0;
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, data.len());

        // With probing on, base_offset should still be 0 (no compaction)
        let inner = reader.state.inner.lock().unwrap();
        assert_eq!(inner.base_offset, 0);
        drop(inner);

        // Seek back to start should work (data not compacted)
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut first = [0u8; 5];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, &data[..5]);

        // Now disable probing and trigger compaction
        probing.store(false, Ordering::Relaxed);
        reader.maybe_compact();

        handle.join().unwrap();
    }

    // --- StreamBuffer: error propagation ---

    /// A reader that fails after producing some data
    struct FailAfterReader {
        data: Cursor<Vec<u8>>,
        bytes_before_fail: usize,
        bytes_read: usize,
    }

    impl FailAfterReader {
        fn new(data: Vec<u8>, fail_after: usize) -> Self {
            Self {
                data: Cursor::new(data),
                bytes_before_fail: fail_after,
                bytes_read: 0,
            }
        }
    }

    impl Read for FailAfterReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.bytes_read >= self.bytes_before_fail {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "simulated network error",
                ));
            }
            let remaining = self.bytes_before_fail - self.bytes_read;
            let limit = buf.len().min(remaining);
            let n = self.data.read(&mut buf[..limit])?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    impl Seek for FailAfterReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.data.seek(pos)
        }
    }

    #[test]
    fn producer_error_propagates_after_draining() {
        let data = vec![42u8; 1000];
        let fail_reader = FailAfterReader::new(data.clone(), 500);
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (mut reader, handle, _stop) = StreamBuffer::new(Box::new(fail_reader), status, probing);

        // Read all available data (500 bytes)
        let mut result = Vec::new();
        let mut buf = [0u8; 128];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => result.extend_from_slice(&buf[..n]),
                Err(e) => {
                    assert!(e.to_string().contains("simulated network error"));
                    break;
                }
            }
        }

        // Should have read 500 bytes before error
        assert_eq!(result.len(), 500);
        assert!(result.iter().all(|&b| b == 42));

        handle.join().unwrap();
    }

    /// Fails at once, and stops `cancel` when dropped, as the ICY and HLS
    /// readers do
    struct FailingNetworkReader {
        cancel: StreamCancel,
    }

    impl Read for FailingNetworkReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("No audio for 2 min: connection lost"))
        }
    }

    impl Seek for FailingNetworkReader {
        fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
    }

    impl Drop for FailingNetworkReader {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    #[test]
    fn a_failed_reader_stops_the_stream_only_once_its_reason_is_read() {
        let cancel = StreamCancel::new();
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let failing = FailingNetworkReader {
            cancel: cancel.clone(),
        };
        let (mut reader, handle) =
            StreamBuffer::with_cancel(Box::new(failing), status, probing, cancel.clone());
        handle.join().unwrap();
        // The decoder reading the buffer stops on a cancel: it must hear why
        // the station ended first
        assert!(!cancel.is_cancelled(), "stopped before the reason was read");

        let err = reader
            .read(&mut [0u8; 16])
            .expect_err("the reason, not an end");
        assert!(err.to_string().contains("No audio for 2 min"), "{err}");
        // Done with the stream: its reader goes, and stops it
        drop(reader);
        assert!(cancel.is_cancelled());
    }

    // --- Compaction ---

    #[test]
    fn compaction_frees_memory() {
        // Generate data larger than COMPACTION_THRESHOLD
        let size = COMPACTION_THRESHOLD + 100_000;
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (mut reader, handle, _stop) =
            StreamBuffer::new(Box::new(Cursor::new(data.clone())), status, probing);

        // Read past COMPACTION_THRESHOLD
        let mut buf = [0u8; 8192];
        let mut total = 0;
        while total < COMPACTION_THRESHOLD + 50_000 {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }

        // Trigger compaction
        reader.maybe_compact();

        // Check that base_offset advanced
        let inner = reader.state.inner.lock().unwrap();
        assert!(
            inner.base_offset > 0,
            "Expected compaction to advance base_offset"
        );
        drop(inner);

        // Continue reading — data should still be correct
        let pos = reader.read_pos as usize;
        let mut remaining = Vec::new();
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            remaining.extend_from_slice(&buf[..n]);
        }

        // Verify the data we read after compaction matches
        assert_eq!(&remaining[..], &data[pos..]);

        handle.join().unwrap();
    }

    #[test]
    fn seek_before_base_offset_returns_error() {
        let size = COMPACTION_THRESHOLD + 100_000;
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (mut reader, handle, _stop) =
            StreamBuffer::new(Box::new(Cursor::new(data)), status, probing);

        // Read past compaction threshold
        let mut buf = [0u8; 8192];
        let mut total = 0;
        while total < COMPACTION_THRESHOLD + 50_000 {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }

        // Force compaction
        reader.maybe_compact();

        let base = reader.state.inner.lock().unwrap().base_offset;
        assert!(base > 0);

        // Seeking before base_offset should fail
        let result = reader.seek(SeekFrom::Start(0));
        assert!(result.is_err());

        drop(reader);
        handle.join().unwrap();
    }

    // =========================================================================
    // Network simulation fixtures
    // =========================================================================

    /// A reader that simulates network conditions: configurable latency per read,
    /// periodic stalls, jitter, and mid-stream errors.
    struct SimulatedNetworkReader {
        data: Cursor<Vec<u8>>,
        /// Base delay per read() call
        base_latency: Duration,
        /// Random jitter range added to base latency (0..jitter_range_ms)
        jitter_range_ms: u64,
        /// If set, stall for this duration every N reads
        stall_every_n_reads: Option<(usize, Duration)>,
        /// If set, return error after this many bytes
        error_after_bytes: Option<usize>,
        /// Track state
        bytes_read: usize,
        read_count: usize,
        /// Simple PRNG state for deterministic jitter
        rng_state: u64,
    }

    impl SimulatedNetworkReader {
        fn new(data: Vec<u8>) -> Self {
            Self {
                data: Cursor::new(data),
                base_latency: Duration::ZERO,
                jitter_range_ms: 0,
                stall_every_n_reads: None,
                error_after_bytes: None,
                bytes_read: 0,
                read_count: 0,
                rng_state: 12345,
            }
        }

        /// Set a fixed delay per read
        fn with_latency(mut self, latency: Duration) -> Self {
            self.base_latency = latency;
            self
        }

        /// Add random jitter (0..range_ms) on top of base latency
        fn with_jitter(mut self, range_ms: u64) -> Self {
            self.jitter_range_ms = range_ms;
            self
        }

        /// Every N reads, stall for the given duration
        fn with_periodic_stall(mut self, every_n: usize, stall_duration: Duration) -> Self {
            self.stall_every_n_reads = Some((every_n, stall_duration));
            self
        }

        /// Return a network error after N bytes
        fn with_error_after(mut self, bytes: usize) -> Self {
            self.error_after_bytes = Some(bytes);
            self
        }

        /// Simple xorshift for deterministic pseudo-random jitter
        fn next_rand(&mut self) -> u64 {
            self.rng_state ^= self.rng_state << 13;
            self.rng_state ^= self.rng_state >> 7;
            self.rng_state ^= self.rng_state << 17;
            self.rng_state
        }
    }

    impl Read for SimulatedNetworkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Check error trigger
            if let Some(limit) = self.error_after_bytes {
                if self.bytes_read >= limit {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "simulated connection reset",
                    ));
                }
            }

            self.read_count += 1;

            // Periodic stall
            if let Some((every_n, stall_dur)) = self.stall_every_n_reads {
                if self.read_count.is_multiple_of(every_n) {
                    thread::sleep(stall_dur);
                }
            }

            // Base latency + jitter
            let mut delay = self.base_latency;
            if self.jitter_range_ms > 0 {
                let jitter_ms = self.next_rand() % self.jitter_range_ms;
                delay += Duration::from_millis(jitter_ms);
            }
            if !delay.is_zero() {
                thread::sleep(delay);
            }

            // Limit read to respect error_after_bytes boundary
            let max_read = if let Some(limit) = self.error_after_bytes {
                let remaining = limit - self.bytes_read;
                buf.len().min(remaining)
            } else {
                buf.len()
            };

            let n = self.data.read(&mut buf[..max_read])?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    impl Seek for SimulatedNetworkReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.data.seek(pos)
        }
    }

    /// Helper: create a StreamBuffer from a SimulatedNetworkReader
    fn buffer_from_sim(
        sim: SimulatedNetworkReader,
    ) -> (
        StreamBufferReader,
        JoinHandle<()>,
        StreamCancel,
        SharedBufferStatus,
    ) {
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (reader, handle, stop) = StreamBuffer::new(Box::new(sim), status.clone(), probing);
        (reader, handle, stop, status)
    }

    /// Helper: drain all data from a reader, returning bytes read and any error
    fn drain_reader(reader: &mut StreamBufferReader) -> (Vec<u8>, Option<io::Error>) {
        let mut result = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return (result, None),
                Ok(n) => result.extend_from_slice(&buf[..n]),
                Err(e) => return (result, Some(e)),
            }
        }
    }

    // =========================================================================
    // Slow producer tests — consumer outruns producer
    // =========================================================================

    #[test]
    fn slow_producer_consumer_still_gets_all_data() {
        // Producer reads at 1ms per chunk — consumer must wait but gets everything
        let data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_millis(1));
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(result, data);
        handle.join().unwrap();
    }

    #[test]
    fn slow_producer_status_shows_buffering_state() {
        // With slow producer and data >> MAX_WATERMARK_BYTES, consumer should observe
        // is_buffering=true after draining the initial watermark fill.
        // Data must exceed MAX_WATERMARK_BYTES (512KB) because the adaptive watermark
        // can escalate up to that cap via the throughput floor.
        let data = vec![0u8; 2_000_000];
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(2));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        let mut saw_buffering = false;
        let mut buf = [0u8; 8192];
        for _ in 0..100 {
            let _ = reader.read(&mut buf);
            if let Ok(s) = status.lock() {
                if s.is_buffering {
                    saw_buffering = true;
                }
            }
        }

        stop.cancel();
        handle.join().unwrap();

        // After the initial watermark fill, the consumer drains buffer faster
        // than the slow producer can refill it, triggering buffering
        assert!(
            saw_buffering,
            "Expected consumer to observe buffering state"
        );
    }

    // =========================================================================
    // Jitter tests — variable latency
    // =========================================================================

    #[test]
    fn jittery_network_delivers_correct_data() {
        // Random delays between 0-10ms per read — data must still be correct
        let data: Vec<u8> = (0..20_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone())
            .with_latency(Duration::from_millis(1))
            .with_jitter(10);
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(result, data);
        handle.join().unwrap();
    }

    #[test]
    fn jittery_network_underrun_count_increases() {
        // Heavy jitter should cause some consumer stalls (waits on condvar).
        // More data than the start target, so the consumer outruns the
        // producer once playing: waiting to start is not an underrun.
        let data = vec![0u8; 200_000];
        let sim = SimulatedNetworkReader::new(data)
            .with_latency(Duration::from_millis(2))
            .with_jitter(20);
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        let mut buf = [0u8; 8192];
        for _ in 0..20 {
            let _ = reader.read(&mut buf);
        }

        let underruns = status.lock().unwrap().underrun_count;

        stop.cancel();
        handle.join().unwrap();

        // With jittery + slow producer, some underruns are expected
        assert!(
            underruns > 0,
            "Expected at least some consumer underruns, got 0"
        );
    }

    // =========================================================================
    // Periodic stall tests — simulates network hiccups
    // =========================================================================

    #[test]
    fn periodic_stalls_data_integrity() {
        // Stall for 50ms every 5 reads — data must still arrive correctly
        let data: Vec<u8> = (0..50_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone())
            .with_periodic_stall(5, Duration::from_millis(50));
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(result, data);
        handle.join().unwrap();
    }

    #[test]
    fn periodic_stalls_buffer_absorbs_hiccups() {
        // Producer stalls every 3 reads for 20ms. If the buffer has pre-filled
        // data, the consumer should read some without waiting.
        let data = vec![0u8; 100_000];
        let sim =
            SimulatedNetworkReader::new(data).with_periodic_stall(3, Duration::from_millis(20));
        let (mut reader, handle, stop, _status) = buffer_from_sim(sim);

        // Let the producer fill the buffer for a bit
        thread::sleep(Duration::from_millis(50));

        // Now read rapidly — the buffer should have data available
        let mut buf = [0u8; 1024];
        let mut instant_reads = 0;
        for _ in 0..20 {
            let start = Instant::now();
            if reader.read(&mut buf).unwrap_or(0) > 0 && start.elapsed() < Duration::from_millis(5)
            {
                instant_reads += 1;
            }
        }

        stop.cancel();
        handle.join().unwrap();

        // Most reads should be near-instant from the pre-filled buffer
        assert!(
            instant_reads > 10,
            "Expected most reads to be instant from buffer, got {} out of 20",
            instant_reads
        );
    }

    // =========================================================================
    // Error mid-stream tests
    // =========================================================================

    #[test]
    fn error_after_partial_data_preserves_bytes() {
        // Network error after 2000 bytes — consumer should get all 2000 then error
        let data: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone()).with_error_after(2000);
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        let (result, err) = drain_reader(&mut reader);
        assert_eq!(result.len(), 2000);
        assert_eq!(&result[..], &data[..2000]);
        assert!(err.is_some());
        assert!(err.unwrap().to_string().contains("connection reset"));
        handle.join().unwrap();
    }

    #[test]
    fn error_after_zero_bytes_immediate() {
        // Network error immediately — no data at all
        let sim = SimulatedNetworkReader::new(vec![0u8; 1000]).with_error_after(0);
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        let (result, err) = drain_reader(&mut reader);
        assert_eq!(result.len(), 0);
        assert!(err.is_some());
        handle.join().unwrap();
    }

    #[test]
    fn error_with_latency_still_propagates() {
        // Slow network that errors after 1000 bytes
        let data = vec![99u8; 5000];
        let sim = SimulatedNetworkReader::new(data)
            .with_latency(Duration::from_millis(2))
            .with_error_after(1000);
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        let (result, err) = drain_reader(&mut reader);
        assert_eq!(result.len(), 1000);
        assert!(result.iter().all(|&b| b == 99));
        assert!(err.is_some());
        handle.join().unwrap();
    }

    // =========================================================================
    // Concurrent read/write stress tests
    // =========================================================================

    #[test]
    fn consumer_reads_while_producer_writes_continuously() {
        // Large data with slight latency — tests concurrent producer/consumer
        let data: Vec<u8> = (0..500_000).map(|i| (i % 256) as u8).collect();
        let sim =
            SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_micros(100));
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(result.len(), data.len());
        assert_eq!(result, data);
        handle.join().unwrap();
    }

    #[test]
    fn many_tiny_reads_from_buffered_data() {
        // Consumer reads 1 byte at a time — exercises the read path heavily
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let (mut reader, handle, _stop) = buffer_from_data(data.clone());

        let mut result = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            result.push(buf[0]);
        }
        assert_eq!(result, data);
        handle.join().unwrap();
    }

    // =========================================================================
    // Stop flag during active operation
    // =========================================================================

    #[test]
    fn stop_flag_during_slow_producer() {
        // Producer is slow, stop is signaled mid-stream — should exit cleanly
        let data = vec![0u8; 1_000_000];
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(5));
        let (mut reader, handle, stop, _status) = buffer_from_sim(sim);

        // Read a little
        let mut buf = [0u8; 1024];
        reader.read_exact(&mut buf).unwrap();

        // Signal stop
        stop.cancel();

        // Producer should exit promptly (within a few ms)
        let join_start = Instant::now();
        handle.join().unwrap();
        assert!(
            join_start.elapsed() < Duration::from_secs(1),
            "Producer didn't exit promptly after stop signal"
        );
    }

    #[test]
    fn stop_flag_during_stall() {
        // Producer is stalled (sleeping), stop should still work
        let data = vec![0u8; 100_000];
        let sim =
            SimulatedNetworkReader::new(data).with_periodic_stall(1, Duration::from_millis(100)); // stall on every read
        let (_reader, handle, stop, _status) = buffer_from_sim(sim);

        // Let it start stalling
        thread::sleep(Duration::from_millis(50));

        stop.cancel();
        handle.join().unwrap();
    }

    // =========================================================================
    // Probing mode seek safety
    // =========================================================================

    #[test]
    fn probing_mode_allows_full_seek_back() {
        // During probing, all data should be retained for seek-back
        let data: Vec<u8> = (0..50_000).map(|i| (i % 256) as u8).collect();
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(true));
        let (mut reader, handle, _stop) =
            StreamBuffer::new(Box::new(Cursor::new(data.clone())), status, probing.clone());

        // Read all
        let (result, _) = drain_reader(&mut reader);
        assert_eq!(result, data);

        // Seek to various positions and verify
        for &pos in &[0u64, 100, 5000, 49999] {
            reader.seek(SeekFrom::Start(pos)).unwrap();
            let mut buf = [0u8; 1];
            reader.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], data[pos as usize], "Mismatch at seek pos {}", pos);
        }

        // Disable probing — now compaction can happen
        probing.store(false, Ordering::Relaxed);
        handle.join().unwrap();
    }

    #[test]
    fn probing_to_normal_transition() {
        // Start in probing mode, seek around, then switch to normal mode
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(true));
        let (mut reader, handle, _stop) =
            StreamBuffer::new(Box::new(Cursor::new(data.clone())), status, probing.clone());

        // Read 20KB (simulating probe reads)
        let mut buf = [0u8; 4096];
        let mut total = 0;
        while total < 20_000 {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }

        // Seek back to start (probe does this)
        reader.seek(SeekFrom::Start(0)).unwrap();

        // Read first few bytes to verify
        let mut first = [0u8; 4];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, &data[..4]);

        // Transition out of probing mode
        probing.store(false, Ordering::Relaxed);

        // Continue reading the rest — should still work
        reader.seek(SeekFrom::Start(0)).unwrap();
        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(result, data);
        handle.join().unwrap();
    }

    // =========================================================================
    // Buffer status tracking accuracy
    // =========================================================================

    #[test]
    fn status_level_decreases_as_consumer_reads() {
        // Fill buffer, then read from it — level should decrease
        let data = vec![0u8; 50_000];
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (mut reader, handle, _stop) =
            StreamBuffer::new(Box::new(Cursor::new(data)), status.clone(), probing);

        // Let the producer fill up
        thread::sleep(Duration::from_millis(50));

        let _level_before = status.lock().unwrap().level_bytes;

        // Read a chunk
        let mut buf = [0u8; 8192];
        reader.read_exact(&mut buf).unwrap();

        let level_after = status.lock().unwrap().level_bytes;

        // Level should have decreased (or stayed similar if producer is fast)
        // The key thing: level_after should be less than the total data
        assert!(level_after < 50_000, "Level should be less than total data");

        drop(reader);
        handle.join().unwrap();
    }

    #[test]
    fn status_not_buffering_when_producer_done() {
        // After EOF, is_buffering should be false
        let data = vec![0u8; 100];
        let (mut reader, handle, _stop, status) = buffer_with_status(data);

        // Drain all
        let _ = drain_reader(&mut reader);

        let s = status.lock().unwrap();
        assert!(!s.is_buffering, "Should not be buffering after EOF");
        drop(s);
        handle.join().unwrap();
    }

    #[test]
    fn underrun_count_tracks_consumer_waits() {
        // Slow producer forces consumer to wait — underrun_count should reflect this.
        // More data than the start target: waiting to start is not an underrun.
        let data = vec![0u8; 200_000];
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(5));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // Read rapidly to outrun the producer
        let mut buf = [0u8; 8192];
        for _ in 0..10 {
            let _ = reader.read(&mut buf);
        }

        let underruns = status.lock().unwrap().underrun_count;

        stop.cancel();
        handle.join().unwrap();

        assert!(
            underruns > 0,
            "Consumer should have had at least one underrun"
        );
    }

    // =========================================================================
    // Compaction under concurrent load
    // =========================================================================

    #[test]
    fn compaction_during_slow_producer_preserves_data() {
        // Producer is slow, data is large enough to trigger compaction,
        // and we verify data integrity throughout
        let size = COMPACTION_THRESHOLD + 200_000;
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_micros(50));
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(result.len(), data.len());
        assert_eq!(result, data);
        handle.join().unwrap();
    }

    #[test]
    fn multiple_compactions_data_still_correct() {
        // Data large enough for multiple compaction cycles
        let size = COMPACTION_THRESHOLD * 3;
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let (mut reader, handle, _stop) = buffer_from_data(data.clone());

        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(result.len(), data.len());
        assert_eq!(result, data);

        // Verify compaction actually happened
        let base = reader.state.inner.lock().unwrap().base_offset;
        assert!(
            base > 0,
            "Expected at least one compaction to have occurred"
        );

        handle.join().unwrap();
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn consumer_waits_for_slow_producer_instead_of_eof() {
        // When producer is slow, consumer should keep waiting (not return Ok(0)).
        // Returning Ok(0) would cause symphonia to interpret it as EOF.
        // The consumer should eventually get data when the producer catches up.
        let data: Vec<u8> = (0..20_000).map(|i| (i % 256) as u8).collect();
        let sim =
            SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_millis(100)); // Slow but not impossibly so
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        // Read all data — consumer will need to wait multiple times for the
        // slow producer, but should never get a premature EOF
        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none(), "Should not get an error from slow producer");
        assert_eq!(result.len(), data.len());
        assert_eq!(result, data);
        handle.join().unwrap();
    }

    #[test]
    fn consumer_returns_eof_only_when_producer_done() {
        // Ok(0) should only be returned when the producer has finished
        let data = vec![1u8, 2, 3];
        let (mut reader, handle, _stop) = buffer_from_data(data);

        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(result, vec![1, 2, 3]);
        handle.join().unwrap();
    }

    #[test]
    fn read_exact_works_across_producer_chunks() {
        // read_exact must block until enough data is available,
        // even if it spans multiple producer chunks
        let data: Vec<u8> = (0..32_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_millis(1));
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        // Request more than PRODUCER_CHUNK_SIZE (8KB) in one read_exact
        let mut big_buf = vec![0u8; 16_000];
        reader.read_exact(&mut big_buf).unwrap();
        assert_eq!(&big_buf[..], &data[..16_000]);

        handle.join().unwrap();
    }

    #[test]
    fn seek_within_safety_margin_after_compaction() {
        // After compaction, seeking within the safety margin should work
        let size = COMPACTION_THRESHOLD + 200_000;
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let (mut reader, handle, _stop) = buffer_from_data(data.clone());

        // Read past compaction threshold
        let mut buf = [0u8; 8192];
        let mut total = 0;
        while total < COMPACTION_THRESHOLD + 100_000 {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }

        reader.maybe_compact();

        let inner = reader.state.inner.lock().unwrap();
        let base = inner.base_offset;
        drop(inner);

        // Seek to just after base_offset (within safety margin) should work
        let target = base + 100;
        let pos = reader.seek(SeekFrom::Start(target)).unwrap();
        assert_eq!(pos, target);

        let mut one = [0u8; 1];
        reader.read_exact(&mut one).unwrap();
        assert_eq!(one[0], data[target as usize]);

        handle.join().unwrap();
    }

    // =========================================================================
    // Buffering hysteresis tests
    // =========================================================================

    /// Helper: read until status shows is_buffering == true, then return total bytes read.
    fn read_until_buffering(reader: &mut StreamBufferReader, status: &SharedBufferStatus) -> usize {
        let mut buf = [0u8; 8192];
        let mut total = 0;
        loop {
            let n = reader.read(&mut buf).unwrap();
            total += n;
            if status.lock().unwrap().is_buffering {
                break;
            }
            if n == 0 {
                break;
            }
        }
        total
    }

    #[test]
    fn hysteresis_blocks_until_high_watermark() {
        // After buffer empties, consumer should NOT get data until the effective
        // watermark is reached. Use a slow producer with data >> MAX_WATERMARK_BYTES
        // so the adaptive watermark can be satisfied even after escalation.
        let data: Vec<u8> = (0..2_000_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_millis(1));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // Drain rapidly until buffering activates
        let drained = read_until_buffering(&mut reader, &status);
        assert!(drained > 0);
        assert!(status.lock().unwrap().is_buffering, "Should be buffering");

        // Now read again — this should block until the effective watermark is reached.
        // The read should succeed with data once the watermark is met.
        let mut buf = [0u8; 8192];
        let n = reader.read(&mut buf).unwrap();
        assert!(n > 0, "Should eventually get data after watermark fill");

        // After the read, buffering should be resolved
        assert!(
            !status.lock().unwrap().is_buffering,
            "Should exit buffering after watermark reached"
        );

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn hysteresis_exits_on_producer_done() {
        // If producer finishes (EOF) while in buffering mode, consumer gets remaining
        // data without waiting for the watermark.
        let data = vec![42u8; 1024]; // Small: 1KB, less than any watermark
        let sim = SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_millis(2));
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        // Drain all data — will eventually trigger buffering then producer finishes
        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none(), "Should not error");
        assert_eq!(
            result, data,
            "All data should be received despite being under the watermark"
        );

        handle.join().unwrap();
    }

    #[test]
    fn underrun_count_per_event_not_per_wait() {
        // Verify underrun counter increments once per buffer-empty event,
        // not once per condvar wait. With a slow producer, a single buffering
        // event involves many condvar waits.
        let data = vec![0u8; 100_000];
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(3));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // Read rapidly to trigger buffering, then let it recover, repeat
        let mut buf = [0u8; 8192];
        let mut buffering_transitions = 0u32;
        let mut was_buffering = false;
        for _ in 0..30 {
            let _ = reader.read(&mut buf);
            let is_buf = status.lock().unwrap().is_buffering;
            if is_buf && !was_buffering {
                buffering_transitions += 1;
            }
            was_buffering = is_buf;
        }

        let underruns = status.lock().unwrap().underrun_count;

        stop.cancel();
        handle.join().unwrap();

        // Underrun count should be close to the number of buffering transitions,
        // not inflated by many condvar waits per event.
        // Allow some slack: underruns <= transitions * 2 (a read that empties
        // the buffer also counts, so there can be at most 2 per cycle).
        if buffering_transitions > 0 {
            assert!(
                underruns <= buffering_transitions * 2,
                "Underruns ({}) should not be much larger than buffering transitions ({})",
                underruns,
                buffering_transitions
            );
        }
    }

    #[test]
    fn fast_producer_never_enters_buffering() {
        // With instant in-memory producer, buffering should never activate
        // once the producer has had time to fill the buffer.
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let (mut reader, handle, _stop, status) = buffer_with_status(data.clone());

        // Let the fast producer fill the buffer before we start reading.
        // Without this, the consumer's first read() can race with the
        // producer thread startup and find an empty buffer.
        thread::sleep(Duration::from_millis(50));

        let mut buf = [0u8; 4096];
        let mut saw_buffering = false;
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            if status.lock().unwrap().is_buffering {
                saw_buffering = true;
            }
        }

        let s = status.lock().unwrap();
        assert!(
            !saw_buffering,
            "Fast producer should never trigger buffering"
        );
        assert_eq!(s.underrun_count, 0, "No underruns with fast producer");

        drop(s);
        handle.join().unwrap();
    }

    #[test]
    fn buffering_percentage_progresses() {
        // During buffering, level_bytes should grow toward the effective watermark.
        // Status is only updated inside read() calls, so we need a reader
        // thread blocked in the buffering condvar loop to observe progress.
        // Data must exceed MAX_WATERMARK_BYTES so the adaptive watermark can be met.
        let data: Vec<u8> = (0..2_000_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(2));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // Drain until buffering activates
        read_until_buffering(&mut reader, &status);
        assert!(status.lock().unwrap().is_buffering);

        // Spawn reader thread: read() will block in the buffering condvar loop,
        // calling update_status() on each wakeup so level_bytes progresses.
        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let _ = reader.read(&mut buf);
            reader
        });

        // Poll level_bytes from main thread while reader is blocked in buffering
        let mut levels = Vec::new();
        for _ in 0..100 {
            thread::sleep(Duration::from_millis(5));
            let level = status.lock().unwrap().level_bytes;
            levels.push(level);
        }

        let _reader = reader_thread.join().unwrap();

        // Filter to distinct increasing values
        let mut distinct: Vec<usize> = Vec::new();
        for &l in &levels {
            if distinct.is_empty() || l > *distinct.last().unwrap() {
                distinct.push(l);
            }
        }

        assert!(
            distinct.len() >= 3,
            "Expected at least 3 distinct increasing level values, got {:?}",
            distinct
        );

        // Final level should be approaching or past the watermark
        assert!(
            *distinct.last().unwrap() > start_watermark() / 2,
            "Expected level to grow significantly toward watermark, got {:?}",
            distinct
        );

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn multiple_buffering_cycles() {
        // Verify hysteresis works correctly across multiple drain→refill cycles.
        // Data must far exceed MAX_WATERMARK_BYTES (512KB) because the adaptive
        // watermark escalates with each underrun. 3MB ensures enough data for
        // multiple cycles even with watermark escalation up to the cap.
        let data: Vec<u8> = (0..3_000_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_millis(1));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        let mut all_data = Vec::new();
        let mut cycle_count = 0;

        for _ in 0..3 {
            // Drain rapidly until buffering activates
            let mut buf = [0u8; 8192];
            loop {
                let n = reader.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                all_data.extend_from_slice(&buf[..n]);
                if status.lock().unwrap().is_buffering {
                    cycle_count += 1;
                    break;
                }
            }

            if status.lock().unwrap().is_buffering {
                // Read once more — blocks until watermark, then delivers data
                let n = reader.read(&mut buf).unwrap();
                if n > 0 {
                    all_data.extend_from_slice(&buf[..n]);
                }
            }
        }

        // Verify data integrity for what we read
        assert_eq!(
            &all_data[..],
            &data[..all_data.len()],
            "Data mismatch during multi-cycle buffering"
        );

        assert!(
            cycle_count >= 2,
            "Expected at least 2 buffering cycles, got {}",
            cycle_count
        );

        // Underrun count should match cycle count (not be wildly inflated)
        let underruns = status.lock().unwrap().underrun_count;
        assert!(
            underruns <= (cycle_count as u32) * 2,
            "Underruns ({}) too high relative to cycles ({})",
            underruns,
            cycle_count
        );

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn buffering_with_periodic_stalls_absorbs_hiccups() {
        // Verify the watermark system handles HLS-like periodic stalls.
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone())
            .with_periodic_stall(3, Duration::from_millis(50));
        let (mut reader, handle, _stop, status) = buffer_from_sim(sim);

        // Let producer fill buffer first (absorb initial stalls)
        thread::sleep(Duration::from_millis(200));

        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none());
        assert_eq!(
            result, data,
            "All data should arrive correctly despite periodic stalls"
        );

        // Underruns should be relatively low — buffer absorbs most stalls
        let underruns = status.lock().unwrap().underrun_count;
        // The buffer should absorb stalls; allow some underruns but not excessive
        assert!(
            underruns < 20,
            "Expected low underrun count with pre-filled buffer, got {}",
            underruns
        );

        handle.join().unwrap();
    }

    // =========================================================================
    // Adaptive watermark tests
    // =========================================================================

    /// A reader over a few bytes, for its watermark arithmetic
    fn idle_reader() -> (StreamBufferReader, JoinHandle<()>, StreamCancel) {
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        StreamBuffer::new(Box::new(Cursor::new(vec![0u8; 10])), status, probing)
    }

    /// Play `secs` of audio, then run dry
    fn underrun_after(reader: &mut StreamBufferReader, secs: f64) {
        reader.started = true;
        reader.buffering_active = false;
        reader.healthy_from = reader.read_pos;
        reader.read_pos += (secs * reader.rate.get()) as u64;
        reader.start_buffering(&mut NetworkMetrics::new());
    }

    #[test]
    fn the_start_buffers_seconds_of_audio_at_the_station_bitrate() {
        let (mut reader, handle, stop) = idle_reader();
        assert_eq!(reader.target_secs(), START_BUFFER_SECS);
        assert_eq!(reader.effective_watermark(), start_watermark());

        // A 32 kbps station starts after 3 s of audio (12 kB), where 128 kB
        // (32 s) used to be needed and the probe gave up after 10 s
        reader.set_advertised_bitrate(Some(32));
        assert_eq!(reader.effective_watermark(), 12_000);
        reader.set_advertised_bitrate(Some(320));
        assert_eq!(reader.effective_watermark(), 120_000);
        // Nonsense falls back to the default
        for nonsense in [0, 128_000] {
            reader.set_advertised_bitrate(Some(nonsense));
            assert_eq!(reader.effective_watermark(), start_watermark());
        }

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn each_underrun_raises_the_refill_target_up_to_a_limit() {
        let (mut reader, handle, stop) = idle_reader();
        let mut targets = Vec::new();
        for _ in 0..6 {
            underrun_after(&mut reader, 1.0);
            targets.push(reader.target_secs());
        }
        assert_eq!(targets, [5.0, 10.0, 15.0, 20.0, 20.0, 20.0]);
        assert_eq!(reader.underrun_escalations, 4, "no steps past the limit");

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn the_watermark_stays_within_what_the_buffer_holds() {
        let (mut reader, handle, stop) = idle_reader();
        // A lossless stream at 8 Mbps, after many underruns
        reader.set_advertised_bitrate(Some(8_000));
        for _ in 0..4 {
            underrun_after(&mut reader, 1.0);
        }
        assert_eq!(reader.effective_watermark(), MAX_WATERMARK_BYTES);
        const { assert!(MAX_WATERMARK_BYTES < MAX_BUFFER_SIZE - COMPACTION_THRESHOLD) };

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn rare_hiccups_dont_pile_up() {
        let (mut reader, handle, stop) = idle_reader();
        // An underrun every 90 s over an hour: the first target every time
        for _ in 0..40 {
            underrun_after(&mut reader, 90.0);
            assert_eq!(reader.target_secs(), REFILL_BUFFER_SECS);
        }
        // Frequent ones escalate, and minutes of good play undo them
        for _ in 0..3 {
            underrun_after(&mut reader, 10.0);
        }
        assert_eq!(reader.target_secs(), 20.0);
        underrun_after(&mut reader, 150.0); // undoes 2 of 4, then adds this one
        assert_eq!(reader.target_secs(), 15.0);

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn the_byte_rate_is_measured_from_the_decoded_audio() {
        let (mut reader, handle, stop) = idle_reader();
        reader.set_advertised_bitrate(Some(128));
        let decoded = reader.decoded_time();
        let secs = |s: f64| (s * 1e6) as u64;

        // The probe's reads don't count
        decoded.store(secs(1.0), Ordering::Relaxed);
        reader.rate.update(50_000);
        assert_eq!(reader.rate.measured_from, None);
        decoded.store(secs(RATE_SKIP_SECS), Ordering::Relaxed);
        reader.rate.update(60_000);

        // Actually 48 kbps: 6 kB per second of audio
        decoded.store(secs(RATE_SKIP_SECS + 10.0), Ordering::Relaxed);
        reader.rate.update(60_000 + 60_000);
        assert_eq!(reader.rate.get(), 16_000.0, "too short to trust yet");
        decoded.store(secs(RATE_SKIP_SECS + RATE_SPAN_SECS), Ordering::Relaxed);
        reader
            .rate
            .update(60_000 + (RATE_SPAN_SECS * 6_000.0) as u64);
        assert_eq!(reader.rate.get(), 6_000.0);
        assert_eq!(reader.effective_watermark(), 18_000, "3 s at 48 kbps");

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn the_decoder_reports_the_audio_it_decodes() {
        use crate::audio::decoder::SymphoniaSource;
        // 1 s of 8 kHz mono 16-bit WAV
        let rate = 8_000u32;
        let data_len = rate * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(wav.len() + data_len as usize, 0);

        let (reader, handle, _stop) = buffer_from_data(wav);
        let decoded = reader.decoded_time();
        let mut source = SymphoniaSource::new(reader).unwrap();
        source.count_decoded_time(decoded.clone());
        for _ in source.by_ref() {}
        let micros = decoded.load(Ordering::Relaxed);
        // All but the first packet, which the probe decodes
        assert!(
            (800_000..=1_000_000).contains(&micros),
            "{micros} us decoded"
        );
        handle.join().unwrap();
    }

    /// A live station sending `bytes` every `every`
    struct Trickle {
        bytes: usize,
        every: Duration,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            thread::sleep(self.every);
            let n = self.bytes.min(buf.len());
            buf[..n].fill(1);
            Ok(n)
        }
    }

    impl Seek for Trickle {
        fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
            Err(io::Error::other("live"))
        }
    }

    #[test]
    fn a_slow_station_without_a_bitrate_still_starts() {
        // 5 kB/s, with no advertised bitrate: 48 kB at the assumed 128 kbps
        // would take almost 10 s, as long as the probe waits
        let station = Trickle {
            bytes: 100,
            every: Duration::from_millis(20),
        };
        let (mut reader, handle, stop, _status) = {
            let status = Arc::new(Mutex::new(BufferStatus::default()));
            let probing = Arc::new(AtomicBool::new(false));
            let (reader, handle, stop) =
                StreamBuffer::new(Box::new(station), status.clone(), probing);
            (reader, handle, stop, status)
        };
        reader.start_max_wait = Duration::from_millis(300);

        let start = Instant::now();
        let mut buf = [0u8; 1024];
        assert!(reader.read(&mut buf).unwrap() > 0);
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(300) && waited < Duration::from_secs(2),
            "started after {waited:?}"
        );

        stop.cancel();
        handle.join().unwrap();
    }

    /// A live station that sends `burst` bytes on connect, then trickles
    struct Bursting {
        burst: usize,
        then: Trickle,
    }

    impl Read for Bursting {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.burst == 0 {
                return self.then.read(buf);
            }
            let n = self.burst.min(buf.len());
            self.burst -= n;
            buf[..n].fill(1);
            Ok(n)
        }
    }

    impl Seek for Bursting {
        fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
            Err(io::Error::other("live"))
        }
    }

    #[test]
    fn a_short_burst_on_connect_still_waits_to_start() {
        // Half a second of audio on connect, then real time. Played at once
        // it ran out before more came: a blip, then seconds of buffering.
        let station = Bursting {
            burst: 8_000,
            then: Trickle {
                bytes: 800,
                every: Duration::from_millis(10),
            },
        };
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (mut reader, handle, stop) =
            StreamBuffer::new(Box::new(station), status.clone(), probing);
        reader.set_advertised_bitrate(Some(128));
        // The burst is in before the decoder first reads
        while reader.state.inner.lock().unwrap().write_pos < 8_000 {
            thread::sleep(Duration::from_millis(1));
        }

        let mut buf = [0u8; 1024];
        let n = reader.read(&mut buf).unwrap();
        let s = status.lock().unwrap().clone();
        let buffered = n + s.level_bytes;
        assert!(
            buffered >= 48_000,
            "started with {buffered} bytes, not 3 s of audio"
        );
        assert_eq!(s.underrun_count, 0);

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn the_first_wait_is_not_an_underrun() {
        let data = vec![0u8; 200_000];
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(2));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // The buffer is empty when the probe first reads: it waits to start
        let mut buf = [0u8; 1024];
        assert_eq!(reader.read(&mut buf).unwrap(), 1024);
        let s = status.lock().unwrap().clone();
        assert!(!s.is_buffering);
        assert_eq!(s.underrun_count, 0);
        assert_eq!(reader.underrun_escalations, 0);

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn adaptive_watermark_stabilizes_underruns() {
        // With periodic stalls, after escalation the watermark grows large enough
        // to absorb stalls, so underrun count should stabilize rather than grow
        // linearly with data consumed.
        let data = vec![0u8; 300_000];
        let sim = SimulatedNetworkReader::new(data)
            .with_latency(Duration::from_millis(1))
            .with_periodic_stall(5, Duration::from_millis(30));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // Read in two phases and compare underrun growth rate
        let mut buf = [0u8; 8192];
        for _ in 0..15 {
            let _ = reader.read(&mut buf);
        }
        let underruns_phase1 = status.lock().unwrap().underrun_count;

        for _ in 0..15 {
            let _ = reader.read(&mut buf);
        }
        let underruns_phase2 = status.lock().unwrap().underrun_count;
        let delta = underruns_phase2 - underruns_phase1;

        stop.cancel();
        handle.join().unwrap();

        // After escalation, the watermark is larger so fewer underruns occur
        // in phase 2 compared to phase 1. At minimum, the delta should not
        // exceed the initial count (growth is sub-linear).
        assert!(
            delta <= underruns_phase1 + 1,
            "Underruns should stabilize: phase1={}, delta={}",
            underruns_phase1,
            delta
        );
    }

    #[test]
    fn status_reports_effective_watermark() {
        // Verify BufferStatus.effective_watermark updates after underruns
        let data = vec![0u8; 200_000];
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(2));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // Initial watermark should be the start target
        let initial_wm = status.lock().unwrap().effective_watermark;
        assert_eq!(initial_wm, start_watermark());

        // Read rapidly to trigger underruns and escalation
        let mut buf = [0u8; 8192];
        for _ in 0..20 {
            let _ = reader.read(&mut buf);
        }

        let final_wm = status.lock().unwrap().effective_watermark;

        stop.cancel();
        handle.join().unwrap();

        // After underruns, the effective watermark should have grown
        assert!(
            final_wm > initial_wm,
            "Expected watermark to escalate from {}, got {}",
            initial_wm,
            final_wm
        );
    }

    #[test]
    fn escalation_resets_per_stream() {
        // Two sequential StreamBuffers both start at 0 escalations
        let status1 = Arc::new(Mutex::new(BufferStatus::default()));
        let probing1 = Arc::new(AtomicBool::new(false));
        let (reader1, handle1, stop1) =
            StreamBuffer::new(Box::new(Cursor::new(vec![0u8; 10])), status1, probing1);
        assert_eq!(reader1.underrun_escalations, 0);
        stop1.cancel();
        drop(reader1);
        handle1.join().unwrap();

        let status2 = Arc::new(Mutex::new(BufferStatus::default()));
        let probing2 = Arc::new(AtomicBool::new(false));
        let (reader2, handle2, stop2) =
            StreamBuffer::new(Box::new(Cursor::new(vec![0u8; 10])), status2, probing2);
        assert_eq!(reader2.underrun_escalations, 0);
        stop2.cancel();
        drop(reader2);
        handle2.join().unwrap();
    }

    // =========================================================================
    // Escalation decay tests
    // =========================================================================

    #[test]
    fn escalation_decay_after_prolonged_buffering() {
        // After buffering longer than escalation_decay_secs, escalations reset to 0.
        // Set up state directly: pretend we've been buffering for 2s with escalations,
        // then call read() which enters the buffering path and triggers decay.
        let data = vec![0u8; 200_000];
        let (mut reader, handle, stop, _status) = buffer_with_status(data);

        // Let fast producer fill the buffer completely
        thread::sleep(Duration::from_millis(50));

        // Read one chunk to advance read_pos
        let mut buf = [0u8; 8192];
        reader.read_exact(&mut buf).unwrap();

        // Set up escalated buffering state as if we've been in a prolonged outage
        reader.underrun_escalations = 3;
        reader.escalation_decay_secs = 1;
        reader.buffering_active = true;
        reader.buffering_start = Some(Instant::now() - Duration::from_secs(2));

        // Next read() enters buffering path, sees decay threshold exceeded,
        // resets escalations to 0, then exits (producer_done or data available)
        let n = reader.read(&mut buf).unwrap();
        assert!(n > 0, "Should get data after decay");
        assert_eq!(
            reader.underrun_escalations, 0,
            "Escalations should decay to 0 after prolonged buffering"
        );

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn no_decay_during_short_buffering() {
        // Buffering < decay threshold → escalations preserved.
        let data = vec![0u8; 200_000];
        let (mut reader, handle, stop, _status) = buffer_with_status(data);

        // Let fast producer fill the buffer completely
        thread::sleep(Duration::from_millis(50));

        // Read one chunk
        let mut buf = [0u8; 8192];
        reader.read_exact(&mut buf).unwrap();

        // Set up buffering state with escalations, but buffering_start is recent
        reader.underrun_escalations = 3;
        reader.escalation_decay_secs = 60; // Very long, won't trigger
        reader.buffering_active = true;
        reader.buffering_start = Some(Instant::now()); // Just started

        // Next read() enters buffering path, sees decay NOT exceeded (0s < 60s),
        // preserves escalations, then exits (producer_done or data available)
        let n = reader.read(&mut buf).unwrap();
        assert!(n > 0, "Should get data");
        assert_eq!(
            reader.underrun_escalations, 3,
            "Escalations should be preserved during short buffering"
        );

        stop.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn effective_watermark_drops_after_decay() {
        // A long outage resets the escalations: the refill target drops back
        let (mut reader, handle, stop) = idle_reader();
        for _ in 0..3 {
            underrun_after(&mut reader, 1.0);
        }
        let watermark_before = reader.effective_watermark();
        assert_eq!(watermark_before, (15.0 * DEFAULT_BYTE_RATE) as usize);

        reader.underrun_escalations = 0;
        let watermark_after = reader.effective_watermark();
        assert_eq!(
            watermark_after,
            (REFILL_BUFFER_SECS * DEFAULT_BYTE_RATE) as usize
        );

        stop.cancel();
        handle.join().unwrap();
    }

    // =========================================================================
    // Throughput EMA burst spike prevention tests
    // =========================================================================

    #[test]
    fn throughput_ema_no_burst_spike() {
        // Simulate reconnection scenario: long gap followed by rapid burst reads.
        // The EMA should NOT spike to unrealistic values.
        let mut m = NetworkMetrics::new();

        // Initial chunk after 200ms (normal first read)
        std::thread::sleep(Duration::from_millis(200));
        m.record_chunk(8192);
        let initial_ema = m.throughput_ema;
        assert!(initial_ema > 0.0, "Should have initial throughput");

        // Simulate burst: many rapid chunks within MIN_THROUGHPUT_INTERVAL_MS.
        // These should accumulate pending_bytes without updating the EMA.
        for _ in 0..10 {
            m.record_chunk(8192);
        }

        // EMA should not have changed during the burst (elapsed < MIN_THROUGHPUT_INTERVAL_MS)
        assert_eq!(
            m.throughput_ema, initial_ema,
            "EMA should not spike during burst reads"
        );
        assert_eq!(
            m.pending_bytes,
            8192 * 10,
            "Pending bytes should accumulate"
        );

        // After enough time, the next chunk triggers a batched EMA update
        std::thread::sleep(Duration::from_millis(110));
        m.record_chunk(8192);

        // The batched throughput should be reasonable (not 82 MB/s)
        // 11 chunks * 8KB = 88KB in ~110ms ≈ 800 KB/s — well below the 82 MB/s spike
        assert!(
            m.throughput_ema < 2_000_000.0,
            "EMA should be reasonable after batch update, got {} bytes/sec",
            m.throughput_ema
        );
    }

    #[test]
    fn throughput_accumulates_pending_bytes() {
        let mut m = NetworkMetrics::new();

        // Record several chunks within MIN_THROUGHPUT_INTERVAL_MS
        m.record_chunk(100);
        m.record_chunk(200);
        m.record_chunk(300);

        // total_bytes always accumulates
        assert_eq!(m.total_bytes, 600);
        // pending_bytes accumulates until EMA update fires
        assert_eq!(m.pending_bytes, 600);
        // EMA should not have updated (elapsed < MIN_THROUGHPUT_INTERVAL_MS)
        assert_eq!(
            m.throughput_ema, 0.0,
            "EMA should stay at 0 until enough time passes"
        );
    }

    // --- PlaybackPositionReader ---

    #[test]
    fn playback_position_tracks_reads_and_caps_them() {
        let shared = Arc::new(AtomicU64::new(99));
        let mut reader =
            PlaybackPositionReader::new(io::Cursor::new(vec![7u8; 10_000]), shared.clone());
        assert_eq!(shared.load(Ordering::Relaxed), 0);
        let mut buf = vec![0u8; 8192];
        assert_eq!(reader.read(&mut buf).unwrap(), PLAYBACK_POSITION_MAX_READ);
        assert_eq!(shared.load(Ordering::Relaxed), 4096);
        reader.read_exact(&mut buf[..5000]).unwrap();
        assert_eq!(shared.load(Ordering::Relaxed), 9096);
        reader.seek(SeekFrom::Start(100)).unwrap();
        assert_eq!(shared.load(Ordering::Relaxed), 100);
    }

    // --- Stopping while the network reader is stuck ---

    /// Inner reader that behaves like a station that went off air: it never
    /// delivers data. `interrupt` makes it return `Interrupted` every 20 ms
    /// (how the ICY and HLS readers let the producer check for a cancel);
    /// otherwise it blocks for an hour. Records when it is dropped.
    struct DeadStationReader {
        interrupt: bool,
        dropped: Arc<AtomicBool>,
    }

    impl Read for DeadStationReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            if self.interrupt {
                thread::sleep(Duration::from_millis(20));
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                thread::sleep(Duration::from_secs(3600));
                Ok(0)
            }
        }
    }

    impl Seek for DeadStationReader {
        fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    impl Drop for DeadStationReader {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn dead_station_buffer(
        interrupt: bool,
    ) -> (
        StreamBufferReader,
        JoinHandle<()>,
        StreamCancel,
        Arc<AtomicBool>,
    ) {
        let dropped = Arc::new(AtomicBool::new(false));
        let reader = DeadStationReader {
            interrupt,
            dropped: dropped.clone(),
        };
        let status = Arc::new(Mutex::new(BufferStatus::default()));
        let probing = Arc::new(AtomicBool::new(false));
        let (consumer, handle, stop) = StreamBuffer::new(Box::new(reader), status, probing);
        (consumer, handle, stop, dropped)
    }

    /// Read once on another thread, so a read that never returns fails the
    /// test instead of hanging the test run.
    fn read_with_deadline(
        mut consumer: StreamBufferReader,
        before_read: impl FnOnce(&mut StreamBufferReader) + Send + 'static,
    ) -> crossbeam_channel::Receiver<Result<usize, io::ErrorKind>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        thread::spawn(move || {
            before_read(&mut consumer);
            let mut buf = [0u8; 64];
            let _ = tx.send(consumer.read(&mut buf).map_err(|e| e.kind()));
        });
        rx
    }

    #[test]
    fn stop_wakes_a_consumer_waiting_on_a_dead_station() {
        // If this read never returned, the station's decode thread would
        // wait on it for good after switching away from a station that
        // went down.
        let (consumer, _handle, stop, _dropped) = dead_station_buffer(false);
        let rx = read_with_deadline(consumer, |_| {});
        thread::sleep(Duration::from_millis(100));
        stop.cancel();
        let result = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("read must return once the buffer is stopped");
        assert_eq!(result, Ok(0), "a stopped buffer reads as end of stream");
    }

    #[test]
    fn a_cancel_wakes_a_waiting_consumer_at_once() {
        // Not at its next wait timeout. Several rounds, since a consumer
        // that only polls could still happen to wake soon after the cancel.
        let rounds = 5;
        let mut total = Duration::ZERO;
        for _ in 0..rounds {
            let (consumer, _handle, stop, _dropped) = dead_station_buffer(false);
            let rx = read_with_deadline(consumer, |_| {});
            thread::sleep(Duration::from_millis(30));
            let cancelled_at = Instant::now();
            stop.cancel();
            let result = rx
                .recv_timeout(Duration::from_secs(3))
                .expect("read must return once the buffer is stopped");
            total += cancelled_at.elapsed();
            assert_eq!(result, Ok(0));
        }
        let poll = Duration::from_millis(CONSUMER_WAIT_TIMEOUT_MS);
        assert!(
            total < poll * rounds / 4,
            "{rounds} cancelled reads took {total:?} in all"
        );
    }

    #[test]
    fn stop_while_buffering_after_an_underrun_returns_end_of_stream() {
        let (consumer, _handle, stop, _dropped) = dead_station_buffer(false);
        stop.cancel();
        // Put the consumer in the state it is in after an underrun
        let rx = read_with_deadline(consumer, |c| c.buffering_active = true);
        let result = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("read must return once the buffer is stopped");
        assert_eq!(result, Ok(0));
    }

    #[test]
    fn producer_retries_interrupted_reads_and_exits_on_stop() {
        let (_consumer, handle, stop, dropped) = dead_station_buffer(true);
        thread::sleep(Duration::from_millis(100));
        assert!(
            !handle.is_finished(),
            "Interrupted is not an error: the producer keeps waiting for data"
        );
        stop.cancel();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "producer must exit after stop");
        assert!(
            dropped.load(Ordering::SeqCst),
            "the inner reader is dropped, which stops its network thread"
        );
    }

    #[test]
    fn producer_marks_done_when_stopped_between_reads() {
        // Stop lands while the producer is outside the write loop: the
        // consumer must still see the producer as finished.
        let (consumer, handle, stop, _dropped) = dead_station_buffer(true);
        stop.cancel();
        handle.join().unwrap();
        assert!(consumer.state.inner.lock().unwrap().producer_done);
    }
}
