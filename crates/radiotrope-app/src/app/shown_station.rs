//! The station the window shows, numbered
//!
//! Every switch, by a Play from the window or one the state poll finds
//! (an agent's, or the controller taking up a Play), gets a new number. A
//! logo downloaded in the background carries the number it was fetched
//! for, and is dropped when it lands after another switch. The shared
//! state alone can't tell: it names an earlier station for a moment when
//! the controller takes up a Play the window has already moved past.

#[derive(Default)]
pub struct ShownStation {
    seq: u64,
    url: String,
}

impl ShownStation {
    /// The window now shows the station at `url`. Returns its number.
    pub fn show(&mut self, url: &str) -> u64 {
        self.seq += 1;
        self.url = url.to_string();
        self.seq
    }

    /// The number of the station shown now
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether the station at `url` isn't the one shown (the poll's check)
    pub fn changed(&self, url: &str) -> bool {
        self.url != url
    }

    /// Whether a logo fetched for `url` while `seq` was shown may still
    /// be shown
    pub fn is_current(&self, seq: u64, url: &str) -> bool {
        self.seq == seq && self.url == url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "http://a.test/live";
    const B: &str = "http://b.test/live";

    #[test]
    fn play_against_a_late_logo() {
        let mut shown = ShownStation::default();
        // The window plays A, then B before A's logo arrived
        let a = shown.show(A);
        let b = shown.show(B);
        assert!(!shown.is_current(a, A));

        // The controller takes up A's Play only now: the poll finds A in
        // the shared state for a moment and shows it, then B again
        assert!(shown.changed(A));
        let a_again = shown.show(A);
        assert!(shown.changed(B));
        let b_again = shown.show(B);

        // Every logo fetched before that last switch is too late; the
        // last one (or the cache it filled) is what shows
        for (seq, url) in [(a, A), (b, B), (a_again, A)] {
            assert!(!shown.is_current(seq, url));
        }
        assert!(shown.is_current(b_again, B));
        assert!(!shown.changed(B));
    }

    #[test]
    fn a_logo_for_the_station_still_shown_lands() {
        let mut shown = ShownStation::default();
        let a = shown.show(A);
        // The poll finds the station the window already shows: no switch
        assert!(!shown.changed(A));
        assert!(shown.is_current(a, A));
        // Another station's logo, fetched meanwhile (an edited favorite)
        assert!(!shown.is_current(shown.seq(), B));
    }
}
