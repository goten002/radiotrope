//! Stream statistics and event broadcasting
//!
//! `StreamStats` is a shared snapshot of all stream metrics, polled by the UI.
//! `EventBus` broadcasts discrete `StreamEvent`s to subscribers.
//! `DecoderStats` provides atomic counters for the hot decode path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::audio::health::HealthState;
use crate::audio::types::CodecInfo;
use crate::stream::types::StreamType;

/// Snapshot of all stream statistics, updated by the engine on its 500ms tick.
#[derive(Debug, Clone)]
pub struct StreamStats {
    pub codec_info: Option<CodecInfo>,
    pub stream_type: Option<StreamType>,
    pub stream_url: String,

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

    pub play_started_at: Option<Instant>,
}

impl Default for StreamStats {
    fn default() -> Self {
        Self {
            codec_info: None,
            stream_type: None,
            stream_url: String::new(),
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

/// Discrete events broadcast to subscribers
#[derive(Debug, Clone)]
pub enum StreamEvent {
    PlaybackStarted {
        codec_info: CodecInfo,
        stream_url: String,
    },
    PlaybackStopped,
    MetadataChanged {
        title: String,
        artist: String,
    },
    HealthChanged(HealthState),
    Error(String),
}

/// Broadcast mechanism for stream events
pub struct EventBus {
    subscribers: Mutex<Vec<Sender<StreamEvent>>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    /// Create a new event bus with no subscribers
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Subscribe to events. Returns a receiver that will get all future events.
    pub fn subscribe(&self) -> Receiver<StreamEvent> {
        let (tx, rx) = unbounded();
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(tx);
        }
        rx
    }

    /// Emit an event to all subscribers. Removes disconnected subscribers.
    pub fn emit(&self, event: StreamEvent) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.retain(|tx| tx.send(event.clone()).is_ok());
        }
    }
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
        assert!(stats.stream_url.is_empty());
        assert_eq!(stats.frames_played, 0);
        assert_eq!(stats.decode_errors, 0);
        assert_eq!(stats.bytes_received, 0);
        assert_eq!(stats.sample_count, 0);
        assert_eq!(stats.health_state, HealthState::WaitingForAudio);
        assert!(stats.play_started_at.is_none());
    }

    #[test]
    fn stream_stats_clone() {
        let mut stats = StreamStats::default();
        stats.frames_played = 42;
        stats.bytes_received = 1024;
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

    // --- StreamEvent ---

    #[test]
    fn stream_event_playback_started() {
        let evt = StreamEvent::PlaybackStarted {
            codec_info: CodecInfo {
                codec_name: "MP3".to_string(),
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: None,
                bitrate: Some(128),
            },
            stream_url: "http://example.com".to_string(),
        };
        let debug = format!("{:?}", evt);
        assert!(debug.contains("PlaybackStarted"));
        assert!(debug.contains("MP3"));
    }

    #[test]
    fn stream_event_clone() {
        let evt = StreamEvent::Error("test".to_string());
        let cloned = evt.clone();
        if let StreamEvent::Error(msg) = cloned {
            assert_eq!(msg, "test");
        } else {
            panic!("Expected Error variant");
        }
    }

    #[test]
    fn stream_event_all_variants() {
        let _ = format!("{:?}", StreamEvent::PlaybackStopped);
        let _ = format!(
            "{:?}",
            StreamEvent::MetadataChanged {
                title: "Song".to_string(),
                artist: "Artist".to_string(),
            }
        );
        let _ = format!("{:?}", StreamEvent::HealthChanged(HealthState::Healthy));
        let _ = format!("{:?}", StreamEvent::Error("err".to_string()));
    }

    // --- EventBus ---

    #[test]
    fn event_bus_subscribe_and_emit() {
        let bus = EventBus::new();
        let rx = bus.subscribe();

        bus.emit(StreamEvent::PlaybackStopped);

        let evt = rx.recv().unwrap();
        assert!(matches!(evt, StreamEvent::PlaybackStopped));
    }

    #[test]
    fn event_bus_multiple_subscribers() {
        let bus = EventBus::new();
        let rx1 = bus.subscribe();
        let rx2 = bus.subscribe();

        bus.emit(StreamEvent::PlaybackStopped);

        assert!(matches!(rx1.recv().unwrap(), StreamEvent::PlaybackStopped));
        assert!(matches!(rx2.recv().unwrap(), StreamEvent::PlaybackStopped));
    }

    #[test]
    fn event_bus_disconnected_subscriber_cleanup() {
        let bus = EventBus::new();
        let rx1 = bus.subscribe();
        let _rx2 = bus.subscribe();
        drop(rx1); // disconnect first subscriber

        // Should not panic, and remaining subscriber should get the event
        bus.emit(StreamEvent::PlaybackStopped);

        // Verify the dead subscriber was cleaned up
        let subs = bus.subscribers.lock().unwrap();
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn event_bus_no_subscribers() {
        let bus = EventBus::new();
        // Should not panic when emitting with no subscribers
        bus.emit(StreamEvent::PlaybackStopped);
    }

    #[test]
    fn event_bus_multiple_events() {
        let bus = EventBus::new();
        let rx = bus.subscribe();

        bus.emit(StreamEvent::PlaybackStopped);
        bus.emit(StreamEvent::Error("err1".to_string()));
        bus.emit(StreamEvent::Error("err2".to_string()));

        assert!(matches!(rx.recv().unwrap(), StreamEvent::PlaybackStopped));
        if let StreamEvent::Error(msg) = rx.recv().unwrap() {
            assert_eq!(msg, "err1");
        }
        if let StreamEvent::Error(msg) = rx.recv().unwrap() {
            assert_eq!(msg, "err2");
        }
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
