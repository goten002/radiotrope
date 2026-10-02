//! Playlist parsing (PLS/M3U)
//!
//! Resolves playlist URLs to stream URLs, handling PLS and M3U formats.

use std::time::Duration;

use crate::config::hls::MAX_PLAYLIST_BYTES;
use crate::config::network::{
    CONNECT_TIMEOUT_SECS, MAX_PLAYLIST_DEPTH, MAX_PLAYLIST_ENTRIES, USER_AGENT,
};
use crate::error::{RadioError, Result};
use crate::stream::cancel::StreamCancel;
use crate::stream::hls::{read_body, too_large};
use crate::stream::Deadline;

/// Result of checking a URL's playlist type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistCheck {
    Pls,
    M3u,
    Hls,
    NotPlaylist,
}

/// Check what type of playlist a URL points to based on extension.
/// Stations also serve playlists from URLs without one (`/listen.php?id=7`,
/// `/radio`): [`sniff_playlist`] tells from the response.
pub fn check_playlist_type(url: &str) -> PlaylistCheck {
    let lower = url.to_lowercase();
    if lower.ends_with(".m3u8") || lower.contains(".m3u8?") {
        PlaylistCheck::Hls
    } else if lower.ends_with(".pls") || lower.contains(".pls?") {
        PlaylistCheck::Pls
    } else if lower.ends_with(".m3u") || lower.contains(".m3u?") {
        PlaylistCheck::M3u
    } else {
        PlaylistCheck::NotPlaylist
    }
}

/// What a response is, from its `Content-Type` and the first bytes of its
/// body: a playlist of some kind, or (as far as can be told) audio.
///
/// The body decides when it starts like a playlist (`#EXTM3U`,
/// `[playlist]`); otherwise a playlist `Content-Type` does, if the body is
/// text. An M3U with `#EXT-X-` tags is HLS, and only one with them: an HLS
/// `Content-Type` or a `.m3u8` name is also given to plain M3U lists.
pub fn sniff_playlist(content_type: Option<&str>, head: &[u8]) -> PlaylistCheck {
    let text = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let text = text.trim_ascii_start();
    let starts_with = |prefix: &[u8]| {
        text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    let hls_tags = text.windows(7).any(|w| w.eq_ignore_ascii_case(b"#EXT-X-"));
    let m3u = if hls_tags {
        PlaylistCheck::Hls
    } else {
        PlaylistCheck::M3u
    };

    if starts_with(b"#EXTM3U") || starts_with(b"#EXT-X-") {
        return m3u;
    }
    if starts_with(b"[playlist]") {
        return PlaylistCheck::Pls;
    }
    // Audio is binary; a playlist is lines of text
    let is_text = !text.is_empty()
        && text
            .iter()
            .all(|&b| b >= 0x20 || matches!(b, b'\t' | b'\n' | b'\r'));
    if !is_text {
        return PlaylistCheck::NotPlaylist;
    }
    let mime = content_type
        .and_then(|ct| ct.split(';').next())
        .map(|ct| ct.trim().to_ascii_lowercase());
    match mime.as_deref() {
        Some(
            "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/x-mpegurl"
            | "audio/mpegurl",
        ) => m3u,
        Some("audio/x-scpls" | "audio/scpls" | "application/pls+xml") => PlaylistCheck::Pls,
        // Just a stream's address, as some station pages hand out
        _ if starts_with(b"http://") || starts_with(b"https://") => PlaylistCheck::M3u,
        _ => PlaylistCheck::NotPlaylist,
    }
}

/// Whether a reply's `Content-Type` is a web page's
pub(crate) fn is_web_page(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
}

/// Where a station's playlists lead
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub url: String,
    /// An HLS playlist; otherwise a stream to connect to (or so it seems:
    /// the stream's response can still turn out to be a playlist)
    pub hls: bool,
}

/// Extract the base URL (directory) from a full URL
pub fn get_base_url(url: &str) -> String {
    url.rsplit_once('/')
        .map(|(base, _)| base)
        .unwrap_or("")
        .to_string()
}

