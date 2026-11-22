use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TryRecvError};
use radiotrope_library::data::types::Station;
use radiotrope_library::providers::{RadiotropeProvider, SearchResults, StationProvider};
use ratatui::widgets::ListState;
use unicode_segmentation::UnicodeSegmentation;

const PAGE_SIZE: usize = 30;
/// A search slower than this shows how long it has been waiting
const SHOW_WAIT_AFTER: Duration = Duration::from_secs(3);
/// The one message for a failed search: the cause goes to the log
pub const SEARCH_FAILED: &str = "Can't reach the station directory. Check your connection.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    generation: u64,
    query: String,
    offset: usize,
}

pub struct Reply {
    pub request: Request,
    pub result: Result<SearchResults, String>,
}

pub struct SearchWorker {
    requests: Sender<Request>,
    queued: Receiver<Request>,
    pub replies: Receiver<Reply>,
}

impl SearchWorker {
    pub fn spawn() -> std::io::Result<Self> {
        let (requests, queued) = bounded::<Request>(1);
        let work = queued.clone();
        let (sender, replies) = unbounded();
        thread::Builder::new()
            .name("station-search".into())
            .spawn(move || {
                let mut provider = None;
                while let Ok(request) = work.recv() {
                    let result = (|| {
                        if provider.is_none() {
                            provider = Some(RadiotropeProvider::new()?);
                        }
                        provider
                            .as_ref()
                            .unwrap()
                            .search(&request.query, PAGE_SIZE, request.offset)
                    })()
                    .map_err(|error| error.to_string());
                    if sender.send(Reply { request, result }).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            queued,
            replies,
        })
    }

    pub fn clear_pending(&self) {
        let _ = self.queued.try_recv();
    }

    pub fn request(&self, request: Request) -> Result<(), String> {
        self.clear_pending();
        self.requests
            .try_send(request)
            .map_err(|error| error.to_string())
    }
}

#[derive(Default)]
pub struct Search {
    pub open: bool,
    pub editing: bool,
    pub query: String,
    /// A byte index on a grapheme boundary.
    pub caret: usize,
    pub rows: Vec<Station>,
    pub list: ListState,
    pub error: Option<String>,
    generation: u64,
    submitted: String,
    offset: usize,
    has_more: bool,
    pending: bool,
    /// When the request in flight was sent
    pending_since: Option<Instant>,
    started: bool,
}

impl Search {
    pub fn open(&mut self) {
        self.open = true;
        self.editing = true;
    }

    pub fn close(&mut self) {
        *self = Self {
            generation: self.generation + 1,
            ..Self::default()
        };
    }

