//! Configuration constants for the radiotrope engine

/// Audio-related configuration
pub mod audio {
    /// FFT window size for visualization
    pub const FFT_SIZE: usize = 512;

    /// Number of frequency bands in spectrum display
    pub const SPECTRUM_BANDS: usize = 16;

    /// VU meter decay factor (0.0-1.0, higher = slower decay)
    pub const VU_DECAY: f32 = 0.7;
}

/// Network-related configuration
pub mod network {
    /// User agent for HTTP requests
    pub const USER_AGENT: &str = concat!("Radiotrope/", env!("CARGO_PKG_VERSION"));

    /// Connection timeout in seconds
    pub const CONNECT_TIMEOUT_SECS: u64 = 10;

    /// Read timeout in seconds
    pub const READ_TIMEOUT_SECS: u64 = 30;

    /// Maximum playlist resolution depth
    pub const MAX_PLAYLIST_DEPTH: usize = 5;
}

/// HLS-related configuration
pub mod hls {
    /// Number of segments to buffer
    pub const SEGMENT_BUFFER_SIZE: usize = 3;

    /// Segment download timeout in seconds
    pub const SEGMENT_TIMEOUT_SECS: u64 = 15;
}

/// Timeout configuration for resilience
pub mod timeouts {
    /// Maximum time to wait for format probe (symphonia) in seconds
    pub const PROBE_TIMEOUT_SECS: u64 = 10;

    /// Maximum time in buffering state before giving up in seconds
    pub const BUFFERING_TIMEOUT_SECS: u64 = 15;

    /// Time without receiving audio data before considering stream dead
    pub const STREAM_STALL_TIMEOUT_SECS: u64 = 5;

    /// Base delay between retries in seconds (exponential backoff: 2^n * base)
    pub const RETRY_BASE_DELAY_SECS: u64 = 2;

    /// Maximum backoff delay in seconds (cap for exponential backoff)
    pub const MAX_BACKOFF_SECS: u64 = 30;
}

/// Stream buffer configuration (producer-consumer architecture)
pub mod buffer {
    /// Maximum buffer size (bytes) — hard cap to prevent unbounded memory growth
    pub const MAX_BUFFER_SIZE: usize = 4 * 1024 * 1024;
    /// Compact buffer when consumed data exceeds this threshold (bytes)
    pub const COMPACTION_THRESHOLD: usize = 2 * 1024 * 1024;
    /// Keep this many bytes before read cursor on compaction (safety margin for seeks)
    pub const COMPACTION_SAFETY_MARGIN: usize = 64 * 1024;
    /// Chunk size for producer reads from inner reader (bytes)
    pub const PRODUCER_CHUNK_SIZE: usize = 8 * 1024;
    /// Maximum time consumer blocks waiting for data (milliseconds)
    pub const CONSUMER_WAIT_TIMEOUT_MS: u64 = 500;
    /// EMA smoothing factor for throughput (0.0–1.0)
    pub const EMA_ALPHA_THROUGHPUT: f64 = 0.3;
    /// EMA smoothing factor for jitter (0.0–1.0)
    pub const EMA_ALPHA_JITTER: f64 = 0.2;
    /// High watermark for buffering hysteresis (bytes).
    /// Once the buffer empties and enters buffering mode, the consumer blocks
    /// until the buffer refills to this level before delivering data again.
    /// 64KB provides ~2-4 seconds of buffer for typical radio streams (128-256 kbps).
    pub const HIGH_WATERMARK_BYTES: usize = 64 * 1024;
}
