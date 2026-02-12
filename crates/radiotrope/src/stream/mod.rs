//! Stream handling
//!
//! Handles different stream types: HLS, ICY (Icecast/Shoutcast), direct.
//! Resolves URLs (PLS/M3U playlists, HLS detection), connects to streams,
//! extracts ICY metadata, downloads HLS segments with MPEG-TS demuxing.

use crate::config::timeouts::{MAX_BACKOFF_SECS, RETRY_BASE_DELAY_SECS};
use std::time::Duration;

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
/// e.g., with base=2s: 2s, 4s, 8s, 16s, 30s, 30s, ...
pub(crate) fn backoff_delay(consecutive_failures: u32) -> Duration {
    let exp = consecutive_failures.saturating_sub(1).min(5);
    let delay_secs = RETRY_BASE_DELAY_SECS.saturating_mul(1u64 << exp);
    Duration::from_secs(delay_secs.min(MAX_BACKOFF_SECS))
}
