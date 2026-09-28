//! Stream statistics
//!
//! `StreamStats` is a shared snapshot of all stream metrics, polled by the UI.
//! `DecoderStats` provides atomic counters for the hot decode path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::audio::health::HealthState;
use crate::audio::types::CodecInfo;
use crate::stream::types::StreamType;

/// Snapshot of all stream statistics, updated by the engine on its 500ms tick.
#[derive(Debug, Clone)]
pub struct StreamStats {
    pub codec_info: Option<CodecInfo>,
    /// Direct (ICY) or HLS, for a station played with `play_stream`
    pub stream_type: Option<StreamType>,

    pub frames_played: u64,
    pub decode_errors: u64,
    pub bytes_received: u64,
    pub segments_downloaded: u64,
    pub sample_count: u64,

    pub health_state: HealthState,

    pub buffer_level_bytes: usize,
    pub buffer_capacity_bytes: usize,
    pub is_buffering: bool,
    pub throughput_kbps: f64,
    pub underrun_count: u32,
    pub effective_watermark: usize,
    /// Times the output ran out of decoded audio and played silence (the
    /// station stalled for longer than the audio decoded ahead)
    pub output_underruns: u64,

    pub play_started_at: Option<Instant>,
}

impl Default for StreamStats {
    fn default() -> Self {
        Self {
            codec_info: None,
            stream_type: None,
            frames_played: 0,
            decode_errors: 0,
            bytes_received: 0,
            segments_downloaded: 0,
            sample_count: 0,
            health_state: HealthState::WaitingForAudio,
            buffer_level_bytes: 0,
            buffer_capacity_bytes: 0,
            is_buffering: false,
            throughput_kbps: 0.0,
            underrun_count: 0,
            effective_watermark: 0,
            output_underruns: 0,
            play_started_at: None,
        }
    }
}

/// Thread-safe handle to shared stats
pub type SharedStats = Arc<Mutex<StreamStats>>;

/// Create a new shared stats instance
pub fn new_shared_stats() -> SharedStats {
    Arc::new(Mutex::new(StreamStats::default()))
}

/// Atomic counters for the hot decode path (lock-free)
pub struct DecoderStats {
    pub frames_played: AtomicU64,
    pub decode_errors: AtomicU64,
}

impl Default for DecoderStats {
    fn default() -> Self {
        Self::new()
    }
}

impl DecoderStats {
    /// Create a new decoder stats instance with zeroed counters
    pub fn new() -> Self {
        Self {
            frames_played: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
        }
    }

    /// Increment the frames-played counter (called from decode loop)
    pub fn record_frame(&self) {
        self.frames_played.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the decode-errors counter
    pub fn record_error(&self) {
        self.decode_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Read both counters (not truly atomic pair, but close enough for stats)
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.frames_played.load(Ordering::Relaxed),
            self.decode_errors.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- StreamStats ---

    #[test]
    fn stream_stats_default() {
        let stats = StreamStats::default();
        assert!(stats.codec_info.is_none());
        assert!(stats.stream_type.is_none());
        assert_eq!(stats.frames_played, 0);
        assert_eq!(stats.decode_errors, 0);
        assert_eq!(stats.bytes_received, 0);
        assert_eq!(stats.sample_count, 0);
        assert_eq!(stats.health_state, HealthState::WaitingForAudio);
        assert!(stats.play_started_at.is_none());
    }

    #[test]
    fn stream_stats_clone() {
        let stats = StreamStats {
            frames_played: 42,
            bytes_received: 1024,
            ..StreamStats::default()
        };
        let cloned = stats.clone();
        assert_eq!(cloned.frames_played, 42);
        assert_eq!(cloned.bytes_received, 1024);
    }

    #[test]
    fn stream_stats_debug() {
        let stats = StreamStats::default();
        let debug = format!("{:?}", stats);
        assert!(debug.contains("StreamStats"));
    }

    // --- SharedStats ---

    #[test]
    fn new_shared_stats_creates_default() {
        let shared = new_shared_stats();
        let stats = shared.lock().unwrap();
        assert_eq!(stats.frames_played, 0);
        assert!(stats.codec_info.is_none());
    }

    #[test]
    fn shared_stats_multiple_arcs() {
        let shared = new_shared_stats();
        let s2 = shared.clone();
        {
            let mut stats = shared.lock().unwrap();
            stats.frames_played = 100;
        }
        let stats = s2.lock().unwrap();
        assert_eq!(stats.frames_played, 100);
    }

    // --- DecoderStats ---

    #[test]
    fn decoder_stats_new() {
        let stats = DecoderStats::new();
        let (packets, errors) = stats.snapshot();
        assert_eq!(packets, 0);
        assert_eq!(errors, 0);
    }

    #[test]
    fn decoder_stats_record_frame() {
        let stats = DecoderStats::new();
        stats.record_frame();
        stats.record_frame();
        stats.record_frame();
        let (packets, errors) = stats.snapshot();
        assert_eq!(packets, 3);
        assert_eq!(errors, 0);
    }

    #[test]
    fn decoder_stats_record_error() {
        let stats = DecoderStats::new();
        stats.record_error();
        stats.record_error();
        let (packets, errors) = stats.snapshot();
        assert_eq!(packets, 0);
        assert_eq!(errors, 2);
    }

    #[test]
    fn decoder_stats_mixed() {
        let stats = DecoderStats::new();
        stats.record_frame();
        stats.record_error();
        stats.record_frame();
        stats.record_frame();
        stats.record_error();
        let (packets, errors) = stats.snapshot();
        assert_eq!(packets, 3);
        assert_eq!(errors, 2);
    }

    #[test]
    fn decoder_stats_arc_shared() {
        let stats = Arc::new(DecoderStats::new());
        let s2 = stats.clone();

        stats.record_frame();
        s2.record_frame();

        let (packets, _) = stats.snapshot();
        assert_eq!(packets, 2);
    }

    #[test]
    fn decoder_stats_many_increments() {
        let stats = DecoderStats::new();
        for _ in 0..10000 {
            stats.record_frame();
        }
        let (packets, _) = stats.snapshot();
        assert_eq!(packets, 10000);
    }
}