    pub fn previous_boundary(&self) -> usize {
        self.query[..self.caret]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    pub fn next_boundary(&self) -> usize {
        self.query[self.caret..]
            .graphemes(true)
            .next()
            .map_or(self.caret, |part| self.caret + part.len())
    }

    pub fn insert(&mut self, c: char) {
        self.query.insert(self.caret, c);
        self.place_caret(self.caret + c.len_utf8());
    }

    pub fn delete(&mut self, start: usize, end: usize) {
        self.query.replace_range(start..end, "");
        self.place_caret(start);
    }

    fn place_caret(&mut self, at: usize) {
        self.caret = self
            .query
            .grapheme_indices(true)
            .find(|(index, _)| *index >= at)
            .map_or(self.query.len(), |(index, _)| index);
    }

    pub fn submit(&mut self) -> Request {
        self.generation += 1;
        self.submitted = self.query.trim().to_owned();
        self.rows.clear();
        self.list = ListState::default();
        self.started = true;
        self.offset = 0;
        self.has_more = false;
        self.request()
    }

    pub fn retry(&mut self) -> Option<Request> {
        self.error.is_some().then(|| self.request())
    }

    fn request(&mut self) -> Request {
        self.error = None;
        self.pending = true;
        self.pending_since = Some(Instant::now());
        Request {
            generation: self.generation,
            query: self.submitted.clone(),
            offset: self.offset,
        }
    }

    pub fn move_cursor(&mut self, delta: isize, page_at_end: bool) -> Option<Request> {
        let selected = self.list.selected().unwrap_or(0);
        let last = self.rows.len().saturating_sub(1);
        if page_at_end && selected == last && self.has_more && !self.pending && self.error.is_none()
        {
            return Some(self.request());
        }
        if !self.rows.is_empty() {
            self.list
                .select(Some(selected.saturating_add_signed(delta).min(last)));
        }
        None
    }

    /// The directory has more results after these
    pub fn has_more(&self) -> bool {
        self.has_more
    }

    pub fn selected(&self) -> Option<&Station> {
        self.rows.get(self.list.selected()?)
    }

    pub fn status(&self) -> String {
        self.status_at(Instant::now())
    }

    fn status_at(&self, now: Instant) -> String {
        if self.error.is_some() {
            return SEARCH_FAILED.into();
        }
        let waited = self
            .pending_since
            .map(|since| now.saturating_duration_since(since))
            .filter(|waited| *waited >= SHOW_WAIT_AFTER)
            .map(|waited| format!(" {} s", waited.as_secs()))
            .unwrap_or_default();
        match (
            self.started,
            self.pending,
            self.offset,
            self.has_more,
            self.rows.is_empty(),
        ) {
            (false, ..) => "Press Enter with no text for the most-played stations".into(),
            (_, true, 0, ..) => format!("Loading stations…{waited}"),
            (_, true, ..) => format!("Loading more…{waited}"),
            (_, _, _, true, true) => "No stations on this page. Press \u{2193} for more".into(),
            (_, _, _, true, false) => "↓ or PgDn at end for more".into(),
            (_, _, _, false, true) => "No stations found".into(),
            _ => String::new(),
        }
    }

    pub fn poll(&mut self, worker: &SearchWorker) {
        loop {
            match worker.replies.try_recv() {
                Ok(reply) => self.apply(reply),
                Err(TryRecvError::Disconnected) if self.pending => {
                    log::warn!("Station search failed: the search thread ended");
                    self.pending = false;
                    self.pending_since = None;
                    self.error = Some("Search worker unavailable".into());
                    break;
                }
                Err(_) => break,
            }
        }
    }

    pub fn apply(&mut self, reply: Reply) {
        if !self.open
            || !self.pending
            || reply.request.generation != self.generation
            || reply.request.offset != self.offset
        {
            return;
        }
        self.pending = false;
        self.pending_since = None;
        match reply.result {
            Ok(page) => {
                self.offset += PAGE_SIZE;
                self.has_more = page.has_more;
                self.rows.extend(page.stations);
                if !self.rows.is_empty() && self.list.selected().is_none() {
                    self.list.select(Some(0));
                }
            }
            Err(error) => {
                log::warn!("Station search failed: {error}");
                self.error = Some(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(request: Request, stations: Vec<Station>, has_more: bool) -> Reply {
        Reply {
            request,
            result: Ok(SearchResults {
                stations,
                total: None,
                has_more,
            }),
        }
    }

    #[test]
    fn superseded_and_closed_search_replies_are_discarded() {
        let mut search = Search::default();
        search.open();
        search.insert('a');
        let old = search.submit();
        search.insert('b');
        let new = search.submit();
        assert_eq!(new.query, "ab");
        search.apply(page(
            old,
            vec![Station::new("Old", "https://old.test")],
            false,
        ));
        assert_eq!(search.status(), "Loading stations…");
        search.close();
        search.open();
        search.apply(page(
            new,
            vec![Station::new("New", "https://new.test")],
            false,
        ));
        assert!(search.rows.is_empty());
        assert_eq!(
            search.status(),
            "Press Enter with no text for the most-played stations"
        );
    }

    #[test]
    fn paging_advances_by_request_size_and_retries_failed_offset() {
        let mut search = Search::default();
        search.open();
        let first = search.submit();
        search.apply(page(first, vec![], true));
        assert_eq!(
            search.status(),
            "No stations on this page. Press \u{2193} for more"
        );
        let second = search.move_cursor(10, true).unwrap();
        assert_eq!(second.offset, PAGE_SIZE);
        assert!(search.move_cursor(1, true).is_none());
        search.apply(Reply {
            request: second.clone(),
            result: Err("offline".into()),
        });
        assert!(search.move_cursor(1, true).is_none());
        assert_eq!(search.status(), SEARCH_FAILED);
        search.editing = false;
        assert_eq!(search.status(), SEARCH_FAILED);
        let retry = search.retry().unwrap();
        assert_eq!(retry, second);
        search.apply(page(retry, vec![Station::new("A", "https://a.test")], true));
        let third = search.move_cursor(1, true).unwrap();
        assert_eq!(third.offset, 2 * PAGE_SIZE);
        search.apply(page(
            third,
            vec![Station::new("B", "https://b.test")],
            false,
        ));
        assert_eq!(search.list.selected(), Some(0));
        assert_eq!(search.rows.len(), 2);
        assert_eq!(search.status(), "");
    }

    #[test]
    fn a_slow_search_shows_its_wait_after_three_seconds() {
        let mut search = Search::default();
        search.open();
        let request = search.submit();
        let since = search.pending_since.unwrap();
        assert_eq!(search.status_at(since), "Loading stations…");
        assert_eq!(
            search.status_at(since + Duration::from_millis(2900)),
            "Loading stations…"
        );
        assert_eq!(
            search.status_at(since + Duration::from_secs(5)),
            "Loading stations… 5 s"
        );
        search.apply(page(
            request,
            vec![Station::new("A", "https://a.test")],
            true,
        ));
        assert!(search.pending_since.is_none());
        let more = search.move_cursor(1, true).unwrap();
        let since = search.pending_since.unwrap();
        assert_eq!(
            search.status_at(since + Duration::from_secs(12)),
            "Loading more… 12 s"
        );
        search.apply(Reply {
            request: more,
            result: Err("timed out".into()),
        });
        assert_eq!(
            search.status_at(since + Duration::from_secs(40)),
            SEARCH_FAILED
        );
    }
}
