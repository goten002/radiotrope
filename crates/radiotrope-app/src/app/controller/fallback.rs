//! The streams of the station being played, and which is tried when one
//! doesn't connect
//!
//! A station plays its default stream. One that doesn't connect (its
//! address doesn't resolve, or nothing that can be played comes from it)
//! gives way to the station's next stream, in order, and only when the last
//! has failed too does the station show its error. A stream that has played
//! is past this: what happens to it later (a stall, an outage) is the
//! engine's and the reconnects' business, and those start the station again
//! with the stream that last played.

use crate::app::state::AppSnapshot;

/// The streams of the current station, in the order they are tried
#[derive(Debug, Default)]
pub(super) struct StreamList {
    /// The station's own address (its default stream's): what the rest of
    /// the player knows the station by, whichever stream plays
    station: String,
    /// The default stream first, then the backups in order; a stream is
    /// here once
    urls: Vec<String>,
    /// The stream a start begins with: the default, or the one that
    /// last played
    first: usize,
    /// How many streams gave up since the start
    failed: usize,
    /// The stream being tried hasn't played yet
    connecting: bool,
}

impl StreamList {
    /// The streams of the station at `url` (its default stream), with
    /// `backups` to try after it
    pub(super) fn new(url: &str, backups: Vec<String>) -> Self {
        let mut urls = vec![url.to_string()];
        for backup in backups {
            if !backup.trim().is_empty() && !urls.contains(&backup) {
                urls.push(backup);
            }
        }
        Self {
            station: url.to_string(),
            urls,
            first: 0,
            failed: 0,
            connecting: true,
        }
    }

    /// Whether these are the streams of the station at `url`
    pub(super) fn is_station(&self, url: &str) -> bool {
        !self.urls.is_empty() && self.station == url
    }

    /// Which stream is tried or playing: 0 for the default
    pub(super) fn index(&self) -> usize {
        match self.urls.len() {
            0 => 0,
            len => (self.first + self.failed) % len,
        }
    }

    /// How many streams the station has
    pub(super) fn count(&self) -> usize {
        self.urls.len()
    }

    /// The address of the stream tried or playing
    pub(super) fn current(&self) -> &str {
        self.urls
            .get(self.index())
            .map_or(self.station.as_str(), String::as_str)
    }

    /// Say in `state` which stream is tried or playing
    pub(super) fn show(&self, state: &mut AppSnapshot) {
        state.stream_url = Some(self.current().to_string());
        state.stream_index = self.index();
        state.stream_count = self.count();
    }

    /// The station is started again (a reconnect): from the stream that
    /// last played, with every stream to try once more
    pub(super) fn restart(&mut self) {
        self.failed = 0;
        self.connecting = true;
    }

    /// The stream being tried plays: a start begins with it from now on
    pub(super) fn played(&mut self) {
        self.first = self.index();
        self.failed = 0;
        self.connecting = false;
    }

    /// The stream being tried didn't connect: move to the next one and
    /// give its address. `None`, changing nothing, when there is none left
    /// or the stream had played already.
    pub(super) fn next(&mut self) -> Option<&str> {
        if !self.connecting || self.failed + 1 >= self.urls.len() {
            return None;
        }
        self.failed += 1;
        Some(self.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three() -> StreamList {
        StreamList::new("http://a", vec!["http://b".into(), "http://c".into()])
    }

    #[test]
    fn the_default_is_tried_first_then_the_backups_in_order() {
        let mut list = three();
        assert_eq!(
            (list.current(), list.index(), list.count()),
            ("http://a", 0, 3)
        );
        assert_eq!(list.next(), Some("http://b"));
        assert_eq!((list.current(), list.index()), ("http://b", 1));
        assert_eq!(list.next(), Some("http://c"));
        // The last one failed too: the station fails
        assert_eq!(list.next(), None);
        assert_eq!(list.current(), "http://c");
        assert!(list.is_station("http://a"));
        assert!(!list.is_station("http://b"));
    }

    #[test]
    fn a_station_with_one_stream_has_nothing_to_fall_back_to() {
        let mut list = StreamList::new("http://a", Vec::new());
        assert_eq!(list.count(), 1);
        assert_eq!(list.next(), None);
        assert_eq!(list.current(), "http://a");
        // No station yet
        let mut none = StreamList::default();
        assert_eq!((none.count(), none.index(), none.current()), (0, 0, ""));
        assert_eq!(none.next(), None);
        assert!(!none.is_station(""));
    }

    #[test]
    fn a_backup_listed_twice_or_empty_or_the_default_again_is_tried_once() {
        let mut list = StreamList::new(
            "http://a",
            vec![
                "http://b".into(),
                "http://a".into(),
                " ".into(),
                "http://b".into(),
            ],
        );
        assert_eq!(list.count(), 2);
        assert_eq!(list.next(), Some("http://b"));
        assert_eq!(list.next(), None);
    }

    #[test]
    fn a_stream_that_played_isnt_replaced_when_it_fails_later() {
        let mut list = three();
        list.played();
        assert_eq!(list.next(), None);
        assert_eq!(list.current(), "http://a");
    }

    #[test]
    fn a_reconnect_starts_with_the_stream_that_last_played_and_tries_them_all() {
        let mut list = three();
        assert_eq!(list.next(), Some("http://b"));
        list.played();
        // The station is started again after an outage
        list.restart();
        assert_eq!((list.current(), list.index()), ("http://b", 1));
        assert_eq!(list.next(), Some("http://c"));
        assert_eq!(list.next(), Some("http://a"));
        assert_eq!(list.next(), None);
        // And again: the backup that played is still first
        list.restart();
        assert_eq!(list.current(), "http://b");
    }
}