/// True for an absolute `http://` or `https://` URL (scheme in any case)
fn is_http_url(s: &str) -> bool {
    let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Make a URI absolute, using base_url (a directory, without the trailing
/// slash) if the URI is relative.
///
/// Resolves like a browser (RFC 3986): `/abs/path`, `../up` and
/// `//host/path` work, not just names in the same directory.
pub fn make_absolute_url(uri: &str, base_url: &str) -> String {
    if is_http_url(uri) {
        return uri.to_string();
    }
    match reqwest::Url::parse(&format!("{}/", base_url)).and_then(|base| base.join(uri)) {
        Ok(url) => url.to_string(),
        Err(_) => format!("{}/{}", base_url, uri),
    }
}

/// Drop a UTF-8 byte order mark, which some playlist editors write
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// True for a URI with a scheme of its own (`mms:`, `rtsp:`, `file:`), or a
/// drive letter (`C:\Music`): not one to play or to resolve against the
/// playlist's address
fn has_scheme(uri: &str) -> bool {
    uri.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// `entries` without repeats, at most [`MAX_PLAYLIST_ENTRIES`] of them
fn first_entries(entries: impl Iterator<Item = String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for entry in entries {
        if kept.len() == MAX_PLAYLIST_ENTRIES {
            break;
        }
        if !kept.contains(&entry) {
            kept.push(entry);
        }
    }
    kept
}

/// Parse a PLS playlist and return the first stream URL
pub fn parse_pls(content: &str) -> Option<String> {
    pls_entries(content).into_iter().next()
}

/// The stream URLs of a PLS playlist in order (the first
/// [`MAX_PLAYLIST_ENTRIES`]): a station's mirrors, tried in turn
pub fn pls_entries(content: &str) -> Vec<String> {
    first_entries(strip_bom(content).lines().filter_map(|line| {
        let line = line.trim();
        if !line.to_lowercase().starts_with("file") {
            return None;
        }
        // Split at the first '=' only: stream URLs often carry
        // `?key=value&…` query strings (tokens, session ids)
        let (_, stream_url) = line.split_once('=')?;
        let stream_url = stream_url.trim();
        is_http_url(stream_url).then(|| stream_url.to_string())
    }))
}

/// A `key=value` line, which some M3U files carry between entries.
///
/// A relative entry can hold an `=` too, but only after a `/` or `?`
/// (`live.mp3?sid=1`, `//host/path?x=1`), never in a plain name before it.
fn is_setting(line: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(name, _)| !name.contains(['/', '?']))
}

/// Parse an M3U playlist and return the first stream URL
pub fn parse_m3u(content: &str, base_url: &str) -> Option<String> {
    m3u_entries(content, base_url).into_iter().next()
}

/// The stream URLs of an M3U playlist in order (the first
/// [`MAX_PLAYLIST_ENTRIES`]), relative ones made absolute against
/// `base_url`. Entries of other schemes (`mms://`, `rtsp://`, local files)
/// are left out, and so are the tags of a web page served as a playlist.
pub fn m3u_entries(content: &str, base_url: &str) -> Vec<String> {
    first_entries(
        strip_bom(content)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with(['#', '<']))
            .filter_map(|line| {
                if is_http_url(line) {
                    Some(line.to_string())
                } else if has_scheme(line) || is_setting(line) {
                    None
                } else {
                    Some(make_absolute_url(line, base_url))
                }
            }),
    )
}

/// Resolve a playlist URL to its final stream URL, following chains recursively.
///
/// M3U8 (HLS) URLs pass through unchanged: the HLS resolve fetches them, and
/// hands back a plain M3U list named so. PLS and M3U playlists are fetched
/// and parsed, recursing up to `MAX_PLAYLIST_DEPTH` levels. Of a playlist's
/// entries, the first that leads somewhere is taken (an entry that is a
/// playlist can fail to load).
pub fn resolve_playlist_url(url: &str) -> Result<String> {
    Ok(resolve_playlist(url, &StreamCancel::new(), Deadline::NONE)?.url)
}

/// [`resolve_playlist_url`], stopped by `cancel` between fetches and done by
/// `deadline`, saying whether it ends at an HLS playlist
pub(crate) fn resolve_playlist(
    url: &str,
    cancel: &StreamCancel,
    deadline: Deadline,
) -> Result<Target> {
    resolve_recursive(url, None, MAX_PLAYLIST_DEPTH, cancel, deadline)
}

/// [`resolve_playlist`] for a URL known to serve a playlist of kind `kind`
/// whatever its extension
pub(crate) fn resolve_playlist_as(
    url: &str,
    kind: PlaylistCheck,
    cancel: &StreamCancel,
    deadline: Deadline,
) -> Result<Target> {
    resolve_recursive(url, Some(kind), MAX_PLAYLIST_DEPTH, cancel, deadline)
}

fn resolve_recursive(
    url: &str,
    kind: Option<PlaylistCheck>,
    depth: usize,
    cancel: &StreamCancel,
    deadline: Deadline,
) -> Result<Target> {
    if cancel.is_cancelled() {
        return Err(RadioError::Cancelled);
    }
    if depth == 0 {
        return Err(RadioError::Stream("Playlist nesting too deep".to_string()));
    }
    let hls = match read_playlist(url, kind, cancel, deadline)? {
        Step::Hls => true,
        Step::Stream => false,
        Step::Entries(entries) => {
            return first_that_works(&entries, deadline, |entry| {
                resolve_recursive(entry, None, depth - 1, cancel, deadline)
            })
        }
    };
    Ok(Target {
        url: url.to_string(),
        hls,
    })
}

/// What an address leads to
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// An HLS playlist
    Hls,
    /// Not a playlist by its name: a stream to connect to (whose response
    /// can still turn out to be a playlist)
    Stream,
    /// The stream addresses of a PLS or M3U playlist, in order
    Entries(Vec<String>),
}

