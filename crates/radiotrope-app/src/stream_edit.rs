//! A station's streams as the window lists them: in Edit Favorite, where
//! the user picks the default, adds, removes and reorders them, and in the
//! station browser's stream choice
//!
//! The dialog holds the list while it is edited; the changes to it are
//! worked out here (Slint can't change a list in place), and so is what the
//! favorite keeps once it is saved.

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use radiotrope_app::data::types::{name_from_url, Station, Stream};
use radiotrope_app::text::SEPARATOR;

use crate::{App, StationStream, StreamEdit};

/// How many streams a favorite can have
pub(crate) const MAX_STREAMS: usize = 12;

/// Why an address can't be added to a list that has it
const ALREADY_LISTED: &str = "This stream is already in the list";

/// A stream's row: its address and what is known of it ("AAC · 128 kbps",
/// with "Offline" for one the directory found down)
pub(crate) fn row(stream: &Stream) -> StationStream {
    let offline = matches!(stream.status.as_deref(), Some("down" | "offline"));
    let label = [
        stream.label(),
        if offline { "Offline" } else { "" }.to_string(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(SEPARATOR);
    StationStream {
        url: stream.url.as_str().into(),
        label: label.into(),
        host: name_from_url(&stream.url).into(),
        codec: stream.codec.as_deref().unwrap_or("").into(),
        bitrate: stream
            .bitrate
            .and_then(|b| i32::try_from(b).ok())
            .unwrap_or(0),
        hls: stream.hls,
        status: stream.status.as_deref().unwrap_or("").into(),
    }
}

/// The stream a row stands for
pub(crate) fn stream_of(row: &StationStream) -> Stream {
    Stream {
        url: row.url.to_string(),
        codec: Some(row.codec.to_string()).filter(|c| !c.is_empty()),
        bitrate: u32::try_from(row.bitrate).ok().filter(|b| *b > 0),
        hls: row.hls,
        status: Some(row.status.to_string()).filter(|s| !s.is_empty()),
    }
}

/// A station's streams as rows, in their order
pub(crate) fn rows_of(station: &Station) -> ModelRc<StationStream> {
    ModelRc::new(VecModel::from(
        station.streams.iter().map(row).collect::<Vec<_>>(),
    ))
}

/// Why the address typed can't be added to `rows`; empty when it can
fn problem(rows: &[StationStream], url: &str) -> String {
    let url = url.trim();
    if rows.iter().any(|row| row.url == url) {
        ALREADY_LISTED.into()
    } else if rows.len() >= MAX_STREAMS {
        format!("A station can have up to {MAX_STREAMS} streams")
    } else {
        String::new()
    }
}

/// `rows` with the address typed at the end; unchanged when it is empty
/// or can't be added (see [`problem`])
fn added(mut rows: Vec<StationStream>, url: &str) -> Vec<StationStream> {
    let url = url.trim();
    if !url.is_empty() && problem(&rows, url).is_empty() {
        rows.push(row(&Stream::new(url)));
    }
    rows
}

/// `rows` without the one at `index`; a station keeps its last stream
fn removed(mut rows: Vec<StationStream>, index: i32) -> Vec<StationStream> {
    if let Ok(index) = usize::try_from(index) {
        if index < rows.len() && rows.len() > 1 {
            rows.remove(index);
        }
    }
    rows
}

/// `rows` with the one at `index` moved `by` places (-1: up, 1: down);
/// unchanged when that would take it off the list
fn moved(mut rows: Vec<StationStream>, index: i32, by: i32) -> Vec<StationStream> {
    let to = index
        .checked_add(by)
        .and_then(|to| usize::try_from(to).ok());
    if let (Ok(from), Some(to)) = (usize::try_from(index), to) {
        if from < rows.len() && to < rows.len() {
            rows.swap(from, to);
        }
    }
    rows
}

/// The streams `station` has once its list was edited to `urls`: the ones
/// it had keep what is known of them, the ones added are just addresses
pub(crate) fn edited_streams(station: &Station, urls: &[String]) -> Vec<Stream> {
    urls.iter()
        .map(|url| {
            station
                .streams
                .iter()
                .find(|stream| stream.url == *url)
                .cloned()
                .unwrap_or_else(|| Stream::new(url.clone()))
        })
        .collect()
}

/// Give Edit Favorite the changes to its list of streams
pub(crate) fn install(ui: &App) {
    let edit = ui.global::<StreamEdit>();
    let list = |rows: ModelRc<StationStream>| rows.iter().collect::<Vec<_>>();
    let model = |rows: Vec<StationStream>| ModelRc::new(VecModel::from(rows));
    edit.on_problem(move |rows, url| problem(&list(rows), url.as_str()).into());
    edit.on_added(move |rows, url| model(added(list(rows), url.as_str())));
    edit.on_removed(move |rows, index| model(removed(list(rows), index)));
    edit.on_moved(move |rows, index, by| model(moved(list(rows), index, by)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(urls: &[&str]) -> Vec<StationStream> {
        urls.iter().map(|url| row(&Stream::new(*url))).collect()
    }

    fn urls(rows: &[StationStream]) -> Vec<String> {
        rows.iter().map(|row| row.url.to_string()).collect()
    }

    #[test]
    fn a_row_says_what_is_known_of_its_stream() {
        let stream = Stream::new("http://a.test").with_audio_info(Some("AAC".into()), Some(128));
        let shown = row(&stream);
        assert_eq!(shown.url, "http://a.test");
        assert_eq!(shown.label, "AAC \u{b7} 128 kbps");
        // Nothing known: the row shows the address alone
        assert_eq!(row(&Stream::new("http://b.test")).label, "");
        // One the directory found down says so; a suspect one still plays
        let status = |status: &str| Stream {
            status: Some(status.into()),
            ..stream.clone()
        };
        assert_eq!(
            row(&status("down")).label,
            "AAC \u{b7} 128 kbps \u{b7} Offline"
        );
        assert_eq!(
            row(&status("offline")).label,
            "AAC \u{b7} 128 kbps \u{b7} Offline"
        );
        assert_eq!(row(&status("suspect")).label, "AAC \u{b7} 128 kbps");
        assert_eq!(row(&status("online")).label, "AAC \u{b7} 128 kbps");
    }

    #[test]
    fn a_row_stands_for_its_stream() {
        let stream = Stream {
            url: "https://stream.example.org:8000/live.aac".into(),
            codec: Some("AAC".into()),
            bitrate: Some(128),
            hls: true,
            status: Some("online".into()),
        };
        let shown = row(&stream);
        assert_eq!(shown.host, "stream.example.org");
        assert_eq!(stream_of(&shown), stream);
        let bare = Stream::new("http://b.test");
        assert_eq!(stream_of(&row(&bare)), bare);
    }

    #[test]
    fn a_station_lists_its_streams_in_order() {
        let station = Station::new("Jazz FM", "http://a.test").with_streams(vec![
            Stream::new("http://a.test").with_audio_info(Some("AAC".into()), Some(128)),
            Stream::new("http://b.test"),
        ]);
        let listed: Vec<StationStream> = rows_of(&station).iter().collect();
        assert_eq!(urls(&listed), ["http://a.test", "http://b.test"]);
        assert_eq!(listed[0].label, "AAC \u{b7} 128 kbps");
    }

    #[test]
    fn an_address_is_added_at_the_end_once() {
        let list = added(rows(&["http://a.test"]), "  http://b.test  ");
        assert_eq!(urls(&list), ["http://a.test", "http://b.test"]);
        // Nothing typed, or listed already: the list stays
        assert_eq!(urls(&added(list.clone(), "   ")), urls(&list));
        assert_eq!(urls(&added(list.clone(), "http://a.test")), urls(&list));
        assert_eq!(problem(&list, " http://a.test "), ALREADY_LISTED);
        assert_eq!(problem(&list, "http://c.test"), "");
        assert_eq!(problem(&list, ""), "");
    }

    #[test]
    fn a_full_list_takes_no_more() {
        let many: Vec<String> = (0..MAX_STREAMS)
            .map(|i| format!("http://{i}.test"))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let list = rows(&many);
        assert_eq!(
            problem(&list, "http://one-more.test"),
            "A station can have up to 12 streams"
        );
        assert_eq!(
            added(list.clone(), "http://one-more.test").len(),
            MAX_STREAMS
        );
    }

    #[test]
    fn a_stream_is_removed_but_never_the_last() {
        let list = removed(rows(&["http://a.test", "http://b.test"]), 0);
        assert_eq!(urls(&list), ["http://b.test"]);
        assert_eq!(urls(&removed(list.clone(), 0)), ["http://b.test"]);
        // No such row
        let two = rows(&["http://a.test", "http://b.test"]);
        assert_eq!(removed(two.clone(), 2).len(), 2);
        assert_eq!(removed(two, -1).len(), 2);
    }

    #[test]
    fn a_stream_moves_up_and_down_within_the_list() {
        let list = rows(&["http://a.test", "http://b.test", "http://c.test"]);
        assert_eq!(
            urls(&moved(list.clone(), 2, -1)),
            ["http://a.test", "http://c.test", "http://b.test"]
        );
        assert_eq!(
            urls(&moved(list.clone(), 0, 1)),
            ["http://b.test", "http://a.test", "http://c.test"]
        );
        // Off either end, or no such row: nothing moves
        for (index, by) in [(0, -1), (2, 1), (3, -1), (-1, 1), (i32::MAX, 1)] {
            assert_eq!(urls(&moved(list.clone(), index, by)), urls(&list));
        }
    }

    #[test]
    fn an_edited_list_keeps_what_was_known_of_the_streams_it_had() {
        let station = Station::new("Jazz FM", "http://a.test").with_streams(vec![
            Stream::new("http://a.test").with_audio_info(Some("AAC".into()), Some(128)),
            Stream {
                hls: true,
                status: Some("online".into()),
                ..Stream::new("http://b.test")
            },
        ]);
        let edited = edited_streams(
            &station,
            &["http://new.test".to_string(), "http://b.test".to_string()],
        );
        assert_eq!(edited[0], Stream::new("http://new.test"));
        assert_eq!(edited[1], station.streams[1]);
        assert_eq!(edited.len(), 2);
    }
}
