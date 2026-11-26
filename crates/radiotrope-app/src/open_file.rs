//! A playlist file opened with the player (macOS: Finder's Open With, a
//! drop on the Dock icon, `open -a Radiotrope station.m3u`)
//!
//! A station's playlist (`.m3u`, `.m3u8`, `.pls`) is a short text file with
//! the addresses of its streams, the kind a station's "Listen" link
//! downloads. The addresses are read here; playing the first is Open >
//! Network Stream's job.

use std::io::Read;
use std::path::{Path, PathBuf};

use radiotrope::config::hls::MAX_PLAYLIST_BYTES;
use radiotrope::stream::playlist::{m3u_entries, pls_entries, sniff_playlist, PlaylistCheck};

use crate::App;

/// Something opened with the app, from Finder or with `open`
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Opened {
    /// A file on this computer (a playlist)
    File(PathBuf),
    /// An address (`http://...`), as written
    Url(String),
}

impl Opened {
    /// As the log and the message name it: the file's path, the address
    fn shown(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Url(url) => url.clone(),
        }
    }
}

/// Play what was opened in `window`, from the event loop: the system's
/// call that brought it returns first
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn open_later(window: &slint::Weak<App>, opened: Vec<Opened>) {
    let _ = window.upgrade_in_event_loop(move |ui| open(&ui, &opened));
}

/// Show the window and play the first of `opened` that has a station (one
/// plays at a time), as Open > Network Stream does with an address typed
/// in; if none has, say why the first hasn't
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn open(ui: &App, opened: &[Opened]) {
    crate::tray::show_window(ui);
    let mut problem = None;
    for item in opened {
        match station_of(item) {
            Ok(station) => {
                eprintln!("Opened {}: playing {}", item.shown(), station.url);
                ui.invoke_play_stream(
                    station.url.into(),
                    station.name.into(),
                    Default::default(),
                    Default::default(),
                );
                return;
            }
            Err(e) => {
                eprintln!("Opened {}: {}", item.shown(), e.message());
                problem.get_or_insert((item, e));
            }
        }
    }
    if let Some((item, e)) = problem {
        let name = match item {
            Opened::File(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Opened::Url(url) => url.clone(),
        };
        crate::import_ui::show_file_message(ui, "Open Playlist", &e.message(), &name, true);
    }
}

/// What Open > Network Stream is given for an opened file or address
#[derive(Debug, PartialEq, Eq)]
struct Station {
    /// The stream to play: the file's first
    url: String,
    /// The file's name; "" for an address
    name: String,
}

/// The station an opened file or address stands for: a file's first
/// stream (a station's playlist lists the same programme's mirrors)
fn station_of(opened: &Opened) -> Result<Station, OpenError> {
    let (name, urls) = match opened {
        Opened::File(path) => {
            let playlist = read(path)?;
            (playlist.name, playlist.urls)
        }
        Opened::Url(url) => (String::new(), web_addresses(vec![url.trim().to_string()])),
    };
    let url = urls.into_iter().next().ok_or(OpenError::NotWeb)?;
    Ok(Station { url, name })
}

/// What an opened playlist file leads to
#[derive(Debug, PartialEq, Eq)]
pub struct OpenedPlaylist {
    /// The file's name without its extension: the station's name until the
    /// stream says its own
    pub name: String,
    /// The stream addresses in the file's order, at least one
    pub urls: Vec<String>,
}

/// Why a file can't be played
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The file can't be read; the system's reason
    Unreadable(String),
    /// A playlist of a stream's parts (HLS), which only plays from its
    /// address on the web
    Hls,
    /// No `http://` or `https://` address in it
    NoStreams,
    /// An address of another kind (`mms://`, `rtsp://`)
    NotWeb,
}

impl OpenError {
    /// What to tell the user
    pub fn message(&self) -> String {
        match self {
            Self::Unreadable(reason) => format!("Can't read the file: {reason}."),
            Self::Hls => "This is an HLS playlist, which plays only from its address on the \
                          web. Open that address with Open > Network Stream."
                .into(),
            Self::NoStreams => "The file has no stream addresses (http:// or https://).".into(),
            Self::NotWeb => "Only http:// and https:// addresses can be played.".into(),
        }
    }
}

/// Read the stream addresses of the playlist file at `path`
pub fn read(path: &Path) -> Result<OpenedPlaylist, OpenError> {
    let unreadable = |e: std::io::Error| OpenError::Unreadable(radiotrope_app::text::io_reason(&e));
    let file = std::fs::File::open(path).map_err(unreadable)?;
    // A station's playlist is a few lines; whatever else was opened is not
    // read to its end
    let mut bytes = Vec::new();
    file.take(MAX_PLAYLIST_BYTES as u64)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    let is_pls = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pls"));
    let urls = stream_urls(&bytes, is_pls)?;
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().trim().to_string())
        .unwrap_or_default();
    Ok(OpenedPlaylist { name, urls })
}

/// The stream addresses in a playlist's bytes. The content says what kind
/// it is; `named_pls` (a `.pls` file name) decides for one that doesn't
/// start as either kind does.
fn stream_urls(bytes: &[u8], named_pls: bool) -> Result<Vec<String>, OpenError> {
    let text = String::from_utf8_lossy(bytes);
    let urls = match sniff_playlist(None, bytes) {
        PlaylistCheck::Hls => return Err(OpenError::Hls),
        PlaylistCheck::Pls => pls_entries(&text),
        PlaylistCheck::M3u => web_addresses(m3u_entries(&text, "")),
        PlaylistCheck::NotPlaylist if named_pls => pls_entries(&text),
        PlaylistCheck::NotPlaylist => web_addresses(m3u_entries(&text, "")),
    };
    if urls.is_empty() {
        Err(OpenError::NoStreams)
    } else {
        Ok(urls)
    }
}