/// What `url` leads to, read as a playlist of `kind` (by default the kind
/// its name says). A PLS or M3U is fetched, stopped by `cancel` and done by
/// `deadline`, and what it is decides, not what it is called: an `.m3u` can
/// be HLS.
pub(crate) fn read_playlist(
    url: &str,
    kind: Option<PlaylistCheck>,
    cancel: &StreamCancel,
    deadline: Deadline,
) -> Result<Step> {
    let by_extension = kind.unwrap_or_else(|| check_playlist_type(url));
    match by_extension {
        PlaylistCheck::Hls => return Ok(Step::Hls),
        PlaylistCheck::NotPlaylist => return Ok(Step::Stream),
        PlaylistCheck::Pls | PlaylistCheck::M3u => {}
    }
    deadline.check()?;
    let playlist = fetch_playlist(url, cancel, deadline)?;
    let content_type = playlist.content_type.as_deref();
    let kind = match sniff_playlist(content_type, &playlist.body) {
        // A station's error page in place of its playlist
        PlaylistCheck::NotPlaylist if is_web_page(content_type) => {
            return Err(RadioError::Stream(
                "the server sent a web page instead of a playlist".to_string(),
            ))
        }
        PlaylistCheck::NotPlaylist => by_extension,
        sniffed => sniffed,
    };
    let text = String::from_utf8_lossy(&playlist.body);
    let (entries, name) = match kind {
        PlaylistCheck::Hls => return Ok(Step::Hls),
        PlaylistCheck::Pls => (pls_entries(&text), "PLS"),
        // Relative entries are relative to where the playlist was served
        // from, after any redirects
        _ => (
            m3u_entries(&text, &directory_of(&playlist.final_url)),
            "M3U",
        ),
    };
    if entries.is_empty() {
        return Err(RadioError::Stream(format!(
            "No stream URL found in {name} playlist"
        )));
    }
    Ok(Step::Entries(entries))
}

/// The first of `entries` that `open` gets to work, trying them in turn: a
/// playlist's mirrors. Fails with the first entry's error (the station's
/// main address), and tries no more once cancelled or past `deadline`.
pub(crate) fn first_that_works<T>(
    entries: &[String],
    deadline: Deadline,
    mut open: impl FnMut(&str) -> Result<T>,
) -> Result<T> {
    let mut first_error = None;
    for entry in entries {
        match open(entry) {
            Ok(found) => return Ok(found),
            Err(RadioError::Cancelled) => return Err(RadioError::Cancelled),
            Err(e) => {
                first_error.get_or_insert(e);
                if deadline.check().is_err() {
                    break;
                }
            }
        }
    }
    Err(first_error
        .unwrap_or_else(|| RadioError::Stream("No stream URL found in playlist".to_string())))
}

/// The directory part of `url` without the trailing slash, ignoring any
/// query string (which may itself contain slashes)
fn directory_of(url: &str) -> String {
    match reqwest::Url::parse(url).and_then(|u| u.join(".")) {
        Ok(dir) => dir.as_str().trim_end_matches('/').to_string(),
        Err(_) => get_base_url(url),
    }
}

