//! Crash tests for the parsers of what stations send.
//!
//! Playlists, TS and fMP4 segments, ID3 tags and ICY metadata all come
//! from servers we don't control, and a panic in any thread aborts the app
//! (the release profile sets `panic = "abort"`). These tests feed each
//! parser random bytes, and valid input with random damage (the deeper
//! paths are only reached by input that is mostly valid), and check that it
//! returns. Where a result can be checked cheaply, it is.
//!
//! Each test tries 256 inputs. For a longer hunt:
//! `PROPTEST_CASES=100000 cargo test -p radiotrope --lib untrusted_input`

use proptest::prelude::*;

use super::hls::{
    demux_ts_segment_with_metadata, detect_segment_format, is_valid_segment_uri,
    parse_hls_playlist, split_segment,
};
use super::hls_metadata::{extinf_titles, fmp4_emsg_id3, looks_like_fmp4, parse_extinf_title};
use super::id3::test_util::{frame, id3v1_song, id3v2_song, id3v2_tag};
use super::id3::{id3v2_tag_len, parse_id3v1, parse_id3v2, parse_id3v2_payload, Id3Scanner};
use super::metadata::{decode_html_references, extract_icy_title_as, StationText, StreamMetadata};
use super::playlist::{
    check_playlist_type, make_absolute_url, parse_m3u, parse_pls, sniff_playlist,
};

// --- Damage ---

/// A change to a valid input
#[derive(Debug, Clone)]
enum Damage {
    /// Set the byte at a position
    Set(usize, u8),
    Insert(usize, u8),
    Remove(usize),
    /// Cut the input short
    Cut(usize),
    /// Write a big-endian 32-bit number: where lengths and sizes live
    Number(usize, u32),
    /// Repeat a stretch of the input
    Repeat(usize, usize),
}

/// A byte that parsers treat specially, more often than chance would
fn telling_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        4 => any::<u8>(),
        1 => prop::sample::select(vec![0x00, 0xFF, 0x7F, 0x80, 0x47, b'#', b'\n', b'=', b',', b'\'', b'"']),
    ]
}

fn damage() -> impl Strategy<Value = Damage> {
    let number = prop_oneof![
        Just(0u32),
        Just(1),
        Just(7),
        Just(8),
        Just(16),
        Just(0x7F7F_7F7F),
        Just(0x7FFF_FFFF),
        Just(u32::MAX),
        any::<u32>(),
    ];
    prop_oneof![
        (any::<usize>(), telling_byte()).prop_map(|(at, b)| Damage::Set(at, b)),
        (any::<usize>(), telling_byte()).prop_map(|(at, b)| Damage::Insert(at, b)),
        any::<usize>().prop_map(Damage::Remove),
        any::<usize>().prop_map(Damage::Cut),
        (any::<usize>(), number).prop_map(|(at, n)| Damage::Number(at, n)),
        (any::<usize>(), 1usize..400).prop_map(|(at, len)| Damage::Repeat(at, len)),
    ]
}

fn apply(mut data: Vec<u8>, damage: &[Damage]) -> Vec<u8> {
    for d in damage {
        // Positions wrap around the input's current length
        let at = |pos: usize, len: usize| if len == 0 { 0 } else { pos % len };
        match *d {
            Damage::Set(pos, b) => {
                if !data.is_empty() {
                    let i = at(pos, data.len());
                    data[i] = b;
                }
            }
            Damage::Insert(pos, b) => {
                let i = at(pos, data.len() + 1);
                data.insert(i, b);
            }
            Damage::Remove(pos) => {
                if !data.is_empty() {
                    let i = at(pos, data.len());
                    data.remove(i);
                }
            }
            Damage::Cut(pos) => {
                let i = at(pos, data.len() + 1);
                data.truncate(i);
            }
            Damage::Number(pos, n) => {
                if data.len() >= 4 {
                    let i = at(pos, data.len() - 3);
                    data[i..i + 4].copy_from_slice(&n.to_be_bytes());
                }
            }
            Damage::Repeat(pos, len) => {
                if !data.is_empty() {
                    let i = at(pos, data.len());
                    let end = (i + len).min(data.len());
                    let stretch = data[i..end].to_vec();
                    data.splice(i..i, stretch);
                }
            }
        }
    }
    data
}

