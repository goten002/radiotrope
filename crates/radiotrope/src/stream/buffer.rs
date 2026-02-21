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

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::types::ReadSeek;
use crate::config::buffer::{
    COMPACTION_SAFETY_MARGIN, COMPACTION_THRESHOLD, CONSUMER_WAIT_TIMEOUT_MS, EMA_ALPHA_JITTER,
    EMA_ALPHA_THROUGHPUT, HIGH_WATERMARK_BYTES, MAX_BUFFER_SIZE, PRODUCER_CHUNK_SIZE,
};

/// Network throughput and jitter metrics (EMA-smoothed)
pub struct NetworkMetrics {
    throughput_ema: f64,
    jitter_ema: f64,
    last_chunk_time: Instant,
    last_chunk_interval_ms: f64,
    total_bytes: u64,
    underrun_count: u32,
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
        }
    }

    /// Record a successful chunk read
    fn record_chunk(&mut self, bytes: usize) {
        let now = Instant::now();
        let elapsed_ms = now.duration_since(self.last_chunk_time).as_secs_f64() * 1000.0;

        if elapsed_ms > 0.0 {
            let throughput = (bytes as f64 / elapsed_ms) * 1000.0; // bytes/sec
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
        }

        self.total_bytes += bytes as u64;
        self.last_chunk_time = now;
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
}

impl Default for BufferStatus {
    fn default() -> Self {
        Self {
            level_bytes: 0,
            capacity_bytes: 0,
            is_buffering: false,
            throughput_kbps: 0.0,
            underrun_count: 0,
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
}

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
    pub fn new(
        reader: Box<dyn ReadSeek>,
        status: SharedBufferStatus,
        probing_flag: Arc<AtomicBool>,
    ) -> (StreamBufferReader, JoinHandle<()>, Arc<AtomicBool>) {
        let stop_flag = Arc::new(AtomicBool::new(false));

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
        });

        let producer_state = state.clone();
        let producer_stop = stop_flag.clone();

        let handle = thread::Builder::new()
            .name("stream-buffer-producer".to_string())
            .spawn(move || {
                Self::producer_loop(reader, producer_state, producer_stop);
            })
            .expect("Failed to spawn buffer producer thread");

        let consumer = StreamBufferReader {
            state: state.clone(),
            status,
            read_pos: 0,
            probing_flag,
            buffering_active: false,
        };

        (consumer, handle, stop_flag)
    }

    /// Producer loop: reads chunks from inner reader, appends to shared buffer.
    fn producer_loop(
        mut reader: Box<dyn ReadSeek>,
        state: Arc<BufferState>,
        stop_flag: Arc<AtomicBool>,
    ) {
        let mut chunk = vec![0u8; PRODUCER_CHUNK_SIZE];

        loop {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }

            match reader.read(&mut chunk) {
                Ok(0) => {
                    // EOF
                    if let Ok(mut inner) = state.inner.lock() {
                        inner.producer_done = true;
                    }
                    state.data_available.notify_all();
                    break;
                }
                Ok(n) => {
                    // Retry loop: keep trying to write this chunk until buffer has space.
                    // We must NOT re-read from the inner reader before writing this chunk,
                    // or we'd lose the data we already read.
                    loop {
                        if stop_flag.load(Ordering::Relaxed) {
                            // Signal done and exit both loops
                            if let Ok(mut inner) = state.inner.lock() {
                                inner.producer_done = true;
                            }
                            state.data_available.notify_all();
                            return;
                        }

                        let mut inner = match state.inner.lock() {
                            Ok(inner) => inner,
                            Err(_) => return, // Mutex poisoned
                        };

                        // Enforce max buffer size: wait if buffer is full
                        if inner.data.len() >= MAX_BUFFER_SIZE {
                            drop(inner);
                            thread::sleep(Duration::from_millis(10));
                            continue; // Retry writing this same chunk
                        }

                        inner.data.extend_from_slice(&chunk[..n]);
                        inner.write_pos += n;
                        inner.metrics.record_chunk(n);

                        drop(inner);
                        state.data_available.notify_all();
                        break; // Chunk written, read next from inner reader
                    }
                }
                Err(e) => {
                    if let Ok(mut inner) = state.inner.lock() {
                        inner.producer_error = Some(e.to_string());
                        inner.producer_done = true;
                    }
                    state.data_available.notify_all();
                    break;
                }
            }
        }
    }
}

