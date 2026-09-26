//! ID3 tag parsing and in-stream tag scanning
//!
//! Parses ID3v2 and ID3v1 tags into [`StreamMetadata`] using symphonia's
//! metadata readers, and provides [`Id3Scanner`], which finds tags embedded
//! in a raw MP3/AAC byte stream, cuts them out of the audio, and parses them.
//!
//! Tags show up in radio streams in a few places:
//! - HLS packed audio segments start with an ID3v2 tag (timestamp, and song
//!   info when the station sends it)
//! - HLS MPEG-TS segments carry ID3v2 tags in a metadata PES stream
//! - Icecast/SHOUTcast stations that pipe whole MP3 files carry an ID3v2 tag
//!   at the start of each track and an ID3v1 tag at the end

use std::io::Cursor;

use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataReader, StandardTag, Tag};
use symphonia::default::meta::{Id3v1Reader, Id3v2Reader};

use crate::stream::metadata::{MetadataSource, StreamMetadata};

/// Size of an ID3v2 header (and footer)
pub const ID3V2_HEADER_LEN: usize = 10;

/// Size of an ID3v1 tag
pub const ID3V1_TAG_LEN: usize = 128;

/// Largest ID3v2 tag the scanner will accept. Bigger "tags" are treated as
/// audio: a real in-stream tag with song info is far smaller, and holding
/// back more audio than this while waiting for the rest would stall playback.
pub const MAX_ID3V2_TAG_LEN: usize = 1024 * 1024;

/// Total length of the ID3v2 tag whose header starts at `data[0]`, including
/// the header and optional footer.
///
/// Returns `None` if `data` does not start with a valid ID3v2 header (or is
/// shorter than a header).
pub fn id3v2_tag_len(data: &[u8]) -> Option<usize> {
    if data.len() < ID3V2_HEADER_LEN || &data[..3] != b"ID3" {
        return None;
    }
    let major = data[3];
    let revision = data[4];
    let flags = data[5];
    if !(2..=4).contains(&major) || revision == 0xFF {
        return None;
    }
    // Undefined flag bits must be clear
    let undefined_flags = match major {
        2 => 0x3F,
        3 => 0x1F,
        _ => 0x0F,
    };
    if flags & undefined_flags != 0 {
        return None;
    }
    // Sync-safe size: the top bit of each byte must be clear
    let size_bytes = &data[6..10];
    if size_bytes.iter().any(|&b| b & 0x80 != 0) {
        return None;
    }
    let size = size_bytes
        .iter()
        .fold(0usize, |acc, &b| (acc << 7) | b as usize);
    let footer = if major == 4 && flags & 0x10 != 0 {
        ID3V2_HEADER_LEN
    } else {
        0
    };
    Some(ID3V2_HEADER_LEN + size + footer)
}

/// Parse an ID3v2 tag into song info.
///
/// Returns `None` if the tag cannot be parsed or has no title or artist
/// (e.g. the timestamp-only tag at the start of every HLS packed audio segment).
pub fn parse_id3v2(tag: &[u8]) -> Option<StreamMetadata> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(tag.to_vec())), Default::default());
    let mut reader = Id3v2Reader::try_new(mss, MetadataOptions::default()).ok()?;
    let buffer = reader.read_all().ok()?;
    metadata_from_tags(&buffer.revision.media.tags, MetadataSource::Id3v2)
}

/// Parse a 128-byte ID3v1 tag into song info.
///
/// Returns `None` if the block is not an ID3v1 tag or has no title or artist.
pub fn parse_id3v1(tag: &[u8]) -> Option<StreamMetadata> {
    if tag.len() < ID3V1_TAG_LEN || &tag[..3] != b"TAG" {
        return None;
    }
    let mss = MediaSourceStream::new(
        Box::new(Cursor::new(tag[..ID3V1_TAG_LEN].to_vec())),
        Default::default(),
    );
    let mut reader = Id3v1Reader::try_new(mss, MetadataOptions::default()).ok()?;
    let buffer = reader.read_all().ok()?;
    metadata_from_tags(&buffer.revision.media.tags, MetadataSource::Id3v1)
}

