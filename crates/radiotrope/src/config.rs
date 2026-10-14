//! Configuration constants for the radiotrope engine

/// Audio-related configuration
pub mod audio {
    /// FFT window size for visualization
    pub const FFT_SIZE: usize = 512;

    /// Number of frequency bands in spectrum display
    pub const SPECTRUM_BANDS: usize = 16;

    /// Audio played without anyone showing the meters and spectrum after
    /// which they are no longer worked out (seconds; see `AudioAnalysis`)
    pub const UNSHOWN_GRACE_SECS: f32 = 2.0;

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

    /// Decoded audio is kept queued this many output buffers ahead of the
    /// output device, since its callback takes a whole buffer at a time
    pub const DECODE_AHEAD_BUFFERS: u32 = 3;
    /// Least decoded audio kept ahead of the output (ms)
    pub const DECODE_AHEAD_MIN_MS: u64 = 100;
    /// Most decoded audio kept ahead of the output (ms)
    pub const DECODE_AHEAD_MAX_MS: u64 = 1000;
    /// Decoded audio kept ahead when the device's buffer size isn't known (ms)
    pub const DECODE_AHEAD_DEFAULT_MS: u64 = 300;
    /// Length of a chunk of decoded audio in that queue (ms)
    pub const DECODE_CHUNK_MS: u64 = 20;
    /// Silence the output plays at a time when the queue runs dry (ms)
    pub const UNDERRUN_SILENCE_MS: u64 = 10;
}

/// Network-related configuration
pub mod network {
    /// User agent for HTTP requests
    pub const USER_AGENT: &str = concat!("Radiotrope/", env!("CARGO_PKG_VERSION"));

    /// Connection timeout in seconds
    pub const CONNECT_TIMEOUT_SECS: u64 = 10;

    /// Read timeout in seconds
    pub const READ_TIMEOUT_SECS: u64 = 30;

    /// How far the wall clock may run ahead of the monotonic one before a
    /// playing stream takes it that the computer slept, and reconnects
    /// (seconds; Linux, see `icy.rs`)
    pub const SLEEP_JUMP_SECS: u64 = 5;

    /// Maximum playlist resolution depth
    pub const MAX_PLAYLIST_DEPTH: usize = 5;

    /// Most stream addresses of one PLS or M3U playlist tried, in order,
    /// until one plays (a station's mirrors)
    pub const MAX_PLAYLIST_ENTRIES: usize = 5;

    /// Wait before a station that failed to connect is tried once more,
    /// when it is started (milliseconds; see `StreamResolver`)
    pub const CONNECT_RETRY_MS: u64 = 1000;
}

/// HLS-related configuration
pub mod hls {
    /// Number of segments to buffer
    pub const SEGMENT_BUFFER_SIZE: usize = 3;

    /// Segment download timeout in seconds
    pub const SEGMENT_TIMEOUT_SECS: u64 = 15;

    /// How long to wait for the first segment before giving up, in seconds.
    /// Within a station's resolve the wait is also cut to the time left of
    /// [`super::timeouts::RESOLVE_TIMEOUT_SECS`].
    pub const FIRST_SEGMENT_TIMEOUT_SECS: u64 = 12;

    /// Failures in a row (of the media playlist, or of its segments) after
    /// which the playlist is found again from the address it was found
    /// from, in case its own address has expired or its server is gone
    pub const FIND_AGAIN_AFTER_FAILURES: u32 = 2;

    /// Largest playlist read, in bytes (a radio playlist is a few KB). A
    /// larger one is a failed download, and isn't read to the end.
    pub const MAX_PLAYLIST_BYTES: usize = 4 * 1024 * 1024;

    /// Largest segment read, media or init segment, in bytes (audio
    /// segments are well under 1 MB). A larger one is a failed download.
    pub const MAX_SEGMENT_BYTES: usize = 32 * 1024 * 1024;

    /// Longest a playlist or segment download may take, however fast its
    /// bytes come, in seconds. A body that doesn't end (a live stream where
    /// a segment should be) is a failed download.
    pub const MAX_DOWNLOAD_SECS: u64 = 2 * SEGMENT_TIMEOUT_SECS;
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

    /// Connect timeout for a playing stream's own requests: ICY reconnects,
    /// and the HLS downloader's playlist and segment fetches (seconds).
    /// Shorter than [`super::network::CONNECT_TIMEOUT_SECS`], so a playing
    /// stream recovers sooner.
    pub const STREAM_CONNECT_TIMEOUT_SECS: u64 = 5;

    /// Buffering duration before the engine signals a stall to the UI (seconds).
    /// Short enough to give timely feedback, long enough to avoid false alarms
    /// from normal buffering events (HLS gaps, brief hiccups).
    pub const BUFFERING_STALL_THRESHOLD_SECS: u64 = 10;

