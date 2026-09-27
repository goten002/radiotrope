//! Stream metadata types, ICY parsing, and source priority
//!
//! Pure data types and parsing functions for ICY (Icecast/Shoutcast) metadata,
//! plus [`MetadataArbiter`], which decides which source's song info is shown
//! when a stream carries more than one, and [`MetadataSink`], which can hold
//! song changes back until playback reaches them.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::config::metadata::{MAX_SYNC_DELAY_SECS, SYNC_POLL_MS};

fn is_country_code(label: &str) -> bool {
    label.len() == 2 && label.bytes().all(|b| b.is_ascii_lowercase())
}

/// Source of stream metadata
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataSource {
    /// ICY `StreamTitle` (Icecast/SHOUTcast)
    Icy,
    /// ID3v2 tag embedded in the stream
    Id3v2,
    /// ID3v1 tag embedded in the stream
    Id3v1,
    /// `title=`/`artist=` attributes on an HLS playlist `#EXTINF` line
    HlsPlaylist,
}

impl MetadataSource {
    /// Priority when several sources are present: higher wins.
    ///
    /// ICY beats ID3, which beats HLS playlist titles. ID3v1 and ID3v2 share
    /// a rank since they come from the same place in the stream.
    pub fn priority(self) -> u8 {
        match self {
            MetadataSource::Icy => 2,
            MetadataSource::Id3v2 | MetadataSource::Id3v1 => 1,
            MetadataSource::HlsPlaylist => 0,
        }
    }
}

/// Parsed stream metadata with artist/title split
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub source: MetadataSource,
}

impl StreamMetadata {
    /// Create metadata, trimming fields and treating blank ones as missing.
    pub fn new(title: Option<String>, artist: Option<String>, source: MetadataSource) -> Self {
        let clean = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Self {
            title: clean(title),
            artist: clean(artist),
            source,
        }
    }

    /// True if there is neither a title nor an artist
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.artist.is_none()
    }

    /// Create metadata from an ICY title string.
    ///
    /// Splits on first ` - ` separator: "Artist - Title" → artist="Artist", title="Title".
    /// If no separator found, the whole string becomes the title. So does
    /// text laid out in fields split by ` :: ` (`Song :: Artist A. - Artist
    /// B. :: 1933`): which field is which isn't said, and a ` - ` inside a
    /// field doesn't split artist from title.
    pub fn from_icy_title(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() {
            return Self {
                title: None,
                artist: None,
                source: MetadataSource::Icy,
            };
        }

        let split = raw.find(" - ").filter(|_| !raw.contains(" :: "));
        if let Some(pos) = split {
            let artist = raw[..pos].trim().to_string();
            let title = raw[pos + 3..].trim().to_string();
            Self {
                title: if title.is_empty() { None } else { Some(title) },
                artist: if artist.is_empty() {
                    None
                } else {
                    Some(artist)
                },
                source: MetadataSource::Icy,
            }
        } else {
            Self {
                title: Some(raw.to_string()),
                artist: None,
                source: MetadataSource::Icy,
            }
        }
    }
}

