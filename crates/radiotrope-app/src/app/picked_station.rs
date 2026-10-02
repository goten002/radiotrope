//! A station picked in the window that the controller hasn't taken yet
//!
//! A pick shows in the window at once, but the controller first stops the
//! old station (and finishes its recording) before it takes up the Play.
//! Until then the shared state still holds the old station, playing, and
//! the state poll would put it back for a moment. A pick knows the
//! `play_seq` its station gets when taken, so the poll shows the pick
//! until the shared state reaches it.

use std::time::{Duration, Instant};

/// A station picked in the window, waiting for the controller
#[derive(Debug, Clone)]
pub struct PickedStation {
    /// The `play_seq` the station gets once the controller takes it
    seq: u64,
    pub url: String,
    pub name: Option<String>,
    at: Instant,
}

/// The latest pick still waiting, if any
#[derive(Debug, Default)]
pub struct PendingPick(Option<PickedStation>);

impl PendingPick {
    /// A station was picked while the shared state's `play_seq` was
    /// `play_seq`. Read it before sending the Play: once sent, the
    /// controller may take it at any moment.
    pub fn pick(&mut self, play_seq: u64, url: &str, name: Option<String>, now: Instant) {
        // An earlier pick the controller hasn't taken yet gets its number first
        let seq = match &self.0 {
            Some(earlier) if earlier.seq > play_seq => earlier.seq + 1,
            _ => play_seq + 1,
        };
        self.0 = Some(PickedStation {
            seq,
            url: url.to_string(),
            name,
            at: now,
        });
    }

    /// The pick to show while the shared state is at `play_seq`. None once
    /// the controller took it (or a later Play, an agent's), or after
    /// `timeout`: a controller that never takes it is stuck, and the
    /// state then shows what it holds.
    pub fn waiting(
        &mut self,
        play_seq: u64,
        now: Instant,
        timeout: Duration,
    ) -> Option<&PickedStation> {
        let waits = self
            .0
            .as_ref()
            .is_some_and(|p| play_seq < p.seq && now.duration_since(p.at) < timeout);
        if !waits {
            self.0 = None;
        }
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "http://a.test/live";
    const B: &str = "http://b.test/live";
    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn a_pick_shows_until_the_controller_takes_it() {
        let now = Instant::now();
        let mut pending = PendingPick::default();
        assert!(pending.waiting(4, now, TIMEOUT).is_none());

        pending.pick(4, A, Some("A FM".into()), now);
        // The controller is still stopping the old station
        let picked = pending.waiting(4, now, TIMEOUT).unwrap();
        assert_eq!(picked.url, A);
        assert_eq!(picked.name.as_deref(), Some("A FM"));

        // It took the Play: the shared state shows the pick from now on
        assert!(pending.waiting(5, now, TIMEOUT).is_none());
        assert!(pending.waiting(4, now, TIMEOUT).is_none());
    }

    #[test]
    fn two_quick_picks_show_the_last_one() {
        let now = Instant::now();
        let mut pending = PendingPick::default();
        pending.pick(4, A, None, now);
        pending.pick(4, B, None, now);
        assert_eq!(pending.waiting(4, now, TIMEOUT).unwrap().url, B);
        // The controller took A's Play; the state holds A, B is still due
        assert_eq!(pending.waiting(5, now, TIMEOUT).unwrap().url, B);
        assert!(pending.waiting(6, now, TIMEOUT).is_none());
    }

    #[test]
    fn a_pick_after_a_taken_one_waits_for_its_own() {
        let now = Instant::now();
        let mut pending = PendingPick::default();
        pending.pick(4, A, None, now);
        // A was taken before the next poll looked
        pending.pick(5, B, None, now);
        assert_eq!(pending.waiting(5, now, TIMEOUT).unwrap().url, B);
        assert!(pending.waiting(6, now, TIMEOUT).is_none());
    }

    #[test]
    fn an_agent_play_taken_first_ends_the_wait() {
        let now = Instant::now();
        let mut pending = PendingPick::default();
        pending.pick(4, A, None, now);
        // An agent's Play reached the controller first: the window shows
        // it, then the pick once taken (the poll finds it in the state)
        assert!(pending.waiting(5, now, TIMEOUT).is_none());
    }

    #[test]
    fn a_stuck_controller_lets_the_state_show() {
        let now = Instant::now();
        let mut pending = PendingPick::default();
        pending.pick(4, A, None, now);
        assert!(pending.waiting(4, now + TIMEOUT / 2, TIMEOUT).is_some());
        assert!(pending.waiting(4, now + TIMEOUT, TIMEOUT).is_none());
    }
}