    /// How long a playing station may send no audio (while reconnecting, or
    /// with an HLS playlist that stops adding segments) before its reader
    /// gives up and playback stops with the reason (seconds).
    pub const RECONNECT_GIVE_UP_SECS: u64 = 120;

    /// How long resolving a station may take, from its address to the first
    /// audio: playlist fetches, the HLS playlists and the first segment, or
    /// the first ICY data (seconds). Each step waits at most for the time
    /// left, so a slow station fails with the step's own reason. The app
    /// waits a little longer than this.
    pub const RESOLVE_TIMEOUT_SECS: u64 = 12;
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

    /// Center frequencies for each band (Hz). The two outer bands are
    /// shelves, and a shelf's frequency is where it reaches half its gain:
    /// 40 Hz still moves the kick drum, and 12 kHz lands below where radio
    /// encoders cut off (about 17 kHz for a 128 kbps MP3)
    pub const CENTER_FREQUENCIES: [f32; 10] = [
        40.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 12000.0,
    ];

    /// Display labels for each band
    pub const FREQ_LABELS: [&str; 10] = [
        "40", "64", "125", "250", "500", "1K", "2K", "4K", "8K", "12K",
    ];

    /// Minimum gain per band (dB)
    pub const MIN_GAIN_DB: f32 = -12.0;

    /// Maximum gain per band (dB)
    pub const MAX_GAIN_DB: f32 = 12.0;

    /// Default Q factor for peaking EQ filters
    pub const DEFAULT_Q: f32 = 1.414;

    /// Q of the shelves (the lowest and highest band): the steepest shelf
    /// without a bump or dip at its corner
    pub const SHELF_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

    /// The highest band's shelf moves down to this fraction of the sample
    /// rate when 12 kHz is past it, so it still works on 22.05 kHz streams
    pub const TOP_SHELF_MAX_FRACTION: f32 = 0.4;
}

/// The Sound filters: station volume matching, the limiter, the rumble
/// filter, channels and fades
pub mod sound {
    /// Loudness each Level choice brings stations to (LUFS). Normal is
    /// Apple Music's level and the AES recommendation for music streams.
    pub const TARGET_QUIET_LUFS: f32 = -20.0;
    pub const TARGET_NORMAL_LUFS: f32 = -16.0;
    pub const TARGET_LOUD_LUFS: f32 = -12.0;

    /// Matching turns a station down at most this far, and up at most
    /// `MAX_GAIN_DB` (dB)
    pub const MIN_GAIN_DB: f32 = -12.0;
    pub const MAX_GAIN_DB: f32 = 9.0;

    /// How fast the gain moves toward its target (dB per second): down
    /// quickly, so a loud station or ad doesn't blast, up slowly, so a
    /// quiet intro isn't pumped up
    pub const DOWN_DB_PER_SEC: f32 = 2.0;
    pub const UP_DB_PER_SEC: f32 = 0.5;
    /// Both ways while a new station is first being measured
    pub const LEARNING_DB_PER_SEC: f32 = 3.0;
    /// Down, when the last few seconds are far louder than the target
    pub const PANIC_DB_PER_SEC: f32 = 6.0;
    /// How far above the target the last 3 s must be for that (LU)
    pub const PANIC_LU: f32 = 6.0;

    /// A station measured for the first time starts this far down: it is
    /// most likely loud, and a quiet start beats a blast (dB)
    pub const UNKNOWN_START_DB: f32 = -7.0;
    /// Seconds of sound before a new station's loudness counts as known
    pub const SETTLE_SECS: f32 = 20.0;
    /// Seconds of the stream the loudness is measured over
    pub const WINDOW_SECS: f32 = 60.0;
    /// Quieter than this (momentary LUFS) is silence or dead air: the gain
    /// holds still and the window skips it
    pub const SILENCE_LUFS: f32 = -45.0;

    /// The limiter keeps true peaks under this (dBFS)
    pub const LIMITER_CEILING_DB: f32 = -1.0;
    /// How far ahead it looks, so the gain is down before a peak (ms)
    pub const LIMITER_LOOKAHEAD_MS: f32 = 5.0;
    /// How long it takes to let go after a peak (ms)
    pub const LIMITER_RELEASE_MS: f32 = 100.0;

    /// The rumble filter's corner (Hz): below the lowest note of a bass
    pub const RUMBLE_HZ: f64 = 25.0;