fn metadata_from_tags(tags: &[Tag], source: MetadataSource) -> Option<StreamMetadata> {
    let mut title = None;
    let mut artist = None;
    for tag in tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(s)) if title.is_none() => title = Some(s.to_string()),
            Some(StandardTag::Artist(s)) if artist.is_none() => artist = Some(s.to_string()),
            _ => {}
        }
    }
    let meta = StreamMetadata::new(title, artist, source);
    if meta.is_empty() {
        None
    } else {
        Some(meta)
    }
}

/// True if `data` starts with an MPEG audio or ADTS frame sync (11 set bits).
fn is_frame_sync(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xFF && data[1] & 0xE0 == 0xE0
}

/// True if an ID3v1 tag's text fields look like text rather than audio:
/// every byte is NUL or printable Latin-1.
fn id3v1_text_is_plausible(tag: &[u8]) -> bool {
    // Title, artist, album: bytes 3..93
    tag[3..93].iter().all(|&b| b == 0 || b >= 0x20)
}

/// What the scanner found at a candidate position
enum Candidate {
    /// Not a tag; treat the byte as audio
    NotTag,
    /// Might be a tag, but more bytes are needed to decide
    NeedMore,
    /// A complete tag of this length
    Tag(usize),
}

/// Finds ID3 tags in a raw MP3/AAC byte stream, removes them from the audio
/// and parses them.
///
/// Feed bytes with [`push`](Self::push) as they arrive. Bytes that might be
/// the start of a tag are held back until the scanner can decide, so a tag
/// split across network reads is still found. Call [`flush`](Self::flush) at
/// a known boundary (end of an HLS segment, end of stream) to release them.
///
/// To avoid cutting real audio, a tag is only accepted when it is followed by
/// an audio frame sync, another tag, or the end of the data at a flush.
#[derive(Debug, Default)]
pub struct Id3Scanner {
    pending: Vec<u8>,
}