/// One of `seeds` with a few changes, or now and then random bytes
fn damaged(seeds: Vec<Vec<u8>>) -> BoxedStrategy<Vec<u8>> {
    let count = seeds.len();
    prop_oneof![
        1 => prop::collection::vec(any::<u8>(), 0..2048),
        4 => (0..count, prop::collection::vec(damage(), 0..8))
            .prop_map(move |(i, damage)| apply(seeds[i].clone(), &damage)),
    ]
    .boxed()
}

/// [`damaged`] text, as a server's bytes would be read
fn damaged_text(seeds: &[&str]) -> BoxedStrategy<String> {
    let seeds = seeds.iter().map(|s| s.as_bytes().to_vec()).collect();
    damaged(seeds)
        .prop_map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .boxed()
}

// --- MPEG-TS segments ---

const PMT_PID: u16 = 0x1000;
const AUDIO_PID: u16 = 0x100;
const META_PID: u16 = 0x101;

/// One TS packet; a short payload is padded with adaptation-field stuffing
fn ts_packet(pid: u16, unit_start: bool, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![
        0x47,
        (u8::from(unit_start) << 6) | (pid >> 8) as u8,
        pid as u8,
    ];
    if payload.len() >= 184 {
        p.push(0x10);
    } else {
        let stuffing = 183 - payload.len();
        p.extend_from_slice(&[0x30, stuffing as u8]);
        if stuffing > 0 {
            p.push(0x00);
            p.resize(p.len() + stuffing - 1, 0xFF);
        }
    }
    p.extend_from_slice(&payload[..payload.len().min(184)]);
    p
}

/// A PSI section behind its pointer field (the CRC isn't checked)
fn psi(table_id: u8, body: &[u8]) -> Vec<u8> {
    let len = body.len() + 4;
    let mut section = vec![0, table_id, 0xB0 | (len >> 8) as u8, len as u8];
    section.extend_from_slice(body);
    section.extend_from_slice(&[0; 4]);
    section
}

fn pid_bytes(pid: u16) -> [u8; 2] {
    [0xE0 | (pid >> 8) as u8, pid as u8]
}

/// PES packets (with a PTS) carrying `data`
fn pes(pid: u16, stream_id: u8, data: &[u8]) -> Vec<u8> {
    let mut pes = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
    pes.extend_from_slice(data);
    pes.chunks(184)
        .enumerate()
        .flat_map(|(i, chunk)| ts_packet(pid, i == 0, chunk))
        .collect()
}

/// ADTS frames: a header, then filler
fn adts(frames: usize) -> Vec<u8> {
    (0..frames)
        .flat_map(|_| {
            let mut f = vec![0xFF, 0xF1, 0x50, 0x80, 0x0C, 0x1F, 0xFC];
            f.resize(96, 0x21);
            f
        })
        .collect()
}

/// TS segments like stations send: PAT, PMT, AAC audio and ID3 metadata;
/// the metadata first (as RTL does); no PMT at all; junk before the packets
fn ts_seeds() -> Vec<Vec<u8>> {
    let pat = ts_packet(
        0,
        true,
        &psi(
            0x00,
            &[
                0,
                1,
                0xC1,
                0,
                0,
                0,
                1,
                pid_bytes(PMT_PID)[0],
                pid_bytes(PMT_PID)[1],
            ],
        ),
    );
    let pmt_body = |streams: &[(u8, u16)]| {
        let mut body = vec![0, 1, 0xC1, 0, 0];
        body.extend_from_slice(&pid_bytes(AUDIO_PID));
        body.extend_from_slice(&[0xF0, 0]);
        for &(stream_type, pid) in streams {
            body.push(stream_type);
            body.extend_from_slice(&pid_bytes(pid));
            body.extend_from_slice(&[0xF0, 0]);
        }
        ts_packet(PMT_PID, true, &psi(0x02, &body))
    };
    let pmt = pmt_body(&[(0x0F, AUDIO_PID), (0x15, META_PID)]);
    let tag = pes(META_PID, 0xBD, &id3v2_song("Artist", "Song"));
    let audio = pes(AUDIO_PID, 0xC0, &adts(6));
    vec![
        [pat.clone(), pmt.clone(), tag.clone(), audio.clone()].concat(),
        [tag, pat.clone(), pmt, audio.clone()].concat(),
        audio.clone(),
        [vec![0x12; 37], pat, pmt_body(&[(0x03, AUDIO_PID)]), audio].concat(),
    ]
}