    /// How long the Hum Filter listens before each decision (ms)
    pub const HUM_CHECK_MS: u32 = 1000;
    /// How far a hum line stands above the sound beside it (dB)
    pub const HUM_ABOVE_DB: f32 = 12.0;
    /// Hz beside each line the Hum Filter compares it with
    pub const HUM_BESIDE_HZ: f32 = 8.0;
    /// The hum and harmonics it looks at, and removes
    pub const HUM_HARMONICS: usize = 6;
    /// Lines (of [`HUM_HARMONICS`]) that make it hum
    pub const HUM_MIN_LINES: usize = 3;
    /// Checks in a row with hum before it is removed
    pub const HUM_FOUND_CHECKS: u32 = 6;
    /// Checks in a row without it before removing stops
    pub const HUM_GONE_CHECKS: u32 = 10;
    /// Quietest hum worth removing (amplitude, about -80 dBFS)
    pub const HUM_MIN_LEVEL: f32 = 1e-4;
    /// Width of each notch (Hz)
    pub const HUM_NOTCH_HZ: f64 = 3.0;
    /// The notches fade in and out over this long (ms)
    pub const HUM_FADE_MS: f32 = 1000.0;

    /// The Hiss Reducer leaves the sound below this alone (Hz)...
    pub const HISS_FROM_HZ: f32 = 1000.0;
    /// ...and works fully above this (Hz)
    pub const HISS_FULL_HZ: f32 = 2500.0;
    /// How long it remembers the quietest each band got (s)
    pub const HISS_MEMORY_SECS: f32 = 3.0;
    /// Most it turns a band down: Light, Strong (dB)
    pub const HISS_LIGHT_DB: f32 = -6.0;
    pub const HISS_STRONG_DB: f32 = -12.0;

    /// A new station fades in over this long (ms)
    pub const FADE_IN_MS: f32 = 800.0;
    /// The old station fades out over this long when another starts (ms)
    pub const FADE_OUT_MS: u64 = 400;
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
    /// Audio buffered before a station starts to play (seconds).
    ///
    /// The buffer's targets are times; the stream's byte rate turns them into
    /// bytes (see `DEFAULT_BYTE_RATE`).
    pub const START_BUFFER_SECS: f64 = 3.0;
    /// Longest wait to start (seconds): after this, playing starts with
    /// what is buffered. Well under `timeouts::PROBE_TIMEOUT_SECS`, so a
    /// station slower than `DEFAULT_BYTE_RATE` that doesn't advertise its
    /// bitrate still starts.
    pub const START_MAX_WAIT_SECS: f64 = 6.0;
    /// Audio buffered again after the buffer ran dry (seconds). Each further
    /// underrun adds `REFILL_STEP_SECS`, up to `MAX_REFILL_SECS`.
    pub const REFILL_BUFFER_SECS: f64 = 5.0;
    /// What each further underrun adds to the refill target (seconds)
    pub const REFILL_STEP_SECS: f64 = 5.0;
    /// Largest refill target (seconds)
    pub const MAX_REFILL_SECS: f64 = 20.0;
    /// Audio played without an underrun that undoes one step (seconds), so
    /// rare hiccups over a long session don't pile up
    pub const ESCALATION_FORGET_SECS: f64 = 60.0;
    /// Byte rate assumed while the stream's is unknown: 128 kbps.
    ///
    /// The rate is the station's advertised bitrate (`icy-br`) if it has
    /// one, else measured from the bytes the decoder needs per second of
    /// audio.
    pub const DEFAULT_BYTE_RATE: f64 = 16_000.0;
    /// Audio decoded before the byte rate measurement starts (seconds),
    /// skipping the probe's reads
    pub const RATE_SKIP_SECS: f64 = 2.0;
    /// Audio a byte rate measurement spans before it is used (seconds)
    pub const RATE_SPAN_SECS: f64 = 20.0;
    /// Byte rates are kept within this range (8 kbps to 8 Mbps); an
    /// advertised bitrate outside it is ignored
    pub const MIN_BYTE_RATE: f64 = 1_000.0;
    pub const MAX_BYTE_RATE: f64 = 1_000_000.0;
    /// Largest watermark (bytes). Kept well below `MAX_BUFFER_SIZE` minus
    /// what compaction leaves behind, so a refill can always be reached.
    pub const MAX_WATERMARK_BYTES: usize = 1024 * 1024;

    /// Minimum interval between throughput EMA updates (milliseconds).
    /// Prevents burst reads after reconnection from spiking the EMA.
    pub const MIN_THROUGHPUT_INTERVAL_MS: f64 = 100.0;

    /// Time of continuous buffering before the refill target drops back to
    /// `REFILL_BUFFER_SECS` (seconds). After this long the outage is a
    /// network event, not jitter, and a smaller target resumes playback
    /// sooner once the station is back.
    pub const ESCALATION_DECAY_SECS: u64 = 15;
}