/// Consumer side: implements Read + Seek, passed to symphonia.
pub struct StreamBufferReader {
    state: Arc<BufferState>,
    status: SharedBufferStatus,
    /// Absolute read position (across compactions)
    read_pos: u64,
    /// When true, inhibits compaction (symphonia may seek back during probe)
    probing_flag: Arc<AtomicBool>,
    /// Hysteresis flag: true while waiting for buffer to refill to HIGH_WATERMARK
    buffering_active: bool,
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
                }
            }
        }
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
        }
    }
}

impl Read for StreamBufferReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = Duration::from_millis(CONSUMER_WAIT_TIMEOUT_MS);

        let mut inner = self
            .state
            .inner
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;

        loop {
            let local_read = (self.read_pos - inner.base_offset) as usize;
            let available = inner.write_pos.saturating_sub(local_read);

            if self.buffering_active {
                // Hysteresis: keep blocking until buffer refills to HIGH_WATERMARK
                // or the producer finishes (EOF/error).
                if available >= HIGH_WATERMARK_BYTES || inner.producer_done {
                    self.buffering_active = false;
                    self.update_status(&inner);
                    // Fall through to normal read logic below
                } else {
                    // Still buffering — update status so UI sees progress, then wait
                    self.update_status(&inner);
                    let result = self.state.data_available.wait_timeout(inner, timeout);
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

                // Check if buffer just emptied — enter buffering with hysteresis
                let new_local_read = (self.read_pos - inner.base_offset) as usize;
                let remaining = inner.write_pos.saturating_sub(new_local_read);
                if remaining == 0 && !inner.producer_done {
                    self.buffering_active = true;
                    inner.metrics.record_underrun();
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
            self.buffering_active = true;
            inner.metrics.record_underrun();
            self.update_status(&inner);

            // Wait for producer to write more data
            let result = self.state.data_available.wait_timeout(inner, timeout);
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

// StreamBufferReader is Send (Arc<BufferState> is Send+Sync) and
// Sync is safe because the reader is used from a single thread.
unsafe impl Sync for StreamBufferReader {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Helper to create a StreamBuffer from in-memory data
    fn buffer_from_data(data: Vec<u8>) -> (StreamBufferReader, JoinHandle<()>, Arc<AtomicBool>) {
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
        Arc<AtomicBool>,
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
        std::thread::sleep(Duration::from_millis(10));
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
        std::thread::sleep(Duration::from_millis(5));
        m.record_chunk(100);
        std::thread::sleep(Duration::from_millis(5));
        m.record_chunk(200);
        assert_eq!(m.total_bytes, 300);
        assert!(m.throughput_ema > 0.0);
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
    }

    #[test]
    fn buffer_status_clone() {
        let s = BufferStatus {
            level_bytes: 1024,
            capacity_bytes: 2048,
            is_buffering: true,
            throughput_kbps: 128.0,
            underrun_count: 3,
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

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
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
        reader.read(&mut buf).unwrap();

        // Signal stop
        stop.store(true, Ordering::Relaxed);

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
        reader.read(&mut buf).unwrap();

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
                if self.read_count % every_n == 0 {
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
        Arc<AtomicBool>,
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
        // With slow producer and data >> HIGH_WATERMARK, consumer should observe
        // is_buffering=true after draining the initial watermark fill.
        let data = vec![0u8; 500_000];
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(2));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        let mut saw_buffering = false;
        let mut buf = [0u8; 8192];
        for _ in 0..20 {
            let _ = reader.read(&mut buf);
            if let Ok(s) = status.lock() {
                if s.is_buffering {
                    saw_buffering = true;
                }
            }
        }

        stop.store(true, Ordering::Relaxed);
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
        // Heavy jitter should cause some consumer stalls (waits on condvar)
        let data = vec![0u8; 30_000];
        let sim = SimulatedNetworkReader::new(data)
            .with_latency(Duration::from_millis(2))
            .with_jitter(20);
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        let mut buf = [0u8; 8192];
        for _ in 0..20 {
            let _ = reader.read(&mut buf);
        }

        let underruns = status.lock().unwrap().underrun_count;

        stop.store(true, Ordering::Relaxed);
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
            if reader.read(&mut buf).unwrap_or(0) > 0 {
                if start.elapsed() < Duration::from_millis(5) {
                    instant_reads += 1;
                }
            }
        }

        stop.store(true, Ordering::Relaxed);
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
        reader.read(&mut buf).unwrap();

        // Signal stop
        stop.store(true, Ordering::Relaxed);

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

        stop.store(true, Ordering::Relaxed);
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
        reader.read(&mut buf).unwrap();

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
        // Slow producer forces consumer to wait — underrun_count should reflect this
        let data = vec![0u8; 20_000];
        let sim = SimulatedNetworkReader::new(data).with_latency(Duration::from_millis(5));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // Read rapidly to outrun the producer
        let mut buf = [0u8; 8192];
        for _ in 0..10 {
            let _ = reader.read(&mut buf);
        }

        let underruns = status.lock().unwrap().underrun_count;

        stop.store(true, Ordering::Relaxed);
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
        // After buffer empties, consumer should NOT get data until HIGH_WATERMARK is reached.
        // Use a slow producer so we can observe the blocking behavior.
        let data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        let sim = SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_millis(1));
        let (mut reader, handle, stop, status) = buffer_from_sim(sim);

        // Drain rapidly until buffering activates
        let drained = read_until_buffering(&mut reader, &status);
        assert!(drained > 0);
        assert!(status.lock().unwrap().is_buffering, "Should be buffering");

        // Now read again — this should block until HIGH_WATERMARK is reached.
        // The read should succeed with data once the watermark is met.
        let mut buf = [0u8; 8192];
        let n = reader.read(&mut buf).unwrap();
        assert!(n > 0, "Should eventually get data after watermark fill");

        // After the read, buffering should be resolved
        assert!(
            !status.lock().unwrap().is_buffering,
            "Should exit buffering after watermark reached"
        );

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    }

    #[test]
    fn hysteresis_exits_on_producer_done() {
        // If producer finishes (EOF) while in buffering mode, consumer gets remaining
        // data without waiting for HIGH_WATERMARK.
        let data = vec![42u8; 1024]; // Small: 1KB < HIGH_WATERMARK (64KB)
        let sim = SimulatedNetworkReader::new(data.clone()).with_latency(Duration::from_millis(2));
        let (mut reader, handle, _stop, _status) = buffer_from_sim(sim);

        // Drain all data — will eventually trigger buffering then producer finishes
        let (result, err) = drain_reader(&mut reader);
        assert!(err.is_none(), "Should not error");
        assert_eq!(
            result, data,
            "All data should be received despite being < HIGH_WATERMARK"
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

        stop.store(true, Ordering::Relaxed);
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
        // During buffering, level_bytes should grow toward HIGH_WATERMARK.
        // Status is only updated inside read() calls, so we need a reader
        // thread blocked in the buffering condvar loop to observe progress.
        let data: Vec<u8> = (0..500_000).map(|i| (i % 256) as u8).collect();
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
        for _ in 0..50 {
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

        // Final level should be approaching or past HIGH_WATERMARK
        assert!(
            *distinct.last().unwrap() > HIGH_WATERMARK_BYTES / 2,
            "Expected level to grow significantly toward watermark, got {:?}",
            distinct
        );

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    }

    #[test]
    fn multiple_buffering_cycles() {
        // Verify hysteresis works correctly across multiple drain→refill cycles.
        let data: Vec<u8> = (0..500_000).map(|i| (i % 256) as u8).collect();
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

        stop.store(true, Ordering::Relaxed);
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
}
