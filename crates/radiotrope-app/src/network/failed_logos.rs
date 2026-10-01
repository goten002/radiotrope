//! Logo URLs that failed, so they aren't fetched again on every look
//!
//! A failure that would happen again (the server refused, the data is too
//! large or not an image) is kept for the session. One that may pass (no
//! network, a timeout, a server error) is forgotten after
//! [`RETRY_AFTER`], and a later success clears it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::config::logos::RETRY_AFTER;
use crate::error::{AppError, ServiceProblem};

#[derive(Default)]
pub struct FailedLogos {
    /// When each URL failed, and whether for good
    failed: Mutex<HashMap<String, (Instant, bool)>>,
}

impl FailedLogos {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `url` failed and isn't due for another try
    pub fn has_failed(&self, url: &str) -> bool {
        self.has_failed_at(url, Instant::now())
    }

    /// Remember that fetching `url` failed with `error`
    pub fn record(&self, url: &str, error: &AppError) {
        self.record_at(url, lasting(error), Instant::now());
    }

    /// Remember that `url` gave data that isn't a usable image
    pub fn record_unusable(&self, url: &str) {
        self.record_at(url, true, Instant::now());
    }

    /// `url` worked after all
    pub fn clear(&self, url: &str) {
        self.lock().remove(url);
    }

    fn has_failed_at(&self, url: &str, now: Instant) -> bool {
        self.lock()
            .get(url)
            .is_some_and(|&(at, lasting)| lasting || now.duration_since(at) < RETRY_AFTER)
    }

    fn record_at(&self, url: &str, lasting: bool, now: Instant) {
        let mut failed = self.lock();
        // Passing failures that are due again needn't be kept
        failed.retain(|_, &mut (at, lasting)| lasting || now.duration_since(at) < RETRY_AFTER);
        failed.insert(url.to_string(), (now, lasting));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, bool)>> {
        self.failed.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Whether a fetch that failed with `e` would fail the same way again
fn lasting(e: &AppError) -> bool {
    match e {
        // Too large, not an image, no URL
        AppError::Image(_) | AppError::InvalidResponse(_) | AppError::NotFound(_) => true,
        // The server refused it, other than for taking too long
        _ => matches!(ServiceProblem::of(e), ServiceProblem::Refused(code) if code != 408),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passing_failure_is_tried_again_later() {
        let failed = FailedLogos::new();
        let now = Instant::now();
        failed.record_at("http://a/logo.png", false, now);
        assert!(failed.has_failed_at("http://a/logo.png", now));
        assert!(failed.has_failed_at("http://a/logo.png", now + RETRY_AFTER / 2));
        assert!(!failed.has_failed_at("http://a/logo.png", now + RETRY_AFTER));
        assert!(!failed.has_failed_at("http://b/logo.png", now));
    }

    #[test]
    fn a_lasting_failure_stays_for_the_session() {
        let failed = FailedLogos::new();
        let now = Instant::now();
        failed.record_at("http://a/logo.png", true, now);
        assert!(failed.has_failed_at("http://a/logo.png", now + RETRY_AFTER * 10));
    }

    #[test]
    fn success_and_expiry_forget_failures() {
        let failed = FailedLogos::new();
        let now = Instant::now();
        failed.record_at("http://a/logo.png", false, now);
        failed.clear("http://a/logo.png");
        assert!(!failed.has_failed_at("http://a/logo.png", now));

        // Due passing failures are dropped as new ones come in
        failed.record_at("http://b/logo.png", false, now);
        failed.record_at("http://c/logo.png", true, now);
        failed.record_at("http://d/logo.png", false, now + RETRY_AFTER);
        let kept: Vec<String> = {
            let mut kept: Vec<String> = failed.lock().keys().cloned().collect();
            kept.sort();
            kept
        };
        assert_eq!(kept, ["http://c/logo.png", "http://d/logo.png"]);
    }

    #[test]
    fn which_failures_last() {
        assert!(lasting(&AppError::Image("not an image".into())));
        assert!(lasting(&AppError::InvalidResponse("too large".into())));
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert!(!lasting(&AppError::from(io)));
    }
}
