//! Stream handling
//!
//! Handles different stream types: HLS, ICY (Icecast/Shoutcast), direct.
//! Resolves URLs (PLS/M3U playlists, HLS detection), connects to streams,
//! extracts ICY and embedded ID3 metadata, downloads HLS segments with MPEG-TS demuxing.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::timeouts::{MAX_BACKOFF_SECS, RESOLVE_TIMEOUT_SECS, RETRY_BASE_DELAY_SECS};
use crate::error::{RadioError, Result};

pub mod buffer;
pub mod cancel;
pub mod hls;
pub mod hls_metadata;
pub mod icy;
pub mod id3;
pub mod metadata;
pub mod playlist;
pub mod resolver;
mod shoutcast;
#[cfg(test)]
pub(crate) mod test_server;
pub mod types;
#[cfg(test)]
mod untrusted_input_tests;

pub use buffer::{BufferStatus, SharedBufferStatus, StreamBuffer, StreamBufferReader};
pub use cancel::StreamCancel;
pub use metadata::{MetadataSink, MetadataSource, StreamMetadata};
pub use resolver::StreamResolver;
pub use types::{ResolvedStream, StreamInfo, StreamType};

/// How long a network reader waits for data before returning
/// `ErrorKind::Interrupted`. Stopping the stream through its [`StreamCancel`]
/// ends the wait at once; this bound is for a caller that stops a reader
/// some other way (a stream buffer given a token of its own): it checks for
/// its stop and reads again.
pub(crate) const READ_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How a network reader's background thread ended. The thread records it
/// just before exiting, and the reader reports it once the audio already
/// received has been read: a finished stream ends cleanly, a dead one ends
/// with the reason.
#[derive(Clone, Default)]
pub(crate) struct StreamEnd(Arc<Mutex<Option<std::result::Result<(), String>>>>);

impl StreamEnd {
    /// The stream is complete (a whole file, or the end of an HLS VOD playlist)
    pub(crate) fn finish(&self) {
        self.set(Ok(()));
    }

    /// The thread gave up on the stream; `reason` is shown to the user
    pub(crate) fn fail(&self, reason: String) {
        self.set(Err(reason));
    }

    fn set(&self, end: std::result::Result<(), String>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(end);
    }

    /// Why the thread gave up, if it did
    pub(crate) fn failure(&self) -> Option<String> {
        match &*self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(Err(reason)) => Some(reason.clone()),
            _ => None,
        }
    }

    /// What `read` returns once the thread is gone and its audio is drained.
    /// `unknown` describes a thread that ended without recording why (it was
    /// stopped).
    pub(crate) fn read_result(&self, unknown: &str) -> io::Result<usize> {
        match &*self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(Ok(())) => Ok(0),
            Some(Err(reason)) => Err(io::Error::other(reason.clone())),
            None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, unknown)),
        }
    }
}

/// The reason a reader gives up on a station: no audio for `after`, and the
/// last thing that went wrong
pub(crate) fn gave_up(after: Duration, last_problem: &str) -> String {
    let secs = after.as_secs();
    let after = if secs >= 60 && secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    };
    format!("No audio for {after}: {last_problem}")
}

/// Calculate exponential backoff delay: min(2^(n-1) * base, max)
/// e.g., with base=2s: 2s, 4s, 8s, 10s, 10s, ...
pub(crate) fn backoff_delay(consecutive_failures: u32) -> Duration {
    let exp = consecutive_failures.saturating_sub(1).min(5);
    let delay_secs = RETRY_BASE_DELAY_SECS.saturating_mul(1u64 << exp);
    Duration::from_secs(delay_secs.min(MAX_BACKOFF_SECS))
}

/// Sleep with backoff unless the stream is cancelled first.
/// Returns true if the full duration elapsed, false if cancelled.
pub(crate) fn backoff_sleep(consecutive_failures: u32, cancel: &StreamCancel) -> bool {
    cancel.sleep(backoff_delay(consecutive_failures))
}