/// A fetched playlist
struct Playlist {
    /// Where it was served from, after redirects
    final_url: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

fn fetch_playlist(url: &str, cancel: &StreamCancel, deadline: Deadline) -> Result<Playlist> {
    let wait = deadline.cap(Duration::from_secs(CONNECT_TIMEOUT_SECS));
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(wait)
        .build()?;

    let response = client.get(url).send()?;

    if !response.status().is_success() {
        return Err(RadioError::Stream(format!("HTTP {}", response.status())));
    }

    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // A stream where the playlist should be would never end
    let body = read_body(response, MAX_PLAYLIST_BYTES, cancel, wait)?
        .ok_or_else(|| RadioError::Stream(format!("Playlist {}", too_large(MAX_PLAYLIST_BYTES))))?;
    Ok(Playlist {
        final_url,
        content_type,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- check_playlist_type ---

    #[test]
    fn check_pls_extension() {
        assert_eq!(
            check_playlist_type("http://example.com/stream.pls"),
            PlaylistCheck::Pls
        );
    }

    #[test]
    fn check_pls_with_query() {
        assert_eq!(
            check_playlist_type("http://example.com/stream.pls?sid=1"),
            PlaylistCheck::Pls
        );
    }

    #[test]
    fn check_m3u_extension() {
        assert_eq!(
            check_playlist_type("http://example.com/stream.m3u"),
            PlaylistCheck::M3u
        );
    }

    #[test]
    fn check_m3u_with_query() {
        assert_eq!(
            check_playlist_type("http://example.com/stream.m3u?id=5"),
            PlaylistCheck::M3u
        );
    }

    #[test]
    fn check_m3u8_extension() {
        assert_eq!(
            check_playlist_type("http://example.com/live.m3u8"),
            PlaylistCheck::Hls
        );
    }

    #[test]
    fn check_m3u8_with_query() {
        assert_eq!(
            check_playlist_type("http://example.com/live.m3u8?token=abc"),
            PlaylistCheck::Hls
        );
    }

    #[test]
    fn check_not_playlist() {
        assert_eq!(
            check_playlist_type("http://example.com/stream"),
            PlaylistCheck::NotPlaylist
        );
    }

    #[test]
    fn check_mp3_not_playlist() {
        assert_eq!(
            check_playlist_type("http://example.com/stream.mp3"),
            PlaylistCheck::NotPlaylist
        );
    }

    #[test]
    fn check_case_insensitive() {
        assert_eq!(
            check_playlist_type("http://example.com/stream.PLS"),
            PlaylistCheck::Pls
        );
        assert_eq!(
            check_playlist_type("http://example.com/live.M3U8"),
            PlaylistCheck::Hls
        );
    }

    // --- get_base_url ---

    #[test]
    fn base_url_standard() {
        assert_eq!(
            get_base_url("http://example.com/path/stream.m3u8"),
            "http://example.com/path"
        );
    }

    #[test]
    fn base_url_root() {
        assert_eq!(
            get_base_url("http://example.com/file.m3u8"),
            "http://example.com"
        );
    }

    #[test]
    fn base_url_no_path() {
        assert_eq!(get_base_url("nopath"), "");
    }

    // --- make_absolute_url ---

    #[test]
    fn absolute_url_already_absolute() {
        assert_eq!(
            make_absolute_url("http://other.com/stream", "http://base.com"),
            "http://other.com/stream"
        );
    }

    #[test]
    fn absolute_url_https_already_absolute() {
        assert_eq!(
            make_absolute_url("https://other.com/stream", "http://base.com"),
            "https://other.com/stream"
        );
    }

    #[test]
    fn absolute_url_relative() {
        assert_eq!(
            make_absolute_url("media/stream.aac", "http://example.com/hls"),
            "http://example.com/hls/media/stream.aac"
        );
    }

    #[test]
    fn absolute_url_filename_only() {
        assert_eq!(
            make_absolute_url("segment001.ts", "http://example.com/live"),
            "http://example.com/live/segment001.ts"
        );
    }

    // --- parse_pls ---

    #[test]
    fn parse_pls_standard() {
        let content = "[playlist]\nNumberOfEntries=1\nFile1=http://stream.example.com:8000/live\nTitle1=Test Radio\nLength1=-1\n";
        assert_eq!(
            parse_pls(content),
            Some("http://stream.example.com:8000/live".to_string())
        );
    }

    #[test]
    fn parse_pls_multiple_entries() {
        let content = "[playlist]\nFile1=http://stream1.com/live\nFile2=http://stream2.com/live\n";
        // Returns first entry
        assert_eq!(
            parse_pls(content),
            Some("http://stream1.com/live".to_string())
        );
    }

    #[test]
    fn parse_pls_empty() {
        assert_eq!(parse_pls("[playlist]\n"), None);
    }

    #[test]
    fn parse_pls_no_http_url() {
        let content = "[playlist]\nFile1=/local/path\n";
        assert_eq!(parse_pls(content), None);
    }

    // --- parse_m3u ---

    #[test]
    fn parse_m3u_standard() {
        let content = "#EXTM3U\n#EXTINF:-1,Test Radio\nhttp://stream.example.com/live\n";
        assert_eq!(
            parse_m3u(content, "http://base.com"),
            Some("http://stream.example.com/live".to_string())
        );
    }

    #[test]
    fn parse_m3u_relative_url() {
        let content = "#EXTM3U\n#EXTINF:-1,Test\nstream/live.mp3\n";
        assert_eq!(
            parse_m3u(content, "http://example.com"),
            Some("http://example.com/stream/live.mp3".to_string())
        );
    }

    #[test]
    fn parse_m3u_empty() {
        assert_eq!(parse_m3u("#EXTM3U\n", "http://base.com"), None);
    }

    #[test]
    fn parse_m3u_comments_only() {
        let content = "#EXTM3U\n#EXTINF:-1,Test\n# comment\n";
        assert_eq!(parse_m3u(content, "http://base.com"), None);
    }

    #[test]
    fn parse_m3u_skips_metadata_lines() {
        let content = "#EXTM3U\nkey=value\nhttp://stream.com/live\n";
        // "key=value" has '=' so it's skipped, returns the http URL
        assert_eq!(
            parse_m3u(content, "http://base.com"),
            Some("http://stream.com/live".to_string())
        );
    }

    // --- PlaylistCheck ---

    #[test]
    fn playlist_check_debug() {
        assert_eq!(format!("{:?}", PlaylistCheck::Pls), "Pls");
        assert_eq!(format!("{:?}", PlaylistCheck::M3u), "M3u");
        assert_eq!(format!("{:?}", PlaylistCheck::Hls), "Hls");
        assert_eq!(format!("{:?}", PlaylistCheck::NotPlaylist), "NotPlaylist");
    }

    #[test]
    fn playlist_check_clone() {
        let check = PlaylistCheck::Pls;
        let cloned = check;
        assert_eq!(check, cloned);
    }

    // --- check_playlist_type edge cases ---

    #[test]
    fn check_m3u_not_m3u8() {
        // .m3u should NOT match as HLS
        assert_eq!(
            check_playlist_type("http://example.com/stream.m3u"),
            PlaylistCheck::M3u
        );
    }

    #[test]
    fn check_m3u8_not_m3u() {
        // .m3u8 should NOT match as M3u
        assert_eq!(
            check_playlist_type("http://example.com/stream.m3u8"),
            PlaylistCheck::Hls
        );
    }

    #[test]
    fn check_mixed_case_m3u() {
        assert_eq!(
            check_playlist_type("http://example.com/stream.M3U"),
            PlaylistCheck::M3u
        );
    }

    #[test]
    fn check_empty_url() {
        assert_eq!(check_playlist_type(""), PlaylistCheck::NotPlaylist);
    }

    #[test]
    fn check_pls_in_path_not_extension() {
        // ".pls" in the middle of the URL, not as extension
        assert_eq!(
            check_playlist_type("http://example.com/pls_files/stream"),
            PlaylistCheck::NotPlaylist
        );
    }

    #[test]
    fn check_m3u8_with_fragment() {
        // Fragment after extension
        assert_eq!(
            check_playlist_type("http://example.com/live.m3u8#section"),
            PlaylistCheck::NotPlaylist // no match because "#section" is after .m3u8
        );
    }

    // --- get_base_url edge cases ---

    #[test]
    fn base_url_with_query_string() {
        // The query is part of the last segment, not stripped
        assert_eq!(
            get_base_url("http://example.com/path/stream.m3u8?token=abc"),
            "http://example.com/path"
        );
    }

    #[test]
    fn base_url_deep_path() {
        assert_eq!(
            get_base_url("http://cdn.example.com/a/b/c/d/playlist.m3u8"),
            "http://cdn.example.com/a/b/c/d"
        );
    }

    #[test]
    fn base_url_trailing_slash() {
        assert_eq!(
            get_base_url("http://example.com/path/"),
            "http://example.com/path"
        );
    }

    #[test]
    fn base_url_empty() {
        assert_eq!(get_base_url(""), "");
    }

    // --- make_absolute_url edge cases ---

    #[test]
    fn absolute_url_empty_relative() {
        assert_eq!(make_absolute_url("", "http://base.com"), "http://base.com/");
    }

    #[test]
    fn absolute_url_empty_base() {
        assert_eq!(make_absolute_url("segment.ts", ""), "/segment.ts");
    }

    #[test]
    fn absolute_url_with_query_in_relative() {
        assert_eq!(
            make_absolute_url("seg.ts?token=abc", "http://cdn.com/hls"),
            "http://cdn.com/hls/seg.ts?token=abc"
        );
    }

    // --- parse_pls edge cases ---

    #[test]
    fn parse_pls_case_insensitive_file_key() {
        // PLS spec uses "File", but our parser checks starts_with("file") after to_lowercase
        let content = "[playlist]\nFILE1=http://stream.com/live\n";
        assert_eq!(
            parse_pls(content),
            Some("http://stream.com/live".to_string())
        );
    }

    #[test]
    fn parse_pls_with_whitespace() {
        let content = "[playlist]\n  File1 = http://stream.com/live  \n";
        // split('=').nth(1) gets " http://stream.com/live  " which gets trimmed
        assert_eq!(
            parse_pls(content),
            Some("http://stream.com/live".to_string())
        );
    }

    #[test]
    fn parse_pls_url_with_equals() {
        // Tokenised stream URLs carry '=' in the query string
        let content = "[playlist]\nFile1=http://stream.com/live?key=value&id=123\n";
        assert_eq!(
            parse_pls(content),
            Some("http://stream.com/live?key=value&id=123".to_string())
        );
    }

    #[test]
    fn parse_pls_uppercase_scheme_and_bom() {
        let content = "\u{feff}[playlist]\r\nFile1=HTTP://Stream.com/live\r\n";
        assert_eq!(
            parse_pls(content),
            Some("HTTP://Stream.com/live".to_string())
        );
    }

    #[test]
    fn parse_m3u_with_bom() {
        let content = "\u{feff}#EXTM3U\nhttp://stream.com/live\n";
        assert_eq!(
            parse_m3u(content, "http://base.com/dir"),
            Some("http://stream.com/live".to_string())
        );
        // A BOM on a relative first entry must not end up in the URL
        assert_eq!(
            parse_m3u("\u{feff}live.mp3\n", "http://base.com/dir"),
            Some("http://base.com/dir/live.mp3".to_string())
        );
    }

    #[test]
    fn make_absolute_url_resolves_like_a_browser() {
        let base = "http://example.com/radio/lists";
        assert_eq!(
            make_absolute_url("/stream.mp3", base),
            "http://example.com/stream.mp3"
        );
        assert_eq!(
            make_absolute_url("../live/stream.mp3", base),
            "http://example.com/radio/live/stream.mp3"
        );
        assert_eq!(
            make_absolute_url("//cdn.example.net/s.aac", base),
            "http://cdn.example.net/s.aac"
        );
        assert_eq!(
            make_absolute_url("stream.mp3", base),
            "http://example.com/radio/lists/stream.mp3"
        );
    }

    #[test]
    fn directory_of_ignores_query_slashes() {
        assert_eq!(
            directory_of("http://example.com/a/list.m3u?next=/x/y"),
            "http://example.com/a"
        );
        assert_eq!(
            directory_of("http://example.com/list.m3u"),
            "http://example.com"
        );
    }

    #[test]
    fn parse_pls_https_url() {
        let content = "[playlist]\nFile1=https://secure.stream.com/live\n";
        assert_eq!(
            parse_pls(content),
            Some("https://secure.stream.com/live".to_string())
        );
    }

    #[test]
    fn parse_pls_with_number_not_1() {
        let content = "[playlist]\nFile5=http://stream.com/live\n";
        assert_eq!(
            parse_pls(content),
            Some("http://stream.com/live".to_string())
        );
    }

    #[test]
    fn parse_pls_only_metadata_lines() {
        let content = "[playlist]\nTitle1=Radio\nLength1=-1\nNumberOfEntries=1\n";
        assert_eq!(parse_pls(content), None);
    }

    // --- parse_m3u edge cases ---

    #[test]
    fn parse_m3u_without_header() {
        // M3U without #EXTM3U header is still valid
        let content = "http://stream.com/live\n";
        assert_eq!(
            parse_m3u(content, "http://base.com"),
            Some("http://stream.com/live".to_string())
        );
    }

    #[test]
    fn parse_m3u_blank_lines_between_entries() {
        let content = "#EXTM3U\n\n\n#EXTINF:-1,Test\n\nhttp://stream.com/live\n";
        assert_eq!(
            parse_m3u(content, "http://base.com"),
            Some("http://stream.com/live".to_string())
        );
    }

    #[test]
    fn parse_m3u_crlf_line_endings() {
        let content = "#EXTM3U\r\n#EXTINF:-1,Test\r\nhttp://stream.com/live\r\n";
        assert_eq!(
            parse_m3u(content, "http://base.com"),
            Some("http://stream.com/live".to_string())
        );
    }

    #[test]
    fn parse_m3u_url_with_query_containing_equals() {
        // URL with '=' should still be returned (starts with http)
        let content = "#EXTM3U\nhttp://stream.com/live?key=value\n";
        assert_eq!(
            parse_m3u(content, "http://base.com"),
            Some("http://stream.com/live?key=value".to_string())
        );
    }

    #[test]
    fn parse_m3u_relative_url_with_query() {
        let base = "http://example.com/radio";
        assert_eq!(
            parse_m3u("#EXTM3U\nlive.mp3?sid=1\n", base),
            Some("http://example.com/radio/live.mp3?sid=1".to_string())
        );
        assert_eq!(
            parse_m3u("/live?sid=1&t=a=b\n", base),
            Some("http://example.com/live?sid=1&t=a=b".to_string())
        );
        assert_eq!(
            parse_m3u(
                "//cdn.example.com/live?token=x\n",
                "https://example.com/radio"
            ),
            Some("https://cdn.example.com/live?token=x".to_string())
        );
    }

    #[test]
    fn parse_m3u_skips_settings_before_a_relative_entry() {
        // A '/' in the value doesn't make a setting an entry
        let content = "#EXTM3U\nVersion=2\nTitle=AC/DC Radio\nlive.mp3?sid=1\n";
        assert_eq!(
            parse_m3u(content, "http://example.com/radio"),
            Some("http://example.com/radio/live.mp3?sid=1".to_string())
        );
    }

    #[test]
    fn parse_m3u_relative_file_only() {
        let content = "stream.mp3\n";
        assert_eq!(
            parse_m3u(content, "http://example.com/audio"),
            Some("http://example.com/audio/stream.mp3".to_string())
        );
    }

    #[test]
    fn parse_m3u_first_entry_wins() {
        let content = "http://first.com/stream\nhttp://second.com/stream\n";
        assert_eq!(
            parse_m3u(content, ""),
            Some("http://first.com/stream".to_string())
        );
    }

    #[test]
    fn every_entry_is_kept_in_order() {
        let pls = "[playlist]\nFile1=http://a.example/live\nFile2=mms://b.example/live\n\
                   File3=https://c.example/live\nFile4=http://a.example/live\n";
        assert_eq!(
            pls_entries(pls),
            ["http://a.example/live", "https://c.example/live"]
        );
        let m3u = "#EXTM3U\nhttp://a.example/live\nmms://b.example/live\nrtsp://b.example/live\n\
                   C:\\Music\\song.mp3\nbackup.mp3\nhttp://a.example/live\n";
        assert_eq!(
            m3u_entries(m3u, "http://example.com/radio"),
            [
                "http://a.example/live",
                "http://example.com/radio/backup.mp3"
            ]
        );
        let many: String = (1..=9)
            .map(|i| format!("http://s{i}.example/live\n"))
            .collect();
        assert_eq!(m3u_entries(&many, "").len(), MAX_PLAYLIST_ENTRIES);
        assert_eq!(m3u_entries(&many, "")[0], "http://s1.example/live");
    }

    #[test]
    fn parse_m3u_skips_other_schemes_and_web_page_lines() {
        let content = "mms://radio.example/live\nhttp://radio.example/live\n";
        assert_eq!(
            parse_m3u(content, "http://base.com"),
            Some("http://radio.example/live".to_string())
        );
        let page = "<html>\n<body>Not found</body>\n</html>\n";
        assert_eq!(parse_m3u(page, "http://base.com"), None);
    }

    // --- resolve_playlist_url ---

    #[test]
    fn resolve_non_playlist_passes_through() {
        let url = "http://example.com/stream";
        let result = resolve_playlist_url(url).unwrap();
        assert_eq!(result, "http://example.com/stream");
    }

    #[test]
    fn resolve_hls_passes_through() {
        let url = "http://example.com/live.m3u8";
        let result = resolve_playlist_url(url).unwrap();
        assert_eq!(result, "http://example.com/live.m3u8");
    }

    #[test]
    fn resolve_mp3_url_passes_through() {
        let url = "http://example.com/stream.mp3";
        let result = resolve_playlist_url(url).unwrap();
        assert_eq!(result, "http://example.com/stream.mp3");
    }

    #[test]
    fn resolve_hls_with_query_passes_through() {
        let url = "http://example.com/live.m3u8?token=abc123";
        let result = resolve_playlist_url(url).unwrap();
        assert_eq!(result, "http://example.com/live.m3u8?token=abc123");
    }

    // --- sniff_playlist ---

    mod sniff {
        use super::*;
        use PlaylistCheck::{Hls, M3u, NotPlaylist, Pls};

        #[test]
        fn a_playlist_is_told_by_its_first_line() {
            let m3u = b"#EXTM3U\n#EXTINF:-1,Radio\nhttp://radio.example/live\n";
            assert_eq!(sniff_playlist(None, m3u), M3u);
            let hls = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=64000\nlow.m3u8\n";
            assert_eq!(sniff_playlist(None, hls), Hls);
            let media = b"#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nseg1.aac\n";
            assert_eq!(sniff_playlist(None, media), Hls);
            let pls = b"\xEF\xBB\xBF  [Playlist]\r\nFile1=http://radio.example/live\r\n";
            assert_eq!(sniff_playlist(Some("text/plain"), pls), Pls);
            // What it is beats what the server calls it
            assert_eq!(sniff_playlist(Some("audio/mpeg"), m3u), M3u);
        }

        #[test]
        fn a_playlist_type_decides_for_text() {
            let entry = b"http://radio.example/live\n";
            assert_eq!(sniff_playlist(Some("audio/x-mpegurl"), entry), M3u);
            assert_eq!(sniff_playlist(Some("audio/x-scpls"), b"File1=/live\n"), Pls);
            // An HLS type is also given to plain lists: the tags decide
            assert_eq!(
                sniff_playlist(Some("application/x-mpegURL; charset=utf-8"), b"low.m3u8\n"),
                M3u
            );
            let media = b"#EXTINF:6,\nseg1.aac\n#EXT-X-ENDLIST\n";
            assert_eq!(
                sniff_playlist(Some("application/vnd.apple.mpegurl"), media),
                Hls
            );
            // A bare stream address, as some station pages hand out
            assert_eq!(sniff_playlist(Some("text/plain"), entry), M3u);
            assert_eq!(sniff_playlist(None, b"https://radio.example/live"), M3u);
        }

        #[test]
        fn audio_is_not_a_playlist() {
            let mp3 = [0xFF, 0xFB, 0x90, 0x64, 0x00, 0x0F, 0xF0, 0x00];
            // Not even from a server that calls it one
            assert_eq!(sniff_playlist(Some("audio/x-mpegurl"), &mp3), NotPlaylist);
            assert_eq!(
                sniff_playlist(None, b"ID3\x04\x00\x00\x00\x00\x01\x00"),
                NotPlaylist
            );
            assert_eq!(
                sniff_playlist(Some("application/ogg"), b"OggS\x00\x02"),
                NotPlaylist
            );
            assert_eq!(sniff_playlist(Some("audio/x-scpls"), b""), NotPlaylist);
            let page = b"<html><body>Offline</body></html>";
            assert_eq!(sniff_playlist(Some("text/html"), page), NotPlaylist);
        }
    }

    // --- resolve_playlist_url over HTTP ---

    mod network {
        use super::*;
        use crate::stream::test_server::{Route, TestServer};

        #[test]
        fn a_stalled_playlist_fetch_ends_by_the_deadline() {
            let server = TestServer::start();
            server.route(
                "/station.pls",
                Route::new("[playlist]\n").without_length().stall(),
            );
            let start = std::time::Instant::now();
            let resolved = resolve_playlist(
                &server.url("/station.pls"),
                &StreamCancel::new(),
                Deadline::after(Duration::from_millis(500)),
            );
            assert!(resolved.is_err());
            let took = start.elapsed();
            assert!(took < Duration::from_secs(2), "{took:?}");
        }

        #[test]
        fn follows_a_pls_to_an_m3u_to_the_stream() {
            let server = TestServer::start();
            let m3u = server.url("/lists/station.m3u");
            server.route(
                "/station.pls",
                Route::new(format!("[playlist]\nFile1={m3u}\n")),
            );
            server.route("/lists/station.m3u", Route::new("#EXTM3U\n../live/aac\n"));
            assert_eq!(
                resolve_playlist_url(&server.url("/station.pls")).unwrap(),
                server.url("/live/aac")
            );
        }

        #[test]
        fn keeps_the_query_string_of_a_pls_entry() {
            let server = TestServer::start();
            server.route(
                "/station.pls",
                Route::new("[playlist]\nFile1=http://radio.example/live?sid=1&token=a=b\n"),
            );
            assert_eq!(
                resolve_playlist_url(&server.url("/station.pls")).unwrap(),
                "http://radio.example/live?sid=1&token=a=b"
            );
        }

        #[test]
        fn resolves_m3u_entries_against_the_redirected_url() {
            let server = TestServer::start();
            server.route(
                "/go/station.m3u",
                Route::redirect(&server.url("/edge/7/station.m3u")),
            );
            server.route("/edge/7/station.m3u", Route::new("stream.mp3\n"));
            assert_eq!(
                resolve_playlist_url(&server.url("/go/station.m3u")).unwrap(),
                server.url("/edge/7/stream.mp3")
            );
        }

        #[test]
        fn an_m3u_that_is_hls_resolves_to_hls() {
            let server = TestServer::start();
            server.route(
                "/live.m3u",
                Route::new("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nseg1.aac\n"),
            );
            let target = resolve_playlist(
                &server.url("/live.m3u"),
                &StreamCancel::new(),
                Deadline::NONE,
            )
            .unwrap();
            assert_eq!(
                target,
                Target {
                    url: server.url("/live.m3u"),
                    hls: true
                }
            );
        }

        #[test]
        fn a_playlist_is_read_as_what_it_is() {
            // A PLS saved as .m3u: its lines would read as relative URLs
            let server = TestServer::start();
            server.route(
                "/station.m3u",
                Route::new("[playlist]\nNumberOfEntries=1\nFile1=http://radio.example/live\n"),
            );
            assert_eq!(
                resolve_playlist_url(&server.url("/station.m3u")).unwrap(),
                "http://radio.example/live"
            );
        }

        #[test]
        fn a_playlist_that_fails_to_load_falls_back_to_the_next_entry() {
            let server = TestServer::start();
            server.route(
                "/station.pls",
                Route::new(format!(
                    "[playlist]\nFile1={}\nFile2={}\n",
                    server.url("/gone.m3u"),
                    server.url("/backup.m3u")
                )),
            );
            server.route("/backup.m3u", Route::new("live.mp3\n"));
            assert_eq!(
                resolve_playlist_url(&server.url("/station.pls")).unwrap(),
                server.url("/live.mp3")
            );
        }

        #[test]
        fn a_web_page_is_not_a_playlist() {
            let server = TestServer::start();
            server.route(
                "/station.pls",
                Route::new("<html><body>Moved</body></html>")
                    .header("Content-Type", "text/html; charset=utf-8"),
            );
            let err = resolve_playlist_url(&server.url("/station.pls")).unwrap_err();
            assert!(err.to_string().contains("web page"), "{err}");
            // A playlist served as one is read
            server.route(
                "/station.m3u",
                Route::new("#EXTM3U\nhttp://radio.example/live\n")
                    .header("Content-Type", "text/html"),
            );
            assert_eq!(
                resolve_playlist_url(&server.url("/station.m3u")).unwrap(),
                "http://radio.example/live"
            );
        }

        #[test]
        fn a_playlist_that_never_ends_is_not_read_to_the_end() {
            // A live stream where the playlist should be
            let server = TestServer::start();
            server.route("/station.pls", Route::new(vec![b'x'; 64 * 1024]).endless());
            let start = std::time::Instant::now();
            let err = resolve_playlist_url(&server.url("/station.pls")).unwrap_err();
            assert!(err.to_string().contains("larger than"), "{err}");
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{:?}",
                start.elapsed()
            );
        }

        #[test]
        fn reports_http_errors() {
            let server = TestServer::start();
            let err = resolve_playlist_url(&server.url("/missing.pls")).unwrap_err();
            assert!(err.to_string().contains("404"), "{err}");
        }

        #[test]
        fn stops_at_the_nesting_limit() {
            let server = TestServer::start();
            // a.m3u -> b.m3u -> a.m3u -> ...
            server.route("/a.m3u", Route::new(format!("{}\n", server.url("/b.m3u"))));
            server.route("/b.m3u", Route::new(format!("{}\n", server.url("/a.m3u"))));
            let err = resolve_playlist_url(&server.url("/a.m3u")).unwrap_err();
            assert!(err.to_string().contains("too deep"), "{err}");
        }
    }
}
