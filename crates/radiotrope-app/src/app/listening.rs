//! Listening time of the station playing now, for favorite stats

use std::time::{Duration, Instant};

use radiotrope_app::config::ui::LISTEN_TICK_MAX;

/// A station playing without interruption, counted tick by tick
pub struct ListenSession {
    pub url: String,
    /// Time listened so far
    listened: Duration,
    /// When the last tick counted
    last_tick: Instant,
    /// Seconds of this session already added to the favorite
    pub credited: u64,
}

impl ListenSession {
    pub fn new(url: &str, now: Instant) -> Self {
        Self {
            url: url.to_string(),
            listened: Duration::ZERO,
            last_tick: now,
            credited: 0,
        }
    }

    /// Count the time since the last tick, and return the whole seconds
    /// listened. A gap longer than [`LISTEN_TICK_MAX`] counts as that much:
    /// on Windows the clock runs on while the computer sleeps, and a night
    /// asleep with a station "playing" isn't listening.
    pub fn tick(&mut self, now: Instant) -> u64 {
        let step = now.saturating_duration_since(self.last_tick);
        self.listened += step.min(LISTEN_TICK_MAX);
        self.last_tick = now;
        self.listened.as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_ticks_add_up() {
        let start = Instant::now();
        let mut session = ListenSession::new("http://a", start);
        let mut secs = 0;
        for i in 1..=300 {
            secs = session.tick(start + Duration::from_millis(200 * i));
        }
        assert_eq!(secs, 60);
    }

    #[test]
    fn a_sleep_counts_as_one_short_step() {
        let start = Instant::now();
        let mut session = ListenSession::new("http://a", start);
        for i in 1..=10 {
            session.tick(start + Duration::from_secs(i));
        }
        // Eight hours with the lid closed, then the next poll
        let woke = start + Duration::from_secs(10 + 8 * 3600);
        let secs = session.tick(woke);
        assert_eq!(secs, 10 + LISTEN_TICK_MAX.as_secs());
        // Counting goes on as normal after it
        assert_eq!(
            session.tick(woke + Duration::from_secs(1)),
            11 + LISTEN_TICK_MAX.as_secs()
        );
    }

    #[test]
    fn a_tick_from_the_past_adds_nothing() {
        let start = Instant::now() + Duration::from_secs(5);
        let mut session = ListenSession::new("http://a", start);
        assert_eq!(session.tick(start - Duration::from_secs(1)), 0);
    }
}