/// When a resolve must be done by.
///
/// Each step of a resolve (a playlist fetch, the wait for the first audio)
/// waits at most for the time left, so a station that is slow to start
/// fails with the reason of the step it was stuck on, not the app's
/// generic timeout.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline(Option<Instant>);

impl Deadline {
    /// No deadline: each step keeps its own timeout
    pub(crate) const NONE: Self = Self(None);

    pub(crate) fn after(time: Duration) -> Self {
        Self(Instant::now().checked_add(time))
    }

    /// The deadline of a station's resolve, [`RESOLVE_TIMEOUT_SECS`] from now
    pub(crate) fn for_resolve() -> Self {
        Self::after(Duration::from_secs(RESOLVE_TIMEOUT_SECS))
    }

    /// `limit`, or the time left if that is shorter
    pub(crate) fn cap(&self, limit: Duration) -> Duration {
        match self.0 {
            Some(at) => limit.min(at.saturating_duration_since(Instant::now())),
            None => limit,
        }
    }

    /// A timeout error once the deadline has passed
    pub(crate) fn check(&self) -> Result<()> {
        match self.0 {
            Some(at) if Instant::now() >= at => Err(RadioError::Timeout(format!(
                "the station did not start within {RESOLVE_TIMEOUT_SECS} s"
            ))),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn backoff_sleep_returns_true_on_completion() {
        let stop = StreamCancel::new();
        let start = Instant::now();
        // failures=1 → 2s delay
        let result = backoff_sleep(1, &stop);
        assert!(result, "Should return true when sleep completes");
        assert!(start.elapsed() >= Duration::from_secs(2));
    }

    #[test]
    fn backoff_sleep_returns_false_on_stop() {
        let stop = StreamCancel::new();
        let stop_clone = stop.clone();

        // Cancel after 100ms from another thread
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stop_clone.cancel();
        });

        let start = Instant::now();
        // failures=3 → 8s delay, but should exit early
        let result = backoff_sleep(3, &stop);
        assert!(!result, "Should return false when stopped early");
        // Wakes as soon as it is cancelled
        assert!(start.elapsed() < Duration::from_millis(500));
    }
    #[test]
    fn stream_end_tells_finished_from_failed() {
        let end = StreamEnd::default();
        let err = end.read_result("ICY stream ended").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(end.failure(), None);

        end.finish();
        assert_eq!(end.read_result("ICY stream ended").unwrap(), 0);
        assert_eq!(end.failure(), None);

        end.fail("HTTP 404 Not Found".to_string());
        let err = end.read_result("ICY stream ended").unwrap_err();
        assert_eq!(err.to_string(), "HTTP 404 Not Found");
        assert_eq!(end.failure().as_deref(), Some("HTTP 404 Not Found"));
    }

    #[test]
    fn a_deadline_caps_each_wait_to_the_time_left() {
        let limit = Duration::from_secs(10);
        assert_eq!(Deadline::NONE.cap(limit), limit);
        assert!(Deadline::NONE.check().is_ok());

        let deadline = Deadline::after(Duration::from_millis(200));
        let left = deadline.cap(limit);
        assert!(left <= Duration::from_millis(200) && left > Duration::ZERO);
        assert_eq!(
            deadline.cap(Duration::from_millis(50)),
            Duration::from_millis(50)
        );
        assert!(deadline.check().is_ok());

        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(deadline.cap(limit), Duration::ZERO);
        let err = deadline.check().unwrap_err();
        assert!(matches!(err, RadioError::Timeout(_)), "{err}");
    }

    #[test]
    fn gave_up_names_the_wait_and_the_problem() {
        assert_eq!(
            gave_up(Duration::from_secs(120), "HTTP 404 Not Found"),
            "No audio for 2 min: HTTP 404 Not Found"
        );
        assert_eq!(
            gave_up(Duration::from_secs(90), "x"),
            "No audio for 90 s: x"
        );
        assert_eq!(gave_up(Duration::from_secs(1), "x"), "No audio for 1 s: x");
    }
}
