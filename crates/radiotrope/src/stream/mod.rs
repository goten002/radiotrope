//! Stream handling
//!
//! Handles different stream types: HLS, ICY (Icecast/Shoutcast), direct.
//! Resolves URLs (PLS/M3U playlists, HLS detection), connects to streams,
//! extracts ICY metadata, downloads HLS segments with MPEG-TS demuxing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::timeouts::{MAX_BACKOFF_SECS, RETRY_BASE_DELAY_SECS};

pub mod buffer;
pub mod hls;
pub mod icy;
pub mod metadata;
pub mod playlist;
pub mod resolver;
pub mod types;

pub use buffer::{BufferStatus, SharedBufferStatus, StreamBuffer, StreamBufferReader};
pub use metadata::StreamMetadata;
pub use resolver::StreamResolver;
pub use types::{ResolvedStream, StreamInfo, StreamType};

/// Calculate exponential backoff delay: min(2^(n-1) * base, max)
/// e.g., with base=2s: 2s, 4s, 8s, 10s, 10s, ...
pub(crate) fn backoff_delay(consecutive_failures: u32) -> Duration {
    let exp = consecutive_failures.saturating_sub(1).min(5);
    let delay_secs = RETRY_BASE_DELAY_SECS.saturating_mul(1u64 << exp);
    Duration::from_secs(delay_secs.min(MAX_BACKOFF_SECS))
}

/// Sleep with backoff, checking stop_flag every 250ms.
/// Returns true if the full duration elapsed, false if stopped early.
pub(crate) fn backoff_sleep(consecutive_failures: u32, stop_flag: &Arc<AtomicBool>) -> bool {
    let total = backoff_delay(consecutive_failures);
    let interval = Duration::from_millis(250);
    let start = std::time::Instant::now();
    while start.elapsed() < total {
        if stop_flag.load(Ordering::Relaxed) {
            return false;
        }
        let remaining = total.saturating_sub(start.elapsed());
        std::thread::sleep(remaining.min(interval));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn backoff_sleep_returns_true_on_completion() {
        let stop = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        // failures=1 → 2s delay
        let result = backoff_sleep(1, &stop);
        assert!(result, "Should return true when sleep completes");
        assert!(start.elapsed() >= Duration::from_secs(2));
    }

    #[test]
    fn backoff_sleep_returns_false_on_stop() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        // Set stop flag after 100ms from another thread
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stop_clone.store(true, Ordering::Relaxed);
        });

        let start = Instant::now();
        // failures=3 → 8s delay, but should exit early
        let result = backoff_sleep(3, &stop);
        assert!(!result, "Should return false when stopped early");
        // Should exit well before the full 8s (within ~350ms: 100ms wait + 250ms check interval)
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