proptest! {
    #[test]
    fn ts_segments_never_panic(data in damaged(ts_seeds())) {
        let (audio, tags) = demux_ts_segment_with_metadata(&data);
        // Everything returned was in the input
        prop_assert!(audio.len() + tags.iter().map(Vec::len).sum::<usize>() <= data.len());
        detect_segment_format(&data, "https://example.com/seg1.ts");
        split_segment(&data, false);
    }
}

// --- fMP4 segments ---

fn mp4_box(box_type: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(box_type);
    b.extend_from_slice(body);
    b
}

/// An `emsg` box carrying `message`, in box version 0 or 1
fn emsg(version: u8, message: &[u8]) -> Vec<u8> {
    let scheme = b"https://aomedia.org/emsg/ID3\0";
    let mut body = vec![version, 0, 0, 0];
    if version == 0 {
        body.extend_from_slice(scheme);
        body.push(0); // value
        body.extend_from_slice(&[0; 16]);
    } else {
        body.extend_from_slice(&[0; 20]);
        body.extend_from_slice(scheme);
        body.push(0);
    }
    body.extend_from_slice(message);
    mp4_box(b"emsg", &body)
}

/// fMP4 segments with ID3 in `emsg` boxes, and one with a 64-bit box size
fn fmp4_seeds() -> Vec<Vec<u8>> {
    let tag = id3v2_song("Artist", "Song");
    let media = [mp4_box(b"moof", &[0; 24]), mp4_box(b"mdat", &adts(2))].concat();
    let mut large = vec![0, 0, 0, 1];
    large.extend_from_slice(b"emsg");
    large.extend_from_slice(&(emsg(1, &tag).len() as u64 + 8).to_be_bytes());
    large.extend_from_slice(&emsg(1, &tag)[8..]);
    vec![
        [mp4_box(b"styp", b"msdhmsix"), emsg(1, &tag), media.clone()].concat(),
        [emsg(0, &tag), emsg(0, &tag), media.clone()].concat(),
        [large, media].concat(),
    ]
}

proptest! {
    #[test]
    fn fmp4_segments_never_panic(data in damaged(fmp4_seeds())) {
        for payload in fmp4_emsg_id3(&data) {
            parse_id3v2_payload(payload);
        }
        looks_like_fmp4(&data);
        split_segment(&data, true);
    }
}

// --- ID3 ---

fn id3_seeds() -> Vec<Vec<u8>> {
    let mut v3 = id3v2_tag(&[(b"TIT2", "Song"), (b"TPE1", "Άρτιστ")]);
    v3[3] = 3;
    vec![
        id3v2_song("Artist", "Song"),
        v3,
        id3v1_song("Artist", "Song"),
        [
            frame(417),
            id3v2_song("A", "B"),
            frame(417),
            id3v1_song("C", "D"),
            frame(417),
        ]
        .concat(),
        [id3v2_song("A", "B"), id3v2_song("C", "D")].concat(),
    ]
}

/// Scan `data` in the pieces `cuts` makes, as network reads would split it
fn scan_in_pieces(data: &[u8], cuts: &[usize]) -> (Vec<u8>, Vec<StreamMetadata>) {
    let mut scanner = Id3Scanner::new();
    let mut audio = Vec::new();
    let mut meta = Vec::new();
    let mut rest = data;
    for &cut in cuts {
        let (piece, after) = rest.split_at(cut.min(rest.len()));
        scanner.push(piece, &mut audio, &mut meta);
        rest = after;
    }
    scanner.push(rest, &mut audio, &mut meta);
    scanner.flush(&mut audio, &mut meta);
    (audio, meta)
}

