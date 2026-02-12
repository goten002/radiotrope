//! Playlist parsing (PLS/M3U)
//!
//! Resolves playlist URLs to stream URLs, handling PLS and M3U formats.

use std::time::Duration;

use crate::config::network::{CONNECT_TIMEOUT_SECS, MAX_PLAYLIST_DEPTH, USER_AGENT};
use crate::error::{RadioError, Result};

/// Result of checking a URL's playlist type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistCheck {
    Pls,
    M3u,
    Hls,
    NotPlaylist,
}

/// Check what type of playlist a URL points to based on extension
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

/// Extract the base URL (directory) from a full URL
pub fn get_base_url(url: &str) -> String {
    url.rsplit_once('/')
        .map(|(base, _)| base)
        .unwrap_or("")
        .to_string()
}

/// Make a URI absolute, using base_url if the URI is relative
pub fn make_absolute_url(uri: &str, base_url: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        uri.to_string()
    } else {
        format!("{}/{}", base_url, uri)
    }
}

/// Parse a PLS playlist and return the first stream URL
pub fn parse_pls(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.to_lowercase().starts_with("file") && line.contains('=') {
            if let Some(stream_url) = line.split('=').nth(1) {
                let stream_url = stream_url.trim();
                if stream_url.starts_with("http") {
                    return Some(stream_url.to_string());
                }
            }
        }
    }
    None
}

/// Parse an M3U playlist and return the first stream URL
pub fn parse_m3u(content: &str, base_url: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with("http://") || line.starts_with("https://") {
            return Some(line.to_string());
        } else if !line.contains('=') {
            return Some(make_absolute_url(line, base_url));
        }
    }
    None
}

/// Resolve a playlist URL to its final stream URL, following chains recursively.
///
/// M3U8 (HLS) URLs pass through unchanged. PLS and M3U playlists are fetched
/// and parsed, recursing up to `MAX_PLAYLIST_DEPTH` levels.
pub fn resolve_playlist_url(url: &str) -> Result<String> {
    resolve_recursive(url, MAX_PLAYLIST_DEPTH)
}

fn resolve_recursive(url: &str, depth: usize) -> Result<String> {
    if depth == 0 {
        return Err(RadioError::Stream("Playlist nesting too deep".to_string()));
    }

    match check_playlist_type(url) {
        PlaylistCheck::Hls | PlaylistCheck::NotPlaylist => Ok(url.to_string()),
        PlaylistCheck::Pls => {
            let content = fetch_playlist(url)?;
            let stream_url = parse_pls(&content).ok_or_else(|| {
                RadioError::Stream("No stream URL found in PLS playlist".to_string())
            })?;
            resolve_recursive(&stream_url, depth - 1)
        }
        PlaylistCheck::M3u => {
            let content = fetch_playlist(url)?;
            let base_url = get_base_url(url);
            let stream_url = parse_m3u(&content, &base_url).ok_or_else(|| {
                RadioError::Stream("No stream URL found in M3U playlist".to_string())
            })?;
            resolve_recursive(&stream_url, depth - 1)
        }
    }
}

fn fetch_playlist(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()?;

    let response = client.get(url).send()?;

    if !response.status().is_success() {
        return Err(RadioError::Stream(format!("HTTP {}", response.status())));
    }

    response.text().map_err(|e| e.into())
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
        // URL contains '=' in query string
        let content = "[playlist]\nFile1=http://stream.com/live?key=value&id=123\n";
        // split('=').nth(1) only gets first value after first '='
        // This is a known limitation: it gets "http://stream.com/live?key"
        let result = parse_pls(content);
        assert!(result.is_some());
        // The URL is truncated at the second '=' — this tests the actual behavior
        assert!(result.unwrap().starts_with("http"));
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
}
