//! Settings saved a moment after they change
//!
//! The window's settings (volume, EQ, theme, view, window size...) used to
//! be written only when a station started and at exit. A process that ends
//! without its exit code (a Windows logoff, a crash, a kill, a power cut)
//! lost every change since. The poll hands its view of them here each tick,
//! and gets them back to save once they have stayed the same for a moment,
//! so a slider drag or a window resize is written once.

use std::time::{Duration, Instant};

pub struct SettingsWatch<T> {
    delay: Duration,
    /// What was saved last, or seen first (what was loaded)
    saved: Option<T>,
    /// A change not saved yet, and when it was last different
    pending: Option<(T, Instant)>,
}

impl<T: Clone + PartialEq> SettingsWatch<T> {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            saved: None,
            pending: None,
        }
    }

    /// The settings as they are `now`. Returns them when they are due to
    /// be saved: changed, then left alone for the delay.
    pub fn note(&mut self, settings: T, now: Instant) -> Option<T> {
        let Some(saved) = &self.saved else {
            self.saved = Some(settings);
            return None;
        };
        if settings == *saved {
            // Changed back before it was saved
            self.pending = None;
            return None;
        }
        match &self.pending {
            Some((pending, _)) if *pending == settings => {}
            _ => self.pending = Some((settings, now)),
        }
        let (_, since) = self.pending.as_ref()?;
        if now.saturating_duration_since(*since) < self.delay {
            return None;
        }
        let (settings, _) = self.pending.take()?;
        self.saved = Some(settings.clone());
        Some(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAY: Duration = Duration::from_secs(2);

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn what_was_loaded_is_not_saved_again() {
        let start = Instant::now();
        let mut watch = SettingsWatch::new(DELAY);
        assert_eq!(watch.note(1, start), None);
        assert_eq!(watch.note(1, at(start, 5000)), None);
    }

    #[test]
    fn a_change_is_saved_once_it_has_settled() {
        let start = Instant::now();
        let mut watch = SettingsWatch::new(DELAY);
        watch.note(1, start);
        assert_eq!(watch.note(2, at(start, 200)), None);
        assert_eq!(watch.note(2, at(start, 2000)), None);
        assert_eq!(watch.note(2, at(start, 2200)), Some(2));
        // Saved: nothing more to do
        assert_eq!(watch.note(2, at(start, 9000)), None);
    }

    #[test]
    fn a_drag_is_saved_once_when_it_stops() {
        let start = Instant::now();
        let mut watch = SettingsWatch::new(DELAY);
        watch.note(0, start);
        for step in 1..=20 {
            assert_eq!(watch.note(step, at(start, step * 200)), None);
        }
        assert_eq!(watch.note(20, at(start, 4000 + 2000)), Some(20));
    }

    #[test]
    fn a_change_undone_in_time_is_not_saved() {
        let start = Instant::now();
        let mut watch = SettingsWatch::new(DELAY);
        watch.note(1, start);
        watch.note(2, at(start, 200));
        assert_eq!(watch.note(1, at(start, 400)), None);
        assert_eq!(watch.note(1, at(start, 5000)), None);
    }
}
