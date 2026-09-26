//! Song info carried inside HLS streams
//!
//! HLS has no ICY metadata. Stations send now-playing info as ID3 "timed
//! metadata" instead, in one of these places:
//! - MPEG-TS segments: a PES stream with PMT stream type 0x15 (see
//!   [`demux_ts_segment_with_metadata`](crate::stream::hls::demux_ts_segment_with_metadata))
//! - Packed audio segments (`.aac`, `.mp3`): ID3 tags between audio frames,
//!   removed with [`Id3Scanner`](crate::stream::id3::Id3Scanner)
//! - fMP4/CMAF segments: `emsg` boxes holding an ID3 tag ([`fmp4_emsg_id3`])
//!
//! Some playlists also put `title="…",artist="…"` on each `#EXTINF` line
//! ([`parse_extinf_title`]).

use std::collections::HashMap;

use crate::stream::metadata::{MetadataSource, StreamMetadata};

/// `emsg` scheme for ID3 timed metadata in CMAF (AOM "Carriage of ID3 Timed
/// Metadata in CMAF")
pub const EMSG_ID3_SCHEME_AOM: &str = "https://aomedia.org/emsg/ID3";

/// `emsg` scheme Apple uses for the same thing
pub const EMSG_ID3_SCHEME_APPLE: &str = "https://developer.apple.com/streaming/emsg-id3";

/// Box types that can start an fMP4 segment
const MP4_SEGMENT_BOXES: [&[u8; 4]; 7] = [
    b"ftyp", b"styp", b"moof", b"moov", b"emsg", b"sidx", b"prft",
];

/// True if `data` starts with an MP4 box that begins fMP4 segments
pub fn looks_like_fmp4(data: &[u8]) -> bool {
    data.len() >= 8 && MP4_SEGMENT_BOXES.iter().any(|b| &data[4..8] == *b)
}

/// ID3 payloads of the ID3 `emsg` boxes at the top level of an fMP4 segment.
pub fn fmp4_emsg_id3(data: &[u8]) -> Vec<&[u8]> {
    let mut found = Vec::new();
    let mut pos = 0;
    while data.len() - pos >= 8 {
        let size32 = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let box_type = &data[pos + 4..pos + 8];
        let (header_len, size) = match size32 {
            0 => (8, data.len() - pos),
            1 => {
                if data.len() - pos < 16 {
                    break;
                }
                let mut large = [0u8; 8];
                large.copy_from_slice(&data[pos + 8..pos + 16]);
                (
                    16,
                    u64::from_be_bytes(large).min(usize::MAX as u64) as usize,
                )
            }
            n => (8, n as usize),
        };
        if size < header_len || size > data.len() - pos {
            break;
        }
        if box_type == b"emsg" {
            if let Some(payload) = emsg_id3_payload(&data[pos + header_len..pos + size]) {
                found.push(payload);
            }
        }
        pos += size;
    }
    found
}

/// The message data of an `emsg` box body if its scheme is ID3
fn emsg_id3_payload(body: &[u8]) -> Option<&[u8]> {
    let (&version, rest) = body.split_first()?;
    let rest = rest.get(3..)?; // flags
    let (scheme, rest) = match version {
        0 => {
            let (scheme, rest) = split_cstr(rest)?;
            let (_value, rest) = split_cstr(rest)?;
            // timescale, presentation_time_delta, event_duration, id
            (scheme, rest.get(16..)?)
        }
        1 => {
            // timescale, presentation_time (64-bit), event_duration, id
            let rest = rest.get(20..)?;
            let (scheme, rest) = split_cstr(rest)?;
            let (_value, rest) = split_cstr(rest)?;
            (scheme, rest)
        }
        _ => return None,
    };
    if scheme == EMSG_ID3_SCHEME_AOM.as_bytes() || scheme == EMSG_ID3_SCHEME_APPLE.as_bytes() {
        Some(rest)
    } else {
        None
    }
}

/// Split a NUL-terminated string off the front of `data`
fn split_cstr(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = data.iter().position(|&b| b == 0)?;
    Some((&data[..end], &data[end + 1..]))
}

/// Map each segment URI in a media playlist to the full title of its
/// `#EXTINF` line (everything after the duration's comma).
///
/// m3u8-rs stops titles at the first comma, which splits
/// `title="…",artist="…"`, so this reads the raw playlist text instead.
pub fn extinf_titles(playlist: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(playlist);
    let mut titles = HashMap::new();
    let mut pending: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending = rest.split_once(',').map(|(_, title)| title.to_string());
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else if let Some(title) = pending.take() {
            titles.insert(line.to_string(), title);
        }
    }
    titles
}