proptest! {
    #[test]
    fn id3_tags_never_panic(data in damaged(id3_seeds())) {
        if let Some(len) = id3v2_tag_len(&data) {
            prop_assert!(len >= 10);
        }
        parse_id3v2(&data);
        parse_id3v1(&data);
        parse_id3v2_payload(&data);
    }

    #[test]
    fn id3_scanning_is_the_same_however_the_stream_is_split(
        data in damaged(id3_seeds()),
        cuts in prop::collection::vec(0usize..300, 0..12),
    ) {
        let (audio, meta) = scan_in_pieces(&data, &[]);
        prop_assert!(audio.len() <= data.len());
        let (split_audio, split_meta) = scan_in_pieces(&data, &cuts);
        prop_assert_eq!(split_audio, audio);
        prop_assert_eq!(split_meta, meta);
    }
}

// --- HLS playlists ---

const HLS_SEEDS: [&str; 3] = [
    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6.0\n#EXT-X-MEDIA-SEQUENCE:1727384723\n\
     #EXT-X-PROGRAM-DATE-TIME:2026-09-28T10:00:00.000Z\n\
     #EXTINF:6.0,title=\"Song, with comma\",artist=\"Band\",url=\"https://x/?a=1\"\nseg1.ts?t=1\n\
     #EXT-X-DISCONTINUITY\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:5.97 tvg-id=\"x\",\n/abs/seg2.aac\n\
     #EXT-X-BYTERANGE:1024@0\n#EXTINF:6,\nhttps://cdn.example.com/seg3.ts\n",
    "\u{feff}#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
     #EXT-X-KEY:METHOD=AES-128,URI=\"key\",IV=0x1234\n#EXTINF:4.0,\nseg1.m4s\n#EXT-X-ENDLIST\n",
    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\nlow/index.m3u8\n\
     #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1,URI=\"iframe.m3u8\"\n\
     #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"en\",URI=\"en.m3u8\"\n\
     #EXT-X-STREAM-INF:BANDWIDTH=64000\n../high.m3u8?token=abc\n",
];

proptest! {
    #[test]
    fn hls_playlists_never_panic(text in damaged_text(&HLS_SEEDS)) {
        let _ = parse_hls_playlist(text.as_bytes());
        for (uri, title) in extinf_titles(text.as_bytes()) {
            is_valid_segment_uri(&uri);
            parse_extinf_title(&title);
        }
    }

    #[test]
    fn extinf_titles_never_panic(title in damaged_text(&[
        "title=\"Song, \\\"quoted\\\"\",artist=\"Band\"",
        "TITLE=Song,Artist='Band'",
        "title=\"unterminated",
    ])) {
        parse_extinf_title(&title);
    }
}

// --- PLS and M3U playlists ---

const PLAYLIST_SEEDS: [&str; 5] = [
    "[playlist]\nNumberOfEntries=2\nFile1=http://s1.example.com:8000/live?sid=1&t=a=b\n\
     Title1=Station\nLength1=-1\nFile2=https://s2.example.com/\nVersion=2\n",
    "\u{feff}#EXTM3U\n#EXTINF:-1,Station\nhttp://example.com/stream\n",
    "#EXTM3U\n# comment\n\n../up/stream.mp3\n",
    "//host.example.com/path?x=1\n",
    "#EXTM3U\nVersion=2\nlive.mp3?sid=1&t=a=b\n",
];

proptest! {
    #[test]
    fn playlists_never_panic(
        text in damaged_text(&PLAYLIST_SEEDS),
        content_type in prop::option::of(prop::sample::select(vec![
            "audio/x-scpls", "audio/mpegurl", "application/vnd.apple.mpegurl",
            "text/html; charset=utf-8", "audio/mpeg", "",
        ])),
    ) {
        sniff_playlist(content_type, text.as_bytes());
        if let Some(url) = parse_pls(&text) {
            prop_assert!(!url.is_empty());
        }
        for base in ["http://example.com/dir", "", "not a url"] {
            if let Some(url) = parse_m3u(&text, base) {
                prop_assert!(!url.is_empty());
            }
        }
        check_playlist_type(&text);
        make_absolute_url(&text, "http://example.com/dir");
    }
}