/// Parse ICY metadata string to extract StreamTitle value.
///
/// ICY metadata format: `StreamTitle='Artist - Song';StreamUrl='...';`
pub fn parse_icy_metadata(metadata: &str) -> Option<String> {
    let start = metadata.find("StreamTitle='")?;
    let start = start + 13; // length of "StreamTitle='"
    let end = metadata[start..].find("';")?;
    let title = metadata[start..start + end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Extract ICY title from a raw metadata block (with null padding).
///
/// Raw ICY metadata blocks are null-padded to a multiple of 16 bytes.
/// This strips null bytes, decodes the text (see [`StationText`]), then
/// parses the StreamTitle.
pub fn extract_icy_title(raw_block: &[u8]) -> Option<String> {
    extract_icy_title_as(raw_block, &mut StationText::default())
}

/// [`extract_icy_title`] for a station whose text `text` decodes
pub fn extract_icy_title_as(raw_block: &[u8], text: &mut StationText) -> Option<String> {
    // Strip null bytes from end
    let end = raw_block
        .iter()
        .rposition(|&b| b != 0)
        .map(|p| p + 1)
        .unwrap_or(0);
    if end == 0 {
        return None;
    }

    parse_icy_metadata(&text.decode(&raw_block[..end])).map(|title| decode_html_references(&title))
}

/// `text` with HTML character references (`&#924;`, `&#x39C;`, `&amp;`)
/// replaced by the characters they stand for.
///
/// Some stations send song titles this way in their ICY metadata: every
/// Greek or Cyrillic letter as a code, or a `'` as `&#39;` so it can't end
/// `StreamTitle='…'`.
/// A reference escaped once more (`&amp;#924;`) is read too. Anything that
/// isn't a complete reference stays as it is (`Simon & Garfunkel`, `R&B`).
pub fn decode_html_references(text: &str) -> String {
    let once = decode_references_once(text);
    if once != text && once.contains("&#") {
        decode_references_once(&once)
    } else {
        once
    }
}

fn decode_references_once(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        match html_reference(rest) {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The character the reference at the start of `text` (at its `&`) stands
/// for, and the reference's length
fn html_reference(text: &str) -> Option<(char, usize)> {
    // The longest is `&#x10FFFF;`, give or take leading zeros
    let end = text.bytes().take(16).position(|b| b == b';')?;
    let name = &text[1..end];
    let c = match name.strip_prefix('#') {
        Some(number) => {
            let (digits, radix) = match number.strip_prefix(['x', 'X']) {
                Some(hex) => (hex, 16),
                None => (number, 10),
            };
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            let code = u32::from_str_radix(digits, radix).ok()?;
            char::from_u32(code).filter(|c| !c.is_control())?
        }
        None => match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => ' ',
            _ => return None,
        },
    };
    Some((c, end + 1))
}

/// Decodes text a station sends without saying how it is encoded: ICY
/// titles and `icy-name`.
///
/// Text that is valid UTF-8 is UTF-8. Anything else is in the legacy
/// encoding the station most likely uses (Windows-1252 in Western Europe,
/// Windows-1253 or ISO-8859-7 for Greek, Windows-1251 for Cyrillic, and so
/// on), guessed from the text itself and a hint to the station's language:
/// its country domain (`.gr`, `.de`), or, when its domain says nothing
/// (`.com`, `.fm`), the listener's region, since most people listen to
/// stations in their own language. The hint settles text that could be
/// either: an all-capitals Greek title reads just as well as lower-case
/// Russian. Keep one per station: the guess steadies as more of its text
/// is seen.
pub struct StationText {
    detector: EncodingDetector,
    /// Country code hinting at the station's language (`gr`, `de`)
    country: Option<String>,
}

/// Country domains used by sites anywhere (`radio.fm`, `.tv`, `.io`),
/// which say nothing about a station's language
const WORLDWIDE_CCTLDS: &[&str] = &[
    "ac", "ai", "bz", "cc", "cd", "co", "cx", "dj", "fm", "gg", "in", "io", "la", "me", "ms", "nu",
    "st", "tk", "to", "tv", "vc", "vu", "ws",
];

impl Default for StationText {
    fn default() -> Self {
        Self::new("", None)
    }
}

impl StationText {
    /// For the station at `url`, heard by this computer's user
    pub fn for_url(url: &str) -> Self {
        Self::new(url, sys_locale::get_locale().as_deref())
    }

    /// For the station at `url`, heard by a user whose locale is `locale`
    /// (`el-GR`, `el_GR.UTF-8`)
    fn new(url: &str, locale: Option<&str>) -> Self {
        let station_country = reqwest::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str()?.rsplit('.').next().map(str::to_string))
            .filter(|label| is_country_code(label) && !WORLDWIDE_CCTLDS.contains(&label.as_str()));
        let user_country = locale
            .and_then(|locale| locale.split(['-', '_']).nth(1))
            .map(|region| {
                region
                    .split(['.', '@'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase()
            })
            .filter(|region| is_country_code(region));
        Self {
            detector: EncodingDetector::new(Iso2022JpDetection::Deny),
            country: station_country.or(user_country),
        }
    }

    pub fn decode(&mut self, bytes: &[u8]) -> String {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return text.to_string();
        }
        self.detector.feed(bytes, false);
        // Keeps the next text from reading as a continuation of this one
        self.detector.feed(b"\n", false);
        let tld = self.country.as_deref().map(str::as_bytes);
        let encoding = self.detector.guess(tld, Utf8Detection::Deny);
        encoding.decode_without_bom_handling(bytes).0.into_owned()
    }
}

/// Decides which source's song info is shown when a stream has several.
///
/// The highest-priority source that currently has song info "holds" the
/// display; updates from lower-priority sources are ignored while it does.
/// An empty update from the holder (e.g. ICY `StreamTitle=''`) releases it,
/// and the latest update a lower-priority source sent meanwhile is shown.
/// Repeated identical updates are dropped.
#[derive(Debug, Default)]
pub struct MetadataArbiter {
    holder: Option<u8>,
    /// Latest update ignored because a higher-priority source held the display
    deferred: Option<StreamMetadata>,
    last_shown: Option<(Option<String>, Option<String>)>,
}

impl MetadataArbiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer an update; returns the update to show, if any.
    pub fn offer(&mut self, meta: StreamMetadata) -> Option<StreamMetadata> {
        let priority = meta.source.priority();

        if meta.is_empty() {
            if self.holder == Some(priority) {
                self.holder = None;
                if let Some(deferred) = self.deferred.take() {
                    return self.show(deferred);
                }
            }
            return None;
        }

        if let Some(holder) = self.holder {
            if priority < holder {
                self.deferred = Some(meta);
                return None;
            }
        }
        if self
            .deferred
            .as_ref()
            .is_some_and(|d| d.source.priority() <= priority)
        {
            self.deferred = None;
        }
        self.show(meta)
    }

    fn show(&mut self, meta: StreamMetadata) -> Option<StreamMetadata> {
        self.holder = Some(meta.source.priority());
        let key = (meta.title.clone(), meta.artist.clone());
        if self.last_shown.as_ref() == Some(&key) {
            return None;
        }
        self.last_shown = Some(key);
        Some(meta)
    }
}

/// Sending side of a stream's metadata channel.
///
/// Every source in a stream (ICY, embedded ID3, HLS playlist) offers its
/// updates here; the shared [`MetadataArbiter`] decides what reaches the
/// receiver. Cheap to clone.
///
/// Readers download ahead of what is heard (the stream buffer, and up to
/// several HLS segments), so a sink made with [`synced`](Self::synced) holds
/// each update until the decoder has read up to the byte offset where it
/// appeared in the stream.
#[derive(Debug, Clone)]
pub struct MetadataSink {
    inner: SinkInner,
}

#[derive(Debug, Clone)]
enum SinkInner {
    /// Updates are sent as soon as they are offered
    Immediate {
        tx: Sender<StreamMetadata>,
        arbiter: Arc<Mutex<MetadataArbiter>>,
    },
    /// Updates go to a scheduler thread that releases them in step with playback
    Synced { tx: Sender<TimedMetadata> },
}

#[derive(Debug)]
struct TimedMetadata {
    at_byte: u64,
    meta: StreamMetadata,
    offered: Instant,
}

impl MetadataSink {
    /// Create a sink whose updates are sent as soon as they are offered.
    pub fn channel() -> (Self, Receiver<StreamMetadata>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let inner = SinkInner::Immediate {
            tx,
            arbiter: Arc::new(Mutex::new(MetadataArbiter::new())),
        };
        (Self { inner }, rx)
    }

    /// Create a sink that holds each update until `playback_position` (bytes
    /// of the stream the decoder has read) reaches the update's offset, or
    /// until [`MAX_SYNC_DELAY_SECS`] have passed.
    pub fn synced(playback_position: Arc<AtomicU64>) -> (Self, Receiver<StreamMetadata>) {
        let (in_tx, in_rx) = crossbeam_channel::unbounded();
        let (out_tx, out_rx) = crossbeam_channel::unbounded();
        let spawned = thread::Builder::new()
            .name("stream-metadata".to_string())
            .spawn(move || run_scheduler(in_rx, out_tx, playback_position));
        match spawned {
            Ok(_) => (
                Self {
                    inner: SinkInner::Synced { tx: in_tx },
                },
                out_rx,
            ),
            // No thread: fall back to unsynced updates rather than none
            Err(_) => Self::channel(),
        }
    }

    /// Offer an update that applies from the start of the stream (or now).
    pub fn offer(&self, meta: StreamMetadata) -> bool {
        self.offer_at(meta, 0)
    }

    /// Offer an update that applies from byte `at_byte` of the reader's
    /// output. Returns false if it was rejected or the receiver is gone
    /// (always true for a synced sink whose scheduler is running).
    pub fn offer_at(&self, meta: StreamMetadata, at_byte: u64) -> bool {
        match &self.inner {
            SinkInner::Immediate { tx, arbiter } => {
                let chosen = arbiter
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .offer(meta);
                match chosen {
                    Some(meta) => tx.send(meta).is_ok(),
                    None => false,
                }
            }
            SinkInner::Synced { tx } => tx
                .send(TimedMetadata {
                    at_byte,
                    meta,
                    offered: Instant::now(),
                })
                .is_ok(),
        }
    }
}

/// Holds offered updates until playback reaches them, then applies the
/// arbiter. Runs until the receiver is dropped, or the stream's readers
/// have stopped and every held update has been released.
fn run_scheduler(
    rx: Receiver<TimedMetadata>,
    out: Sender<StreamMetadata>,
    playback_position: Arc<AtomicU64>,
) {
    let poll = Duration::from_millis(SYNC_POLL_MS);
    let max_delay = Duration::from_secs(MAX_SYNC_DELAY_SECS);
    let mut arbiter = MetadataArbiter::new();
    let mut pending: VecDeque<TimedMetadata> = VecDeque::new();
    let mut producers_open = true;

    loop {
        if producers_open {
            match rx.recv_timeout(poll) {
                Ok(timed) => pending.push_back(timed),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => producers_open = false,
            }
        } else if pending.is_empty() {
            return;
        } else {
            thread::sleep(poll);
        }

        let played = playback_position.load(Ordering::Relaxed);
        while let Some(front) = pending.front() {
            if front.at_byte > played && front.offered.elapsed() < max_delay {
                break;
            }
            let Some(timed) = pending.pop_front() else {
                break;
            };
            if let Some(meta) = arbiter.offer(timed.meta) {
                if out.send(meta).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MetadataSource ---

    #[test]
    fn metadata_source_equality() {
        assert_eq!(MetadataSource::Icy, MetadataSource::Icy);
    }

    #[test]
    fn metadata_source_debug() {
        assert_eq!(format!("{:?}", MetadataSource::Icy), "Icy");
    }

    #[test]
    fn metadata_source_clone() {
        let source = MetadataSource::Icy;
        let cloned = source;
        assert_eq!(source, cloned);
    }

    // --- StreamMetadata ---

    #[test]
    fn stream_metadata_equality() {
        let a = StreamMetadata {
            title: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            source: MetadataSource::Icy,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn stream_metadata_debug() {
        let m = StreamMetadata {
            title: Some("Song".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        };
        let debug = format!("{:?}", m);
        assert!(debug.contains("Song"));
        assert!(debug.contains("Icy"));
    }

    // --- from_icy_title ---

    #[test]
    fn from_icy_title_with_separator() {
        let m = StreamMetadata::from_icy_title("Pink Floyd - Comfortably Numb");
        assert_eq!(m.artist, Some("Pink Floyd".to_string()));
        assert_eq!(m.title, Some("Comfortably Numb".to_string()));
        assert_eq!(m.source, MetadataSource::Icy);
    }

    #[test]
    fn from_icy_title_no_separator() {
        let m = StreamMetadata::from_icy_title("Just A Title");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("Just A Title".to_string()));
    }

    #[test]
    fn from_icy_title_empty() {
        let m = StreamMetadata::from_icy_title("");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, None);
    }

    #[test]
    fn from_icy_title_whitespace_only() {
        let m = StreamMetadata::from_icy_title("   ");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, None);
    }

    #[test]
    fn from_icy_title_multiple_separators() {
        let m = StreamMetadata::from_icy_title("A - B - C");
        assert_eq!(m.artist, Some("A".to_string()));
        assert_eq!(m.title, Some("B - C".to_string()));
    }

    #[test]
    fn from_icy_title_dash_without_spaces() {
        let m = StreamMetadata::from_icy_title("Artist-Title");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("Artist-Title".to_string()));
    }

    #[test]
    fn from_icy_title_with_special_chars() {
        let m = StreamMetadata::from_icy_title("Motörhead - Ace of Spades (Live)");
        assert_eq!(m.artist, Some("Motörhead".to_string()));
        assert_eq!(m.title, Some("Ace of Spades (Live)".to_string()));
    }

    // --- parse_icy_metadata ---

    #[test]
    fn parse_standard_icy_metadata() {
        let raw = "StreamTitle='Pink Floyd - Comfortably Numb';StreamUrl='';";
        assert_eq!(
            parse_icy_metadata(raw),
            Some("Pink Floyd - Comfortably Numb".to_string())
        );
    }

    #[test]
    fn parse_icy_metadata_empty_title() {
        let raw = "StreamTitle='';StreamUrl='';";
        assert_eq!(parse_icy_metadata(raw), None);
    }

    #[test]
    fn parse_icy_metadata_no_stream_title() {
        let raw = "SomeOtherField='value';";
        assert_eq!(parse_icy_metadata(raw), None);
    }

    #[test]
    fn parse_icy_metadata_title_only() {
        let raw = "StreamTitle='Just Music';";
        assert_eq!(parse_icy_metadata(raw), Some("Just Music".to_string()));
    }

    // --- extract_icy_title ---

    #[test]
    fn extract_from_null_padded_block() {
        let mut block = b"StreamTitle='Test Song';".to_vec();
        block.resize(48, 0);
        assert_eq!(extract_icy_title(&block), Some("Test Song".to_string()));
    }

    #[test]
    fn extract_from_all_null_block() {
        let block = vec![0u8; 32];
        assert_eq!(extract_icy_title(&block), None);
    }

    #[test]
    fn extract_from_empty_block() {
        assert_eq!(extract_icy_title(&[]), None);
    }

    // --- from_icy_title edge cases ---

    #[test]
    fn from_icy_title_separator_at_start() {
        // " - Title" after trim → "- Title", no " - " found → title only
        let m = StreamMetadata::from_icy_title(" - Title");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("- Title".to_string()));
    }

    #[test]
    fn from_icy_title_separator_at_end() {
        // "Artist - " after trim → "Artist -", no " - " with content after → title only
        let m = StreamMetadata::from_icy_title("Artist - ");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("Artist -".to_string()));
    }

    #[test]
    fn from_icy_title_only_separator() {
        // " - " after trim → "-", no " - " found → title only
        let m = StreamMetadata::from_icy_title(" - ");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("-".to_string()));
    }

    #[test]
    fn from_icy_title_extra_whitespace_around_separator() {
        let m = StreamMetadata::from_icy_title("  Artist  -  Title  ");
        // outer trim applied first, then split on first " - "
        assert_eq!(m.artist, Some("Artist".to_string()));
        assert_eq!(m.title, Some("Title".to_string()));
    }

    #[test]
    fn from_icy_title_unicode_cjk() {
        let m = StreamMetadata::from_icy_title("アーティスト - 曲名");
        assert_eq!(m.artist, Some("アーティスト".to_string()));
        assert_eq!(m.title, Some("曲名".to_string()));
    }

    #[test]
    fn from_icy_title_greek_with_year() {
        let m = StreamMetadata::from_icy_title("ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022");
        assert_eq!(m.artist, Some("ΠΑΝΟΣ ΚΙΑΜΟΣ".to_string()));
        assert_eq!(m.title, Some("ΘΑ ΜΕ ΖΗΤΑΣ - 2022".to_string()));
        assert_eq!(m.source, MetadataSource::Icy);
    }

    #[test]
    fn from_icy_title_very_long_string() {
        let artist = "A".repeat(500);
        let title = "T".repeat(500);
        let raw = format!("{} - {}", artist, title);
        let m = StreamMetadata::from_icy_title(&raw);
        assert_eq!(m.artist.as_ref().map(|s| s.len()), Some(500));
        assert_eq!(m.title.as_ref().map(|s| s.len()), Some(500));
    }

    #[test]
    fn from_icy_title_newlines_and_tabs() {
        let m = StreamMetadata::from_icy_title("Artist\n - \tTitle");
        // The " - " is preceded by \n and followed by \t
        // find(" - ") finds the literal " - " at position of "\n - "
        // Since outer trim doesn't remove internal whitespace, it depends on where " - " is found
        assert!(m.title.is_some() || m.artist.is_some());
    }

    // --- parse_icy_metadata edge cases ---

    #[test]
    fn parse_icy_metadata_with_url_field() {
        let raw = "StreamTitle='Song Name';StreamUrl='http://example.com';";
        assert_eq!(parse_icy_metadata(raw), Some("Song Name".to_string()));
    }

    #[test]
    fn parse_icy_metadata_whitespace_title() {
        let raw = "StreamTitle='   ';";
        // "   " trimmed becomes empty → None
        assert_eq!(parse_icy_metadata(raw), None);
    }

    #[test]
    fn parse_icy_metadata_special_chars_in_title() {
        let raw = "StreamTitle='Rock & Roll (feat. DJ)';";
        assert_eq!(
            parse_icy_metadata(raw),
            Some("Rock & Roll (feat. DJ)".to_string())
        );
    }

    #[test]
    fn parse_icy_metadata_quotes_in_title() {
        // Title with single quotes: "It's Alright" — the parser finds the FIRST "';",
        // which is "';' Alright';" at the end, so the full title is captured
        let raw = "StreamTitle='It's Alright';";
        assert_eq!(parse_icy_metadata(raw), Some("It's Alright".to_string()));
    }

    #[test]
    fn parse_icy_metadata_greek_title() {
        let raw = "StreamTitle='ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022';StreamUrl='';";
        assert_eq!(
            parse_icy_metadata(raw),
            Some("ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022".to_string())
        );
    }

    #[test]
    fn parse_icy_metadata_missing_closing_quote() {
        let raw = "StreamTitle='No Closing Quote";
        assert_eq!(parse_icy_metadata(raw), None);
    }

    #[test]
    fn parse_icy_metadata_multiple_stream_titles() {
        // Should return the first one
        let raw = "StreamTitle='First';StreamTitle='Second';";
        assert_eq!(parse_icy_metadata(raw), Some("First".to_string()));
    }

    // --- extract_icy_title edge cases ---

    #[test]
    fn extract_from_single_null_byte() {
        assert_eq!(extract_icy_title(&[0]), None);
    }

    #[test]
    fn extract_from_non_utf8_block() {
        // Invalid UTF-8 sequences handled by from_utf8_lossy
        let mut block = vec![0xFF, 0xFE];
        block.extend_from_slice(b"StreamTitle='Fallback';");
        block.resize(48, 0);
        // from_utf8_lossy replaces invalid bytes with replacement char
        // The StreamTitle should still be extractable
        assert_eq!(extract_icy_title(&block), Some("Fallback".to_string()));
    }

    #[test]
    fn extract_from_exact_16_byte_block() {
        // ICY metadata is always multiple of 16 bytes
        let block = b"StreamTitle='A';".to_vec();
        assert_eq!(block.len(), 16); // exactly 16 bytes
        assert_eq!(extract_icy_title(&block), Some("A".to_string()));
    }

    #[test]
    fn extract_from_block_with_interior_nulls() {
        // Null bytes before the actual content shouldn't happen in practice,
        // but rposition finds last non-null
        let mut block = vec![0u8; 48];
        let title = b"StreamTitle='Mid';";
        block[..title.len()].copy_from_slice(title);
        assert_eq!(extract_icy_title(&block), Some("Mid".to_string()));
    }

    #[test]
    fn extract_greek_icy_title_from_null_padded_block() {
        let raw = "StreamTitle='ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022';";
        let mut block = raw.as_bytes().to_vec();
        // Pad to next multiple of 16
        let padded_len = block.len().div_ceil(16) * 16;
        block.resize(padded_len, 0);
        assert_eq!(
            extract_icy_title(&block),
            Some("ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022".to_string())
        );
    }

    // --- StreamMetadata inequality ---

    #[test]
    fn stream_metadata_inequality() {
        let a = StreamMetadata {
            title: Some("A".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        };
        let b = StreamMetadata {
            title: Some("B".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn stream_metadata_none_vs_some() {
        let a = StreamMetadata {
            title: None,
            artist: None,
            source: MetadataSource::Icy,
        };
        let b = StreamMetadata {
            title: Some("Song".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        };
        assert_ne!(a, b);
    }

    // --- StreamMetadata::new / is_empty ---

    #[test]
    fn new_trims_and_drops_blank_fields() {
        let m = StreamMetadata::new(
            Some("  Song ".to_string()),
            Some("   ".to_string()),
            MetadataSource::Id3v2,
        );
        assert_eq!(m.title, Some("Song".to_string()));
        assert_eq!(m.artist, None);
        assert!(!m.is_empty());
        assert!(StreamMetadata::new(None, None, MetadataSource::Icy).is_empty());
    }

    // --- MetadataArbiter ---

    fn meta(source: MetadataSource, artist: &str, title: &str) -> StreamMetadata {
        StreamMetadata::new(Some(title.to_string()), Some(artist.to_string()), source)
    }

    fn empty(source: MetadataSource) -> StreamMetadata {
        StreamMetadata::new(None, None, source)
    }

    fn shown(a: &mut MetadataArbiter, m: StreamMetadata) -> Option<String> {
        a.offer(m).and_then(|m| m.title)
    }

    #[test]
    fn source_priorities() {
        assert!(MetadataSource::Icy.priority() > MetadataSource::Id3v2.priority());
        assert_eq!(
            MetadataSource::Id3v2.priority(),
            MetadataSource::Id3v1.priority()
        );
        assert!(MetadataSource::Id3v1.priority() > MetadataSource::HlsPlaylist.priority());
    }

    #[test]
    fn arbiter_shows_single_source() {
        let mut a = MetadataArbiter::new();
        assert_eq!(
            shown(&mut a, meta(MetadataSource::Id3v2, "A", "1")),
            Some("1".into())
        );
        assert_eq!(
            shown(&mut a, meta(MetadataSource::Id3v2, "A", "2")),
            Some("2".into())
        );
    }

    #[test]
    fn arbiter_drops_duplicates() {
        let mut a = MetadataArbiter::new();
        assert!(a.offer(meta(MetadataSource::Icy, "A", "1")).is_some());
        assert!(a.offer(meta(MetadataSource::Icy, "A", "1")).is_none());
        assert!(a.offer(meta(MetadataSource::Icy, "B", "1")).is_some());
    }

    #[test]
    fn arbiter_ignores_empty_updates() {
        let mut a = MetadataArbiter::new();
        assert!(a.offer(empty(MetadataSource::Icy)).is_none());
        assert!(a.offer(empty(MetadataSource::Id3v2)).is_none());
    }

    #[test]
    fn arbiter_icy_wins_over_id3() {
        let mut a = MetadataArbiter::new();
        assert!(a.offer(meta(MetadataSource::Icy, "A", "icy")).is_some());
        assert!(a.offer(meta(MetadataSource::Id3v2, "B", "id3")).is_none());
        assert!(a.offer(meta(MetadataSource::Id3v1, "B", "id3v1")).is_none());
    }

    #[test]
    fn arbiter_icy_takes_over_from_id3() {
        let mut a = MetadataArbiter::new();
        assert!(a.offer(meta(MetadataSource::Id3v2, "B", "id3")).is_some());
        assert_eq!(
            shown(&mut a, meta(MetadataSource::Icy, "A", "icy")),
            Some("icy".into())
        );
        assert!(a.offer(meta(MetadataSource::Id3v2, "C", "id3 2")).is_none());
    }

    #[test]
    fn arbiter_empty_icy_falls_back_to_id3() {
        let mut a = MetadataArbiter::new();
        assert!(a.offer(meta(MetadataSource::Icy, "A", "icy")).is_some());
        assert!(a.offer(meta(MetadataSource::Id3v2, "B", "id3")).is_none());
        // ICY goes blank: the ID3 song that was held back is shown
        assert_eq!(
            shown(&mut a, empty(MetadataSource::Icy)),
            Some("id3".into())
        );
        // New ID3 updates now go through
        assert_eq!(
            shown(&mut a, meta(MetadataSource::Id3v2, "C", "id3 2")),
            Some("id3 2".into())
        );
        // ICY comes back and wins again
        assert_eq!(
            shown(&mut a, meta(MetadataSource::Icy, "A", "icy 2")),
            Some("icy 2".into())
        );
        assert!(a.offer(meta(MetadataSource::Id3v2, "D", "id3 3")).is_none());
    }

    #[test]
    fn arbiter_empty_icy_without_id3_shows_nothing() {
        let mut a = MetadataArbiter::new();
        assert!(a.offer(meta(MetadataSource::Icy, "A", "icy")).is_some());
        assert!(a.offer(empty(MetadataSource::Icy)).is_none());
        // Same ICY title again after a blank: display still shows it, so no resend
        assert!(a.offer(meta(MetadataSource::Icy, "A", "icy")).is_none());
    }

    #[test]
    fn arbiter_empty_id3_does_not_release_icy() {
        let mut a = MetadataArbiter::new();
        assert!(a.offer(meta(MetadataSource::Icy, "A", "icy")).is_some());
        assert!(a.offer(empty(MetadataSource::Id3v2)).is_none());
        assert!(a.offer(meta(MetadataSource::Id3v2, "B", "id3")).is_none());
    }

    #[test]
    fn arbiter_id3_wins_over_hls_playlist() {
        let mut a = MetadataArbiter::new();
        assert!(a
            .offer(meta(MetadataSource::HlsPlaylist, "A", "pl"))
            .is_some());
        assert!(a.offer(meta(MetadataSource::Id3v2, "B", "id3")).is_some());
        assert!(a
            .offer(meta(MetadataSource::HlsPlaylist, "C", "pl 2"))
            .is_none());
    }

    // --- MetadataSink ---

    #[test]
    fn sink_forwards_chosen_updates() {
        let (sink, rx) = MetadataSink::channel();
        let other = sink.clone();
        assert!(sink.offer(meta(MetadataSource::Icy, "A", "icy")));
        assert!(!other.offer(meta(MetadataSource::Id3v2, "B", "id3")));
        let got: Vec<_> = rx.try_iter().map(|m| m.title.unwrap()).collect();
        assert_eq!(got, vec!["icy".to_string()]);
    }

    #[test]
    fn sink_offer_after_receiver_dropped() {
        let (sink, rx) = MetadataSink::channel();
        drop(rx);
        assert!(!sink.offer(meta(MetadataSource::Icy, "A", "icy")));
    }

    // --- Synced sink ---

    fn recv(rx: &Receiver<StreamMetadata>, ms: u64) -> Option<String> {
        rx.recv_timeout(Duration::from_millis(ms))
            .ok()
            .and_then(|m| m.title)
    }

    #[test]
    fn synced_sink_waits_for_playback() {
        let position = Arc::new(AtomicU64::new(0));
        let (sink, rx) = MetadataSink::synced(position.clone());
        assert!(sink.offer_at(meta(MetadataSource::Id3v2, "A", "first"), 0));
        assert!(sink.offer_at(meta(MetadataSource::Id3v2, "B", "second"), 10_000));
        assert_eq!(recv(&rx, 1000), Some("first".into()));
        // Not played yet
        assert_eq!(recv(&rx, 300), None);
        position.store(9_999, Ordering::Relaxed);
        assert_eq!(recv(&rx, 300), None);
        position.store(10_000, Ordering::Relaxed);
        assert_eq!(recv(&rx, 1000), Some("second".into()));
    }

    #[test]
    fn synced_sink_applies_priority_in_play_order() {
        let position = Arc::new(AtomicU64::new(0));
        let (sink, rx) = MetadataSink::synced(position.clone());
        // ICY title plays first, a later ID3 tag must not replace it
        sink.offer_at(meta(MetadataSource::Icy, "A", "icy"), 100);
        sink.offer_at(meta(MetadataSource::Id3v2, "B", "id3"), 200);
        position.store(1_000, Ordering::Relaxed);
        assert_eq!(recv(&rx, 1000), Some("icy".into()));
        assert_eq!(recv(&rx, 300), None);
    }

    #[test]
    fn synced_sink_releases_pending_after_producers_stop() {
        let position = Arc::new(AtomicU64::new(0));
        let (sink, rx) = MetadataSink::synced(position.clone());
        sink.offer_at(meta(MetadataSource::Id3v2, "A", "later"), 5_000);
        // The reader finished downloading (e.g. HLS VOD) but playback continues
        drop(sink);
        assert_eq!(recv(&rx, 300), None);
        position.store(5_000, Ordering::Relaxed);
        assert_eq!(recv(&rx, 1000), Some("later".into()));
        // Scheduler exits once nothing is pending
        assert!(rx.recv_timeout(Duration::from_millis(1000)).is_err());
    }

    mod html_references {
        use super::*;

        /// A Greek station's title, every letter sent as an HTML code
        const CODED: &str = "&#924;&#940;&#957;&#945; &#945;&#965;&#964;&#972;&#957;&#945; \
            &#952;&#941;&#955;&#969; :: &#924;&#960;&#945;&#961;&#959;&#973;&#963;&#951;&#962; \
            &#913;. - &#917;&#963;&#954;&#949;&#957;&#940;&#950;&#965; &#929;. :: 1933";

        #[test]
        fn a_title_sent_as_html_codes_reads_as_text() {
            assert_eq!(
                decode_html_references(CODED),
                "Μάνα αυτόνα θέλω :: Μπαρούσης Α. - Εσκενάζυ Ρ. :: 1933"
            );
            let block = format!("StreamTitle='{CODED}';");
            let title = extract_icy_title(block.as_bytes()).unwrap();
            assert!(title.starts_with("Μάνα αυτόνα θέλω ::"), "{title}");
        }

        #[test]
        fn reads_hex_named_and_twice_escaped_references() {
            assert_eq!(decode_html_references("&#x39C;&#X3AC;"), "Μά");
            assert_eq!(
                decode_html_references("Tom &amp; Jerry &quot;Live&quot; &lt;3"),
                "Tom & Jerry \"Live\" <3"
            );
            assert_eq!(decode_html_references("&amp;#924;&amp;#940;"), "Μά");
            let block = b"StreamTitle='Rock &#39;n&#39; Roll';StreamUrl='';";
            assert_eq!(extract_icy_title(block).as_deref(), Some("Rock 'n' Roll"));
        }

        #[test]
        fn leaves_what_is_not_a_reference() {
            for text in [
                "Simon & Garfunkel",
                "R&B; Soul",
                "&#; &#x; &#xZZ; &#+5; &#-5; &bogus;",
                "&#0; &#xD800; &#1114112;",
                "AT&T",
                "ends with &",
                "&#924",
                "Ράδιο & Μουσική;",
            ] {
                assert_eq!(decode_html_references(text), text);
            }
        }

        #[test]
        fn a_title_in_fields_is_not_split_at_a_dash_inside_one() {
            let m = StreamMetadata::from_icy_title(
                "Μάνα αυτόνα θέλω :: Μπαρούσης Α. - Εσκενάζυ Ρ. :: 1933",
            );
            assert_eq!(m.artist, None);
            assert_eq!(
                m.title.as_deref(),
                Some("Μάνα αυτόνα θέλω :: Μπαρούσης Α. - Εσκενάζυ Ρ. :: 1933")
            );
        }
    }

    mod station_text {
        use super::*;
        use encoding_rs::{Encoding, WINDOWS_1251, WINDOWS_1252, WINDOWS_1253};

        /// An ICY metadata block with `title` in `encoding`
        fn block(title: &str, encoding: &'static Encoding) -> Vec<u8> {
            let mut block = b"StreamTitle='".to_vec();
            block.extend_from_slice(&encoding.encode(title).0);
            block.extend_from_slice(b"';StreamUrl='';");
            block.resize(block.len().next_multiple_of(16), 0);
            block
        }

        /// The title a listener in the US gets from the station at `url`
        fn title_from(url: &str, title: &str, encoding: &'static Encoding) -> Option<String> {
            heard_in("en-US", url, title, encoding)
        }

        /// The title a listener with `locale` gets from the station at `url`
        fn heard_in(
            locale: &str,
            url: &str,
            title: &str,
            encoding: &'static Encoding,
        ) -> Option<String> {
            let mut text = StationText::new(url, Some(locale));
            extract_icy_title_as(&block(title, encoding), &mut text)
        }

        #[test]
        fn utf8_titles_stay_as_they_are() {
            for title in [
                "Motörhead - Ace of Spades",
                "Άλκηστις Πρωτοψάλτη - Ιθάκη",
                "Кино - Группа крови",
            ] {
                let block = block(title, encoding_rs::UTF_8);
                assert_eq!(extract_icy_title(&block).as_deref(), Some(title));
            }
        }

        #[test]
        fn western_european_titles_are_readable() {
            for title in [
                "Motörhead - Ace of Spades",
                "Édith Piaf - Non, je ne regrette rien",
            ] {
                assert_eq!(
                    title_from("http://stream.example.com/live", title, WINDOWS_1252).as_deref(),
                    Some(title)
                );
            }
        }

        #[test]
        fn greek_titles_are_readable() {
            let mixed_case = ["Άλκηστις Πρωτοψάλτη - Ιθάκη", "Μίκης Θεοδωράκης - Άρνηση"];
            let capitals = "ΜΑΡΙΝΕΛΛΑ - ΣΕ ΘΥΜΑΜΑΙ";
            for url in [
                "http://radio.example.gr:8000/stream",
                "http://stream.example.com/live",
                "http://example.fm/radio",
            ] {
                for title in mixed_case {
                    assert_eq!(
                        title_from(url, title, WINDOWS_1253).as_deref(),
                        Some(title),
                        "from {url}"
                    );
                }
                // All capitals reads as well as lower-case Russian: a Greek
                // domain or a Greek listener settles it
                for locale in ["el-GR", "el_GR.UTF-8"] {
                    assert_eq!(
                        heard_in(locale, url, capitals, WINDOWS_1253).as_deref(),
                        Some(capitals),
                        "from {url} in {locale}"
                    );
                }
            }
            assert_eq!(
                title_from("http://radio.example.gr/", capitals, WINDOWS_1253).as_deref(),
                Some(capitals)
            );
        }

        #[test]
        fn a_greek_listener_still_reads_other_stations_right() {
            for title in ["Die Ärzte - Schrei nach Liebe", "Motörhead - Ace of Spades"] {
                assert_eq!(
                    heard_in("el-GR", "http://stream.example.com/", title, WINDOWS_1252).as_deref(),
                    Some(title)
                );
            }
            // A country domain beats the listener's region. (Without one,
            // Windows-1251 Russian reads as Greek to a Greek listener: the
            // two share byte ranges, and the region decides.)
            let title = "Кино - Группа крови";
            assert_eq!(
                heard_in("el-GR", "http://radio.example.ru/", title, WINDOWS_1251).as_deref(),
                Some(title)
            );
            let title = "Édith Piaf - Non, je ne regrette rien";
            assert_eq!(
                heard_in("el-GR", "http://radio.example.fr/", title, WINDOWS_1252).as_deref(),
                Some(title)
            );
        }

        #[test]
        fn cyrillic_titles_are_readable() {
            let title = "Кино - Группа крови";
            assert_eq!(
                title_from("http://stream.example.ru/live", title, WINDOWS_1251).as_deref(),
                Some(title)
            );
        }

        #[test]
        fn the_station_name_counts_towards_its_titles() {
            let mut text = StationText::for_url("http://stream.example.com/live");
            let name = WINDOWS_1253.encode("Ράδιο Αθήνα 9,84").0;
            assert_eq!(text.decode(&name), "Ράδιο Αθήνα 9,84");
            // One short word could be anything; after the name it is Greek
            let title = "Νύχτα";
            assert_eq!(
                extract_icy_title_as(&block(title, WINDOWS_1253), &mut text).as_deref(),
                Some(title)
            );
        }
    }
}