/// Without the entries that were paths beside the file (a music player's
/// playlist of local files): there is no address to resolve them against
fn web_addresses(entries: Vec<String>) -> Vec<String> {
    entries
        .into_iter()
        .filter(|entry| {
            let start = entry.get(..8).unwrap_or(entry).to_ascii_lowercase();
            start.starts_with("http://") || start.starts_with("https://")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_m3u_gives_its_addresses_in_order() {
        let m3u = b"#EXTM3U\r\n#EXTINF:-1,Jazz FM\r\nhttp://a.example/jazz.mp3\r\n\
                    https://b.example/jazz.aac?token=1&x=2\r\n";
        assert_eq!(
            stream_urls(m3u, false).unwrap(),
            [
                "http://a.example/jazz.mp3",
                "https://b.example/jazz.aac?token=1&x=2"
            ]
        );
        // Without the header, as many stations write it
        assert_eq!(
            stream_urls(b"http://a.example/live\n", false).unwrap(),
            ["http://a.example/live"]
        );
    }

    #[test]
    fn a_pls_gives_its_addresses_whatever_the_file_is_called() {
        let pls = b"[playlist]\nNumberOfEntries=2\nFile1=http://a.example/one\n\
                    Title1=One\nFile2=http://a.example/two\nVersion=2\n";
        let urls = ["http://a.example/one", "http://a.example/two"];
        assert_eq!(stream_urls(pls, true).unwrap(), urls);
        assert_eq!(stream_urls(pls, false).unwrap(), urls);
        // No `[playlist]` line: the name decides
        assert_eq!(
            stream_urls(b"File1=http://a.example/one\n", true).unwrap(),
            ["http://a.example/one"]
        );
    }

    #[test]
    fn a_playlist_of_local_files_has_nothing_to_play() {
        let music = b"#EXTM3U\n#EXTINF:215,Song\nMusic/song.mp3\n/Users/anna/Music/other.flac\n\
                      C:\\Music\\third.mp3\nfile:///Users/anna/fourth.mp3\n";
        assert_eq!(stream_urls(music, false), Err(OpenError::NoStreams));
        assert_eq!(stream_urls(b"", false), Err(OpenError::NoStreams));
        assert_eq!(
            stream_urls(b"\x00\x01\x02\xff", true),
            Err(OpenError::NoStreams)
        );
        // The web addresses among them are kept
        let mixed = b"#EXTM3U\nMusic/song.mp3\nhttp://a.example/live\n";
        assert_eq!(
            stream_urls(mixed, false).unwrap(),
            ["http://a.example/live"]
        );
    }

    #[test]
    fn an_hls_playlist_is_told_apart() {
        let hls = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nseg1.ts\n";
        assert_eq!(stream_urls(hls, false), Err(OpenError::Hls));
        assert!(OpenError::Hls.message().contains("Network Stream"));
    }

    #[test]
    fn an_opened_playlist_is_its_first_stream_named_after_the_file() {
        let dir = std::env::temp_dir().join(format!("rt-open-station-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Jazz FM.pls");
        std::fs::write(
            &path,
            "[playlist]\nFile1=http://a.example/one\nFile2=http://b.example/two\n",
        )
        .unwrap();

        let station = station_of(&Opened::File(path.clone())).unwrap();
        assert_eq!(station.url, "http://a.example/one");
        assert_eq!(station.name, "Jazz FM");
        assert_eq!(
            Opened::File(path.clone()).shown(),
            path.display().to_string()
        );

        let gone = station_of(&Opened::File(dir.join("gone.pls"))).unwrap_err();
        assert!(matches!(gone, OpenError::Unreadable(_)), "{gone:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_opened_address_plays_if_it_is_a_web_address() {
        let station = station_of(&Opened::Url(" https://a.example/live ".into())).unwrap();
        assert_eq!(station.url, "https://a.example/live");
        assert_eq!(station.name, "");

        let other = station_of(&Opened::Url("rtsp://a.example/live".into())).unwrap_err();
        assert_eq!(other, OpenError::NotWeb);
        assert!(other.message().contains("http://"));
    }

    #[test]
    fn a_file_is_read_with_its_name_as_the_stations() {
        let dir = std::env::temp_dir().join(format!("rt-open-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("Jazz FM (live).m3u8");
        std::fs::write(&path, "\u{feff}#EXTM3U\nhttp://a.example/jazz\n").unwrap();
        assert_eq!(
            read(&path).unwrap(),
            OpenedPlaylist {
                name: "Jazz FM (live)".into(),
                urls: vec!["http://a.example/jazz".into()],
            }
        );

        let path = dir.join("LISTEN.PLS");
        std::fs::write(&path, "File1=https://a.example/one\n").unwrap();
        assert_eq!(read(&path).unwrap().urls, ["https://a.example/one"]);

        let missing = read(&dir.join("gone.m3u")).unwrap_err();
        assert!(
            matches!(&missing, OpenError::Unreadable(reason) if !reason.contains("os error")),
            "{missing:?}"
        );
        assert!(missing.message().starts_with("Can't read the file: "));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