impl Id3Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `input`, appending audio bytes to `audio` and parsed song info to `meta`.
    pub fn push(&mut self, input: &[u8], audio: &mut Vec<u8>, meta: &mut Vec<StreamMetadata>) {
        if self.pending.is_empty() {
            let used = self.scan(input, false, audio, meta);
            self.pending.extend_from_slice(&input[used..]);
        } else {
            let mut data = std::mem::take(&mut self.pending);
            data.extend_from_slice(input);
            let used = self.scan(&data, false, audio, meta);
            data.drain(..used);
            self.pending = data;
        }
    }

    /// Release held-back bytes, treating the end of the data as a boundary.
    pub fn flush(&mut self, audio: &mut Vec<u8>, meta: &mut Vec<StreamMetadata>) {
        let data = std::mem::take(&mut self.pending);
        let used = self.scan(&data, true, audio, meta);
        audio.extend_from_slice(&data[used..]);
    }

    /// Number of bytes currently held back
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Scan `data`, returning how many bytes were consumed. Unconsumed bytes
    /// start a possible tag that needs more data.
    fn scan(
        &self,
        data: &[u8],
        at_end: bool,
        audio: &mut Vec<u8>,
        meta: &mut Vec<StreamMetadata>,
    ) -> usize {
        let mut audio_start = 0;
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b != b'I' && b != b'T' {
                i += 1;
                continue;
            }
            match Self::candidate(&data[i..], at_end) {
                Candidate::NotTag => i += 1,
                Candidate::NeedMore => {
                    audio.extend_from_slice(&data[audio_start..i]);
                    return i;
                }
                Candidate::Tag(len) => {
                    audio.extend_from_slice(&data[audio_start..i]);
                    let tag = &data[i..i + len];
                    let parsed = if tag[0] == b'I' {
                        parse_id3v2(tag)
                    } else {
                        parse_id3v1(tag)
                    };
                    meta.extend(parsed);
                    i += len;
                    audio_start = i;
                }
            }
        }
        audio.extend_from_slice(&data[audio_start..]);
        data.len()
    }

    fn candidate(data: &[u8], at_end: bool) -> Candidate {
        let (len, valid_text) =
            if data.starts_with(b"ID3") || (data.len() < 3 && b"ID3".starts_with(data)) {
                if data.len() < ID3V2_HEADER_LEN {
                    return if at_end {
                        Candidate::NotTag
                    } else {
                        Candidate::NeedMore
                    };
                }
                match id3v2_tag_len(data) {
                    Some(len) if len <= MAX_ID3V2_TAG_LEN => (len, true),
                    _ => return Candidate::NotTag,
                }
            } else if data.starts_with(b"TAG") || (data.len() < 3 && b"TAG".starts_with(data)) {
                if data.len() < ID3V1_TAG_LEN {
                    return if at_end {
                        Candidate::NotTag
                    } else {
                        Candidate::NeedMore
                    };
                }
                (ID3V1_TAG_LEN, id3v1_text_is_plausible(data))
            } else {
                return Candidate::NotTag;
            };
        if !valid_text {
            return Candidate::NotTag;
        }

        // Need the whole tag plus a few bytes after it to confirm
        let after = &data[len.min(data.len())..];
        if data.len() < len || after.len() < 3 {
            return match (at_end, data.len() >= len) {
                (false, _) => Candidate::NeedMore,
                // At a boundary, a complete tag needs nothing after it
                (true, true) => Candidate::Tag(len),
                (true, false) => Candidate::NotTag,
            };
        }
        if is_frame_sync(after) || after.starts_with(b"ID3") || after.starts_with(b"TAG") {
            Candidate::Tag(len)
        } else {
            Candidate::NotTag
        }
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    /// Build an ID3v2.4 tag with the given text frames
    pub fn id3v2_tag(frames: &[(&[u8; 4], &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, text) in frames {
            let mut content = vec![3u8]; // UTF-8
            content.extend_from_slice(text.as_bytes());
            body.extend_from_slice(*id);
            body.extend_from_slice(&syncsafe(content.len()));
            body.extend_from_slice(&[0, 0]);
            body.extend(content);
        }
        let mut tag = b"ID3\x04\x00\x00".to_vec();
        tag.extend_from_slice(&syncsafe(body.len()));
        tag.extend(body);
        tag
    }

    /// Build an ID3v2.4 tag with song info
    pub fn id3v2_song(artist: &str, title: &str) -> Vec<u8> {
        id3v2_tag(&[(b"TIT2", title), (b"TPE1", artist)])
    }

    /// Build a 128-byte ID3v1 tag
    pub fn id3v1_song(artist: &str, title: &str) -> Vec<u8> {
        let field = |s: &str, n: usize| {
            let mut v = s.as_bytes().to_vec();
            v.resize(n, 0);
            v
        };
        let mut tag = b"TAG".to_vec();
        tag.extend(field(title, 30));
        tag.extend(field(artist, 30));
        tag.extend(field("", 30)); // album
        tag.extend(field("", 4)); // year
        tag.extend(field("", 30)); // comment
        tag.push(255); // genre
        tag
    }

    fn syncsafe(n: usize) -> [u8; 4] {
        [
            (n >> 21 & 0x7F) as u8,
            (n >> 14 & 0x7F) as u8,
            (n >> 7 & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]
    }

    /// A fake MPEG audio frame: sync word followed by filler that contains
    /// no tag-like bytes
    pub fn frame(len: usize) -> Vec<u8> {
        let mut f = vec![0xFF, 0xFB];
        f.resize(len, 0x55);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> (Vec<u8>, Vec<StreamMetadata>) {
        let mut scanner = Id3Scanner::new();
        let mut audio = Vec::new();
        let mut meta = Vec::new();
        for chunk in chunks {
            scanner.push(chunk, &mut audio, &mut meta);
        }
        scanner.flush(&mut audio, &mut meta);
        (audio, meta)
    }

    fn song(meta: &StreamMetadata) -> (Option<&str>, Option<&str>) {
        (meta.artist.as_deref(), meta.title.as_deref())
    }

    // --- id3v2_tag_len ---

    #[test]
    fn tag_len_of_built_tag() {
        let tag = id3v2_song("A", "B");
        assert_eq!(id3v2_tag_len(&tag), Some(tag.len()));
    }

    #[test]
    fn tag_len_with_footer() {
        let mut tag = id3v2_song("A", "B");
        tag[5] = 0x10;
        assert_eq!(id3v2_tag_len(&tag), Some(tag.len() + 10));
    }

    #[test]
    fn tag_len_rejects_bad_headers() {
        let good = id3v2_song("A", "B");
        assert_eq!(id3v2_tag_len(&good[..9]), None);
        let mut bad_version = good.clone();
        bad_version[3] = 5;
        assert_eq!(id3v2_tag_len(&bad_version), None);
        let mut bad_revision = good.clone();
        bad_revision[4] = 0xFF;
        assert_eq!(id3v2_tag_len(&bad_revision), None);
        let mut bad_flags = good.clone();
        bad_flags[5] = 0x01;
        assert_eq!(id3v2_tag_len(&bad_flags), None);
        let mut bad_size = good.clone();
        bad_size[7] = 0x80;
        assert_eq!(id3v2_tag_len(&bad_size), None);
        assert_eq!(id3v2_tag_len(b"XYZ\x04\x00\x00\x00\x00\x00\x00"), None);
    }

    // --- parse_id3v2 / parse_id3v1 ---

    #[test]
    fn parses_id3v2_song() {
        let meta = parse_id3v2(&id3v2_song("Pink Floyd", "Comfortably Numb")).unwrap();
        assert_eq!(song(&meta), (Some("Pink Floyd"), Some("Comfortably Numb")));
        assert_eq!(meta.source, MetadataSource::Id3v2);
    }

    #[test]
    fn parses_id3v2_unicode() {
        let meta = parse_id3v2(&id3v2_song("Μίκης Θεοδωράκης", "Ζορμπάς")).unwrap();
        assert_eq!(song(&meta), (Some("Μίκης Θεοδωράκης"), Some("Ζορμπάς")));
    }

    #[test]
    fn parses_id3v2_title_only() {
        let meta = parse_id3v2(&id3v2_tag(&[(b"TIT2", "Only Title")])).unwrap();
        assert_eq!(song(&meta), (None, Some("Only Title")));
    }

    #[test]
    fn id3v2_without_song_info_is_none() {
        let tag = id3v2_tag(&[(b"TALB", "Album")]);
        assert!(parse_id3v2(&tag).is_none());
        assert!(parse_id3v2(&id3v2_tag(&[(b"TIT2", "  ")])).is_none());
    }

    #[test]
    fn id3v2_hls_timestamp_tag_is_none() {
        // Packed audio segments start with a PRIV frame holding the timestamp
        let owner = b"com.apple.streaming.transportStreamTimestamp\0";
        let mut content = owner.to_vec();
        content.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x10, 0]);
        let mut frame = b"PRIV".to_vec();
        frame.extend_from_slice(&[0, 0, 0, content.len() as u8, 0, 0]);
        frame.extend(content);
        let mut tag = b"ID3\x04\x00\x00\x00\x00\x00".to_vec();
        tag.push(frame.len() as u8);
        tag.extend(frame);
        assert_eq!(id3v2_tag_len(&tag), Some(tag.len()));
        assert!(parse_id3v2(&tag).is_none());
    }

    #[test]
    fn garbage_id3v2_is_none() {
        assert!(parse_id3v2(b"ID3\x04\x00\x00\x00\x00\x00\x05abc").is_none());
        assert!(parse_id3v2(b"").is_none());
    }

    #[test]
    fn parses_id3v1_song() {
        let meta = parse_id3v1(&id3v1_song("Metallica", "One")).unwrap();
        assert_eq!(song(&meta), (Some("Metallica"), Some("One")));
        assert_eq!(meta.source, MetadataSource::Id3v1);
    }

    #[test]
    fn id3v1_rejects_non_tags() {
        assert!(parse_id3v1(&[0u8; 128]).is_none());
        assert!(parse_id3v1(&id3v1_song("A", "B")[..100]).is_none());
        assert!(parse_id3v1(&id3v1_song("", "")).is_none());
    }

    // --- Id3Scanner ---

    #[test]
    fn scanner_passes_plain_audio() {
        let audio_in = [frame(400), frame(400)].concat();
        let (audio, meta) = scan_all(&[&audio_in]);
        assert_eq!(audio, audio_in);
        assert!(meta.is_empty());
    }

    #[test]
    fn scanner_strips_leading_id3v2() {
        let a = frame(300);
        let data = [id3v2_song("Artist", "Song"), a.clone()].concat();
        let (audio, meta) = scan_all(&[&data]);
        assert_eq!(audio, a);
        assert_eq!(meta.len(), 1);
        assert_eq!(song(&meta[0]), (Some("Artist"), Some("Song")));
    }

    #[test]
    fn scanner_strips_track_boundary_tags() {
        // End of track 1 (ID3v1), start of track 2 (ID3v2)
        let a = frame(300);
        let b = frame(200);
        let data = [
            a.clone(),
            id3v1_song("Old", "Track"),
            id3v2_song("New", "Track"),
            b.clone(),
        ]
        .concat();
        let (audio, meta) = scan_all(&[&data]);
        assert_eq!(audio, [a, b].concat());
        let songs: Vec<_> = meta.iter().map(song).collect();
        assert_eq!(
            songs,
            vec![(Some("Old"), Some("Track")), (Some("New"), Some("Track"))]
        );
    }

    #[test]
    fn scanner_handles_tag_split_across_chunks() {
        let a = frame(300);
        let b = frame(300);
        let data = [a.clone(), id3v2_song("Split", "Tag"), b.clone()].concat();
        // Try every split point through the tag
        for split in 290..330 {
            let (audio, meta) = scan_all(&[&data[..split], &data[split..]]);
            assert_eq!(audio, [a.clone(), b.clone()].concat(), "split at {split}");
            assert_eq!(meta.len(), 1, "split at {split}");
        }
        // One byte at a time
        let chunks: Vec<&[u8]> = data.chunks(1).collect();
        let (audio, meta) = scan_all(&chunks);
        assert_eq!(audio, [a, b].concat());
        assert_eq!(meta.len(), 1);
    }

    #[test]
    fn scanner_holds_back_only_possible_tags() {
        let mut scanner = Id3Scanner::new();
        let mut audio = Vec::new();
        let mut meta = Vec::new();
        scanner.push(&frame(100), &mut audio, &mut meta);
        assert_eq!(scanner.pending_len(), 0);
        scanner.push(b"ID", &mut audio, &mut meta);
        assert_eq!(scanner.pending_len(), 2);
        scanner.push(b"X", &mut audio, &mut meta);
        assert_eq!(scanner.pending_len(), 0);
        assert_eq!(audio.len(), 103);
    }

    #[test]
    fn scanner_ignores_tag_not_followed_by_audio() {
        // "TAG" + plausible text but followed by garbage: keep it as audio
        let mut data = frame(100);
        data.extend(id3v1_song("A", "B"));
        data.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        let (audio, meta) = scan_all(&[&data]);
        assert_eq!(audio, data);
        assert!(meta.is_empty());
    }

    #[test]
    fn scanner_ignores_tag_like_bytes_in_audio() {
        let mut data = frame(100);
        data.extend_from_slice(b"TAG\x01\x02\x03");
        data.extend_from_slice(b"ID3\xFF\xFF\xFF\xFF");
        data.extend(frame(300));
        let (audio, meta) = scan_all(&[&data]);
        assert_eq!(audio, data);
        assert!(meta.is_empty());
    }

    #[test]
    fn scanner_rejects_oversized_tag() {
        let mut data = b"ID3\x04\x00\x00\x7F\x7F\x7F\x7F".to_vec();
        data.extend(frame(100));
        let (audio, meta) = scan_all(&[&data]);
        assert_eq!(audio, data);
        assert!(meta.is_empty());
    }

    #[test]
    fn scanner_accepts_tag_at_flush_boundary() {
        // HLS segment whose last bytes are a tag
        let a = frame(200);
        let data = [a.clone(), id3v2_song("End", "Tag")].concat();
        let (audio, meta) = scan_all(&[&data]);
        assert_eq!(audio, a);
        assert_eq!(meta.len(), 1);
    }

    #[test]
    fn scanner_flush_releases_partial_tag_as_audio() {
        let data = [frame(50), b"ID3\x04".to_vec()].concat();
        let (audio, meta) = scan_all(&[&data]);
        assert_eq!(audio, data);
        assert!(meta.is_empty());
    }

    #[test]
    fn scanner_drops_tags_without_song_info() {
        let a = frame(100);
        let data = [id3v2_tag(&[(b"TALB", "x")]), a.clone()].concat();
        let (audio, meta) = scan_all(&[&data]);
        assert_eq!(audio, a);
        assert!(meta.is_empty());
    }
}
