//! Which paired phones have the Radiotrope Remote app connected right now
//!
//! A phone is connected while its app keeps the state stream
//! (`GET /v1/events`) open, which it does for as long as it shows the
//! player, and for [`GRACE`] after that or after any other request it
//! made, so a phone that reconnects its stream or only sends a request now
//! and then doesn't blink in and out. A phone that is listening (it has a
//! WebRTC call) is connected too.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a phone counts as connected after its last request or after
/// its state stream closed
pub const GRACE: Duration = Duration::from_secs(30);

/// A connected phone
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Online {
    /// The paired phone's id
    pub id: String,
    /// It has the station's sound sent to it
    pub listening: bool,
}

#[derive(Debug, Default)]
struct Seen {
    /// State streams open now
    streams: usize,
    /// The last request, or the last stream closing
    last: Option<Instant>,
}

/// What the server saw of each phone
#[derive(Debug, Default)]
pub struct Presence {
    phones: HashMap<String, Seen>,
}

impl Presence {
    /// The phone `id` made a request
    pub fn request(&mut self, id: &str, now: Instant) {
        self.forget_gone(now);
        self.phones.entry(id.to_string()).or_default().last = Some(now);
    }

    /// The phone `id` opened a state stream
    pub fn stream_opened(&mut self, id: &str, now: Instant) {
        self.forget_gone(now);
        let seen = self.phones.entry(id.to_string()).or_default();
        seen.streams += 1;
        seen.last = Some(now);
    }

    /// One of the phone's state streams closed
    pub fn stream_closed(&mut self, id: &str, now: Instant) {
        if let Some(seen) = self.phones.get_mut(id) {
            seen.streams = seen.streams.saturating_sub(1);
            seen.last = Some(now);
        }
    }

    /// The phone `id` is connected at `now`
    pub fn is_connected(&self, id: &str, now: Instant) -> bool {
        self.phones.get(id).is_some_and(|s| s.is_live(now))
    }

    /// Drop the phones gone for longer than the grace, so the map stays
    /// as small as the phones in use
    fn forget_gone(&mut self, now: Instant) {
        self.phones.retain(|_, s| s.is_live(now));
    }
}

impl Seen {
    fn is_live(&self, now: Instant) -> bool {
        self.streams > 0
            || self
                .last
                .is_some_and(|at| now.saturating_duration_since(at) < GRACE)
    }
}

/// Counts a state stream as open for as long as it lives
pub struct StreamGuard {
    presence: Arc<Mutex<Presence>>,
    id: String,
}

impl StreamGuard {
    pub fn open(presence: &Arc<Mutex<Presence>>, id: &str) -> Self {
        lock(presence).stream_opened(id, Instant::now());
        Self {
            presence: presence.clone(),
            id: id.to_string(),
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        lock(&self.presence).stream_closed(&self.id, Instant::now());
    }
}

pub fn lock(presence: &Mutex<Presence>) -> std::sync::MutexGuard<'_, Presence> {
    presence.lock().unwrap_or_else(|e| e.into_inner())
}

/// The phones of `paired` (in its order) that are connected or listening
pub fn online(
    paired: &[&str],
    presence: &Presence,
    listening: &HashSet<String>,
    now: Instant,
) -> Vec<Online> {
    paired
        .iter()
        .filter_map(|&id| {
            let listening = listening.contains(id);
            (listening || presence.is_connected(id, now)).then(|| Online {
                id: id.to_string(),
                listening,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn a_phone_with_its_stream_open_is_connected_without_listening() {
        let t = Instant::now();
        let mut p = Presence::default();
        p.stream_opened("a", t);
        // Long after: the stream is still open
        let later = t + GRACE * 10;
        assert!(p.is_connected("a", later));
        assert_eq!(
            online(&["a", "b"], &p, &none(), later),
            vec![Online {
                id: "a".into(),
                listening: false
            }]
        );
    }

    #[test]
    fn a_listening_phone_is_connected_and_listening() {
        let t = Instant::now();
        let p = Presence::default();
        let listening: HashSet<String> = ["b".to_string()].into();
        assert_eq!(
            online(&["a", "b"], &p, &listening, t),
            vec![Online {
                id: "b".into(),
                listening: true
            }]
        );
    }

    #[test]
    fn a_phone_drops_off_a_grace_after_its_stream_closes() {
        let t = Instant::now();
        let mut p = Presence::default();
        p.stream_opened("a", t);
        let closed = t + Duration::from_secs(100);
        p.stream_closed("a", closed);
        assert!(p.is_connected("a", closed + GRACE - Duration::from_secs(1)));
        assert!(!p.is_connected("a", closed + GRACE));
        assert!(online(&["a"], &p, &none(), closed + GRACE).is_empty());
    }

    #[test]
    fn a_request_alone_keeps_a_phone_connected_for_the_grace() {
        let t = Instant::now();
        let mut p = Presence::default();
        p.request("a", t);
        assert!(p.is_connected("a", t + Duration::from_secs(5)));
        assert!(!p.is_connected("a", t + GRACE));
        // A second stream keeps it on when the first closes
        p.stream_opened("a", t);
        p.stream_opened("a", t);
        p.stream_closed("a", t);
        assert!(p.is_connected("a", t + GRACE * 2));
    }

    #[test]
    fn a_removed_phone_is_gone_at_once() {
        // Only paired phones are listed: removing one in the dialog takes
        // it off even with its stream still counted and a call going
        let t = Instant::now();
        let mut p = Presence::default();
        p.stream_opened("a", t);
        let listening: HashSet<String> = ["a".to_string()].into();
        assert_eq!(online(&["a"], &p, &listening, t).len(), 1);
        assert!(online(&[], &p, &listening, t).is_empty());
    }

    #[test]
    fn gone_phones_are_forgotten() {
        let t = Instant::now();
        let mut p = Presence::default();
        p.request("a", t);
        p.request("b", t + GRACE * 2);
        assert_eq!(p.phones.len(), 1);
        assert!(!p.is_connected("unknown", t));
    }
}