// --- ICY metadata ---

/// Metadata blocks as ICY servers send them: padded with NULs, titles in
/// UTF-8, Windows-1253 (Greek) or as HTML codes
fn icy_seeds() -> Vec<Vec<u8>> {
    let pad = |mut block: Vec<u8>| {
        block.resize(block.len().div_ceil(16) * 16, 0);
        block
    };
    let greek = "StreamTitle='Καλλιτέχνης - Τραγούδι';";
    vec![
        pad(b"StreamTitle='Artist - Song';StreamUrl='http://x/';".to_vec()),
        pad(greek.as_bytes().to_vec()),
        pad(encoding_rs::WINDOWS_1253.encode(greek).0.into_owned()),
        pad(b"StreamTitle='&#924;&#x39C;&amp;#924; R&B &amp; Soul &#39;';".to_vec()),
        pad(b"StreamTitle='';".to_vec()),
    ]
}

proptest! {
    #[test]
    fn icy_metadata_never_panics(block in damaged(icy_seeds())) {
        let mut text = StationText::for_url("http://radio.example.gr/live");
        if let Some(title) = extract_icy_title_as(&block, &mut text) {
            StreamMetadata::from_icy_title(&title);
        }
        text.decode(&block);
    }

    #[test]
    fn html_codes_never_panic(text in damaged_text(&[
        "&#924;&#x39C;&amp;#924;&#39;&#1114111;&#xD800;&#99999999999;&amp;&lt;&gt;&quot;&apos;&nbsp;",
        "Simon & Garfunkel &#; &#x; &amp R&B &#9",
    ])) {
        decode_html_references(&text);
    }
}

// --- The seeds ---

/// Undamaged, every seed reaches the parsing it is there for: otherwise the
/// tests above would only ever check the first bytes
#[test]
fn the_seeds_are_valid_input() {
    for seed in ts_seeds() {
        let (audio, meta) = split_segment(&seed, false);
        assert_eq!(audio, adts(6));
        assert!(meta.len() <= 1);
    }
    let (_, meta) = split_segment(&ts_seeds()[0], false);
    assert_eq!(meta[0].title.as_deref(), Some("Song"));

    for seed in fmp4_seeds() {
        let payloads = fmp4_emsg_id3(&seed);
        assert!(!payloads.is_empty());
        for payload in payloads {
            assert_eq!(parse_id3v2_payload(payload).len(), 1);
        }
    }

    let seeds = id3_seeds();
    assert!(parse_id3v2(&seeds[0]).is_some());
    assert!(parse_id3v2(&seeds[1]).is_some());
    assert!(parse_id3v1(&seeds[2]).is_some());
    assert_eq!(scan_in_pieces(&seeds[3], &[]).1.len(), 2);
    assert_eq!(parse_id3v2_payload(&seeds[4]).len(), 2);

    for seed in HLS_SEEDS {
        assert!(parse_hls_playlist(seed.as_bytes()).is_ok(), "{seed}");
    }
    let titles = extinf_titles(HLS_SEEDS[0].as_bytes());
    assert!(titles.values().any(|t| parse_extinf_title(t).is_some()));

    assert!(parse_pls(PLAYLIST_SEEDS[0]).is_some());
    for seed in &PLAYLIST_SEEDS[1..] {
        assert!(
            parse_m3u(seed, "http://example.com/dir").is_some(),
            "{seed}"
        );
    }

    let mut text = StationText::for_url("http://radio.example.gr/live");
    let titles: Vec<_> = icy_seeds()
        .iter()
        .map(|block| extract_icy_title_as(block, &mut text))
        .collect();
    assert!(titles[..4].iter().all(Option::is_some), "{titles:?}");
    assert_eq!(titles[1], titles[2], "Windows-1253 reads as Greek");
}
