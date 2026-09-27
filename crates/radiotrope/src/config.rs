//! Configuration constants for the radiotrope engine

/// Audio-related configuration
pub mod audio {
    /// FFT window size for visualization
    pub const FFT_SIZE: usize = 512;

    /// Number of frequency bands in spectrum display
    pub const SPECTRUM_BANDS: usize = 16;
    /// Highest frequency the spectrum bands cover (Hz)
    pub const SPECTRUM_MAX_HZ: f32 = 16_000.0;
    /// Extra gain for the top band, scaled down linearly to none at the bottom
    pub const SPECTRUM_TREBLE_BOOST: f32 = 3.0;

    /// Decibels the VU meter spans from empty to full. The scale follows
    /// each station's own loudness, so quiet and loud (heavily compressed)
    /// stations both swing across the meter.
    pub const VU_RANGE_DB: f32 = 15.0;
    /// How far above the station's typical loudness the meter tops out (dB)
    pub const VU_HEADROOM_DB: f32 = 4.0;
    /// Typical loudness assumed when a stream starts (dBFS RMS)
    pub const VU_START_DB: f32 = -20.0;
    /// Seconds the loudness reference takes to follow a louder station
    pub const VU_ADAPT_UP_SECS: f32 = 0.5;
    /// Seconds the loudness reference takes to follow a quieter station
    pub const VU_ADAPT_DOWN_SECS: f32 = 5.0;
    /// Blocks quieter than this (dBFS RMS) are silence and don't move the
    /// reference
    pub const VU_SILENCE_DB: f32 = -60.0;

    /// VU meter decay factor (0.0-1.0, higher = slower decay)
    pub const VU_DECAY: f32 = 0.7;

    /// How much of a rise the VU and spectrum levels take at once
    /// (0.0-1.0, higher = snappier); falls use [`VU_DECAY`]
    pub const VU_ATTACK: f32 = 0.8;
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

    /// How long to wait for the first segment before giving up, in seconds.
    /// Kept under the app's 15 s resolve timeout so the real reason is shown.
    pub const FIRST_SEGMENT_TIMEOUT_SECS: u64 = 12;
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
    pub const MAX_BACKOFF_SECS: u64 = 10;

    /// Connect timeout for reconnection attempts (seconds).
    /// Shorter than the initial connect timeout to speed up recovery.
    pub const CONNECT_TIMEOUT_SECS: u64 = 5;

    /// Buffering duration before the engine signals a stall to the UI (seconds).
    /// Short enough to give timely feedback, long enough to avoid false alarms
    /// from normal buffering events (HLS gaps, brief hiccups).
    pub const BUFFERING_STALL_THRESHOLD_SECS: u64 = 10;
}

/// Equalizer configuration
pub mod metadata {
    /// Longest a song change waits for playback to reach its position (seconds).
    /// A safety net in case playback position is not reported: covers the
    /// HLS segment queue plus the stream buffer.
    pub const MAX_SYNC_DELAY_SECS: u64 = 60;

    /// How often held-back song changes are checked against playback (ms).
    pub const SYNC_POLL_MS: u64 = 100;
}

pub mod eq {
    /// Number of EQ bands
    pub const NUM_BANDS: usize = 10;

    /// Center frequencies for each band (Hz)
    pub const CENTER_FREQUENCIES: [f32; 10] = [
        31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
    ];

    /// Display labels for each band
    pub const FREQ_LABELS: [&str; 10] = [
        "32", "64", "125", "250", "500", "1K", "2K", "4K", "8K", "16K",
    ];

    /// Minimum gain per band (dB)
    pub const MIN_GAIN_DB: f32 = -12.0;

    /// Maximum gain per band (dB)
    pub const MAX_GAIN_DB: f32 = 12.0;

    /// Default Q factor for peaking EQ filters
    pub const DEFAULT_Q: f32 = 1.414;
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
    /// Maximum time consumer blocks waiting for data before it re-checks the
    /// stop flag (milliseconds). Bounds how long a stop takes while the
    /// station delivers nothing.
    pub const CONSUMER_WAIT_TIMEOUT_MS: u64 = 100;
    /// EMA smoothing factor for throughput (0.0–1.0)
    pub const EMA_ALPHA_THROUGHPUT: f64 = 0.3;
    /// EMA smoothing factor for jitter (0.0–1.0)
    pub const EMA_ALPHA_JITTER: f64 = 0.2;
    /// High watermark for buffering hysteresis (bytes).
    /// Once the buffer empties and enters buffering mode, the consumer blocks
    /// until the buffer refills to this level before delivering data again.
    /// 64KB provides ~2-4 seconds of buffer for typical radio streams (128-256 kbps).
    pub const HIGH_WATERMARK_BYTES: usize = 64 * 1024;
    /// Escalation step per underrun (bytes) — each buffer underrun increases the
    /// effective watermark by this amount to prevent repeated buffering cycles.
    pub const WATERMARK_STEP_BYTES: usize = 64 * 1024;
    /// Maximum effective watermark (bytes) — caps escalation to prevent excessive
    /// buffering delay. 512KB covers ~32s at 128kbps, enough for max ICY backoff.
    pub const MAX_WATERMARK_BYTES: usize = 512 * 1024;
    /// Target buffer duration (seconds) — throughput-based floor for the effective
    /// watermark. Ensures at least this many seconds of audio are buffered based
    /// on the measured throughput EMA.
    pub const TARGET_BUFFER_SECONDS: f64 = 5.0;

    /// Minimum interval between throughput EMA updates (milliseconds).
    /// Prevents burst reads after reconnection from spiking the EMA.
    pub const MIN_THROUGHPUT_INTERVAL_MS: f64 = 100.0;

    /// Time of continuous buffering before resetting watermark escalations (seconds).
    /// After this duration, the outage is considered a network event (not jitter),
    /// and the watermark returns to baseline for faster recovery on reconnection.
    pub const ESCALATION_DECAY_SECS: u64 = 15;
}