/// Song info from `key="value"` attributes in an `#EXTINF` title, e.g.
/// `title="Song",artist="Artist",url="…"`.
///
/// Returns `None` for plain titles without a `title=` attribute: those are
/// usually segment names, not songs.
pub fn parse_extinf_title(extinf_title: &str) -> Option<StreamMetadata> {
    let mut title = None;
    let mut artist = None;
    let mut saw_title_key = false;
    for (key, value) in extinf_attributes(extinf_title) {
        if key.eq_ignore_ascii_case("title") {
            saw_title_key = true;
            title = Some(value);
        } else if key.eq_ignore_ascii_case("artist") {
            artist = Some(value);
        }
    }
    if !saw_title_key {
        return None;
    }
    let meta = StreamMetadata::new(title, artist, MetadataSource::HlsPlaylist);
    if meta.is_empty() {
        None
    } else {
        Some(meta)
    }
}

/// Parse `key="value",key2=value2` pairs. Quoted values may contain commas
/// and backslash-escaped quotes. Parsing stops at anything malformed.
fn extinf_attributes(s: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                key.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if key.is_empty() || chars.next() != Some('=') {
            break;
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            let mut closed = false;
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(escaped) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    '"' => {
                        closed = true;
                        break;
                    }
                    _ => value.push(c),
                }
            }
            if !closed {
                break;
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }
        pairs.push((key, value));
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::id3::test_util::id3v2_song;

    fn mp4_box(box_type: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(box_type);
        b.extend_from_slice(body);
        b
    }

    fn emsg_v1(scheme: &str, message: &[u8]) -> Vec<u8> {
        let mut body = vec![1, 0, 0, 0];
        body.extend_from_slice(&90_000u32.to_be_bytes()); // timescale
        body.extend_from_slice(&123_456u64.to_be_bytes()); // presentation_time
        body.extend_from_slice(&0u32.to_be_bytes()); // event_duration
        body.extend_from_slice(&7u32.to_be_bytes()); // id
        body.extend_from_slice(scheme.as_bytes());
        body.push(0);
        body.extend_from_slice(b"1\0"); // value
        body.extend_from_slice(message);
        mp4_box(b"emsg", &body)
    }

    fn emsg_v0(scheme: &str, message: &[u8]) -> Vec<u8> {
        let mut body = vec![0, 0, 0, 0];
        body.extend_from_slice(scheme.as_bytes());
        body.push(0);
        body.push(0); // empty value
        body.extend_from_slice(&[0u8; 16]);
        body.extend_from_slice(message);
        mp4_box(b"emsg", &body)
    }

    // --- looks_like_fmp4 ---

    #[test]
    fn detects_fmp4_segment_starts() {
        assert!(looks_like_fmp4(&mp4_box(b"styp", b"msdh")));
        assert!(looks_like_fmp4(&mp4_box(b"emsg", b"")));
        assert!(looks_like_fmp4(&mp4_box(b"moof", b"")));
        assert!(!looks_like_fmp4(b"ID3\x04\x00\x00\x00\x00\x00\x00"));
        assert!(!looks_like_fmp4(&[0xFF, 0xF1, 0x50, 0x80, 0, 0, 0, 0]));
        assert!(!looks_like_fmp4(b"moof"));
    }

    // --- fmp4_emsg_id3 ---

    #[test]
    fn finds_id3_in_emsg_v1() {
        let tag = id3v2_song("Artist", "Song");
        let segment = [
            mp4_box(b"styp", b"msdh"),
            emsg_v1(EMSG_ID3_SCHEME_AOM, &tag),
            mp4_box(b"moof", &[0; 20]),
            mp4_box(b"mdat", &[0xAA; 50]),
        ]
        .concat();
        assert_eq!(fmp4_emsg_id3(&segment), vec![tag.as_slice()]);
    }

    #[test]
    fn finds_id3_in_emsg_v0_apple_scheme() {
        let tag = id3v2_song("Artist", "Song");
        let segment = [
            emsg_v0(EMSG_ID3_SCHEME_APPLE, &tag),
            mp4_box(b"mdat", &[1; 8]),
        ]
        .concat();
        assert_eq!(fmp4_emsg_id3(&segment), vec![tag.as_slice()]);
    }

    #[test]
    fn ignores_other_emsg_schemes() {
        let segment = [
            emsg_v1("urn:scte:scte35:2013:bin", b"\xFC\x30"),
            mp4_box(b"mdat", &[1; 8]),
        ]
        .concat();
        assert!(fmp4_emsg_id3(&segment).is_empty());
    }

    #[test]
    fn stops_on_malformed_boxes() {
        let tag = id3v2_song("Artist", "Song");
        let mut segment = emsg_v1(EMSG_ID3_SCHEME_AOM, &tag);
        // A box claiming to be bigger than the data
        segment.extend_from_slice(&1000u32.to_be_bytes());
        segment.extend_from_slice(b"emsg");
        assert_eq!(fmp4_emsg_id3(&segment).len(), 1);
        // Size smaller than its header
        assert!(fmp4_emsg_id3(&[0, 0, 0, 4, b'e', b'm', b's', b'g']).is_empty());
        // Truncated emsg body
        assert!(fmp4_emsg_id3(&mp4_box(b"emsg", &[1, 0, 0, 0, 0])).is_empty());
        assert!(fmp4_emsg_id3(&[]).is_empty());
    }

    #[test]
    fn handles_size_zero_and_large_size_boxes() {
        let tag = id3v2_song("Artist", "Song");
        // Last box with size 0 runs to the end
        let mut last = emsg_v1(EMSG_ID3_SCHEME_AOM, &tag);
        last[..4].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(fmp4_emsg_id3(&last).len(), 1);
        // 64-bit size
        let normal = emsg_v1(EMSG_ID3_SCHEME_AOM, &tag);
        let body = &normal[8..];
        let mut large = 1u32.to_be_bytes().to_vec();
        large.extend_from_slice(b"emsg");
        large.extend_from_slice(&((body.len() + 16) as u64).to_be_bytes());
        large.extend_from_slice(body);
        assert_eq!(fmp4_emsg_id3(&large).len(), 1);
    }

    // --- extinf_titles ---

    #[test]
    fn extinf_titles_keep_commas() {
        let playlist = b"#EXTM3U\n#EXT-X-TARGETDURATION:10\n\
            #EXTINF:10.0,title=\"Hello, Goodbye\",artist=\"The Beatles\"\n\
            seg1.aac\n\
            #EXTINF:10.0,\n\
            #EXT-X-PROGRAM-DATE-TIME:2026-09-26T00:00:00Z\n\
            seg2.aac\n\
            #EXTINF:10\n\
            seg3.aac\n";
        let titles = extinf_titles(playlist);
        assert_eq!(
            titles.get("seg1.aac").map(String::as_str),
            Some(r#"title="Hello, Goodbye",artist="The Beatles""#)
        );
        assert_eq!(titles.get("seg2.aac").map(String::as_str), Some(""));
        assert_eq!(titles.get("seg3.aac"), None);
        let m = parse_extinf_title(&titles["seg1.aac"]).unwrap();
        assert_eq!(m.title.as_deref(), Some("Hello, Goodbye"));
        assert_eq!(m.artist.as_deref(), Some("The Beatles"));
    }

    // --- parse_extinf_title ---

    #[test]
    fn extinf_title_and_artist() {
        let m =
            parse_extinf_title(r#"title="Comfortably Numb",artist="Pink Floyd",url="x""#).unwrap();
        assert_eq!(m.title.as_deref(), Some("Comfortably Numb"));
        assert_eq!(m.artist.as_deref(), Some("Pink Floyd"));
        assert_eq!(m.source, MetadataSource::HlsPlaylist);
    }

    #[test]
    fn extinf_quoted_commas_and_escapes() {
        let m = parse_extinf_title(
            r#"title="Hello, Goodbye",artist="The \"Fab\" Four",url="song_spot=\"M\" MediaBaseId=\"1\"""#,
        )
        .unwrap();
        assert_eq!(m.title.as_deref(), Some("Hello, Goodbye"));
        assert_eq!(m.artist.as_deref(), Some("The \"Fab\" Four"));
    }

    #[test]
    fn extinf_unquoted_and_case_insensitive() {
        let m = parse_extinf_title("TITLE=Song, Artist=Band").unwrap();
        assert_eq!(m.title.as_deref(), Some("Song"));
        assert_eq!(m.artist.as_deref(), Some("Band"));
    }

    #[test]
    fn extinf_plain_title_is_ignored() {
        assert!(parse_extinf_title("segment 42").is_none());
        assert!(parse_extinf_title("").is_none());
        assert!(parse_extinf_title(r#"artist="Only Artist""#).is_none());
    }

    #[test]
    fn extinf_empty_title_is_ignored() {
        assert!(parse_extinf_title(r#"title="",artist="""#).is_none());
    }

    #[test]
    fn extinf_unterminated_quote_stops_parsing() {
        assert!(parse_extinf_title(r#"title="Never ends"#).is_none());
        let m = parse_extinf_title(r#"title="Ok",artist="Never ends"#).unwrap();
        assert_eq!(m.title.as_deref(), Some("Ok"));
        assert_eq!(m.artist, None);
    }
}
