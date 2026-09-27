//! HLS stream reader
//!
//! Downloads HLS segments in the background, handles MPEG-TS demuxing and
//! fMP4 init segments, and provides a Read+Seek interface for the audio engine.
//! Song info found in the segments (ID3 timed metadata) or on the playlist's
//! `#EXTINF` lines is sent on a metadata channel.

use std::collections::HashSet;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use m3u8_rs::{MediaPlaylist, Playlist};
use reqwest::Url;

use crate::config::hls::{FIRST_SEGMENT_TIMEOUT_SECS, SEGMENT_BUFFER_SIZE, SEGMENT_TIMEOUT_SECS};
use crate::config::network::USER_AGENT;
use crate::config::timeouts::{CONNECT_TIMEOUT_SECS, RECONNECT_GIVE_UP_SECS};
use crate::error::{RadioError, Result};
use crate::stream::hls_metadata::{
    extinf_titles, fmp4_emsg_id3, looks_like_fmp4, parse_extinf_title,
};
use crate::stream::id3::{parse_id3v2_payload, Id3Scanner};
use crate::stream::metadata::{MetadataSink, StreamMetadata};
use crate::stream::{gave_up, StreamEnd, READ_POLL_INTERVAL};

/// Detected segment container format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsSegmentFormat {
    MpegTs,
    Fmp4,
    Raw,
}

/// Longest playlist reload wait we accept from `#EXT-X-TARGETDURATION`.
/// Radio segments are 2-10 s; RFC 8216 puts no bound on the tag.
const MAX_TARGET_DURATION_SECS: u64 = 60;

/// HLS stream reader — downloads segments in background, provides Read+Seek
pub struct HlsReader {
    buffer: Cursor<Vec<u8>>,
    receiver: Receiver<Vec<u8>>,
    stop_flag: Arc<AtomicBool>,
    /// How the downloader ended, once it has
    end: StreamEnd,
    _handle: Option<JoinHandle<()>>,
    pub detected_format: HlsSegmentFormat,
    /// Total bytes received from the network (updated by background thread)
    pub bytes_received: Arc<AtomicU64>,
    /// Total HLS segments downloaded (updated by background thread)
    pub segments_downloaded: Arc<AtomicU64>,
}

// Safe: HlsReader is only accessed from one thread at a time (the audio engine thread).
unsafe impl Sync for HlsReader {}

impl HlsReader {
    /// Create a new HLS reader for a media playlist URL.
    ///
    /// Returns the reader and a channel that receives song info updates. With
    /// `playback_position` (bytes of this reader's output the decoder has
    /// read), updates are held until playback reaches the segment they came with.
    ///
    /// Fails with the downloader's reason (a segment's HTTP status, no audio
    /// track, encryption, ...) when no audio arrives, rather than a bare timeout.
    /// Once playing, the downloader gives up after [`RECONNECT_GIVE_UP_SECS`]
    /// without a new segment, and reading returns an error with the reason.
    pub fn new(
        media_url: &str,
        playback_position: Option<Arc<AtomicU64>>,
    ) -> Result<(Self, Receiver<StreamMetadata>)> {
        Self::open(
            media_url,
            playback_position,
            Duration::from_secs(RECONNECT_GIVE_UP_SECS),
        )
    }

    fn open(
        media_url: &str,
        playback_position: Option<Arc<AtomicU64>>,
        give_up: Duration,
    ) -> Result<(Self, Receiver<StreamMetadata>)> {
        let playlist_url = Url::parse(media_url)
            .map_err(|e| RadioError::Stream(format!("Invalid HLS URL {media_url}: {e}")))?;
        let (sender, receiver) = bounded::<Vec<u8>>(SEGMENT_BUFFER_SIZE);
        let (metadata_sink, metadata_rx) = match playback_position {
            Some(position) => MetadataSink::synced(position),
            None => MetadataSink::channel(),
        };
        let stop_flag = Arc::new(AtomicBool::new(false));
        let bytes_received = Arc::new(AtomicU64::new(0));
        let segments_downloaded = Arc::new(AtomicU64::new(0));
        let problem: Arc<Mutex<Option<String>>> = Arc::default();
        let end = StreamEnd::default();

        let downloader = SegmentDownloader {
            playlist_url,
            sender,
            metadata_sink,
            stop_flag: stop_flag.clone(),
            bytes_received: bytes_received.clone(),
            segments_downloaded: segments_downloaded.clone(),
            problem: problem.clone(),
            end: end.clone(),
            give_up,
        };
        let handle = thread::Builder::new()
            .name("hls-downloader".into())
            .spawn(move || downloader.run())?;

        // Wait for the first segment. On failure, stop the downloader so it
        // doesn't keep polling the playlist for a reader that never existed.
        let initial_data =
            match receiver.recv_timeout(Duration::from_secs(FIRST_SEGMENT_TIMEOUT_SECS)) {
                Ok(data) => data,
                Err(e) => {
                    stop_flag.store(true, Ordering::SeqCst);
                    let reason = problem.lock().ok().and_then(|mut p| p.take());
                    return Err(match (e, reason) {
                        (_, Some(reason)) => RadioError::Stream(reason),
                        (RecvTimeoutError::Timeout, None) => {
                            RadioError::Timeout("Timeout waiting for first HLS segment".to_string())
                        }
                        (RecvTimeoutError::Disconnected, None) => {
                            RadioError::Stream("HLS stream ended before any audio".to_string())
                        }
                    });
                }
            };

        let detected_format = detect_segment_format(&initial_data, media_url);

        Ok((
            Self {
                buffer: Cursor::new(initial_data),
                receiver,
                stop_flag,
                end,
                _handle: Some(handle),
                detected_format,
                bytes_received,
                segments_downloaded,
            },
            metadata_rx,
        ))
    }

    /// Create an HlsReader from a test channel (bypasses HTTP)
    #[cfg(test)]
    pub fn from_test_channel(
        receiver: Receiver<Vec<u8>>,
        initial_data: Vec<u8>,
    ) -> (Self, Arc<AtomicBool>) {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let format = detect_segment_format(&initial_data, "test.ts");
        (
            Self {
                buffer: Cursor::new(initial_data),
                receiver,
                stop_flag,
                end: StreamEnd::default(),
                _handle: None,
                detected_format: format,
                bytes_received: Arc::new(AtomicU64::new(0)),
                segments_downloaded: Arc::new(AtomicU64::new(0)),
            },
            stop_clone,
        )
    }

    fn fill_buffer(&mut self) -> io::Result<()> {
        // Drain pending segments (non-blocking)
        while let Ok(data) = self.receiver.try_recv() {
            let pos = self.buffer.position() as usize;

            // Compact: remove already-read data
            if pos > 0 {
                let buf = self.buffer.get_mut();
                if pos <= buf.len() {
                    buf.drain(0..pos);
                }
            }
            self.buffer.set_position(0);
            self.buffer.get_mut().extend(data);
        }

        // If buffer exhausted, wait briefly for the next segment. The
        // downloader keeps retrying on network errors, so a timeout is not
        // the end: return `Interrupted` and let the caller (the stream
        // buffer's producer) check its stop flag and read again. Blocking
        // here instead would keep a stopped station's threads alive for as
        // long as it stays down.
        let remaining = self
            .buffer
            .get_ref()
            .len()
            .saturating_sub(self.buffer.position() as usize);
        if remaining == 0 {
            if self.stop_flag.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "HLS stream stopped",
                ));
            }

            match self.receiver.recv_timeout(READ_POLL_INTERVAL) {
                Ok(data) => {
                    self.buffer.get_mut().clear();
                    self.buffer.set_position(0);
                    self.buffer.get_mut().extend(data);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "waiting for the next HLS segment",
                    ));
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    // Downloader exited: the playlist ended (the empty
                    // buffer then reads as end of stream), or it gave up
                    return self.end.read_result("HLS stream ended").map(|_| ());
                }
            }
        }

        Ok(())
    }
}

impl Read for HlsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.buffer.read(buf)?;
        if n > 0 {
            return Ok(n);
        }

        self.fill_buffer()?;
        self.buffer.read(buf)
    }
}

impl Seek for HlsReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.buffer.seek(pos)
    }
}

impl Drop for HlsReader {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }
}

/// Detect the container format of an HLS segment from its data and URL
pub fn detect_segment_format(data: &[u8], url: &str) -> HlsSegmentFormat {
    if data.len() >= 8 {
        // Check for fMP4 box headers
        let magic = &data[4..8];
        if magic == b"ftyp" || magic == b"moof" || magic == b"moov" {
            return HlsSegmentFormat::Fmp4;
        }
    }

    // Check for MPEG-TS sync byte
    if !data.is_empty() && data[0] == 0x47 {
        return HlsSegmentFormat::MpegTs;
    }

    // Check URL extension
    let lower = url.to_lowercase();
    if lower.ends_with(".ts") || lower.contains(".ts?") {
        return HlsSegmentFormat::MpegTs;
    }
    if lower.ends_with(".m4s") || lower.contains(".m4s?") {
        return HlsSegmentFormat::Fmp4;
    }

    HlsSegmentFormat::Raw
}

/// Check if a segment URI can be a real segment.
///
/// RFC 8216 makes every non-blank line that isn't a tag a URI, and servers
/// name segments anything (`seg.ts`, `chunk.mp4?t=1`, `1727384723`), so only
/// empty lines and `key="value"` fragments are rejected. The fragments came
/// from m3u8-rs splitting `#EXTINF` titles at commas, which
/// [`parse_hls_playlist`] now avoids; the check stays as a safety net.
pub fn is_valid_segment_uri(uri: &str) -> bool {
    let trimmed = uri.trim();
    !trimmed.is_empty() && !trimmed.contains("=\"") && !trimmed.contains("='")
}

/// Size of an MPEG-TS packet
const TS_PACKET_LEN: usize = 188;

/// PMT stream types of audio we can decode, most preferred first: AAC ADTS,
/// AAC LATM, MPEG-4 audio, MPEG-1/2 audio.
const TS_AUDIO_TYPES: [u8; 5] = [0x0F, 0x11, 0x1C, 0x03, 0x04];

/// Audio stream types used as a last resort (AC-3, and 0x80 which some
/// muxers use for private audio)
const TS_FALLBACK_AUDIO_TYPES: [u8; 2] = [0x81, 0x80];

/// PMT stream type of ID3 timed metadata in HLS
const TS_METADATA_TYPE: u8 = 0x15;

/// Demux an MPEG-TS segment and extract audio data
pub fn demux_ts_segment(ts_data: &[u8]) -> Vec<u8> {
    demux_ts_segment_with_metadata(ts_data).0
}

/// Demux an MPEG-TS segment, returning its audio data and the payload of
/// each timed-metadata PES packet (PMT stream type 0x15, which HLS uses for
/// ID3 tags).
///
/// Lenient on purpose: packets it doesn't understand (unknown stream types,
/// PES header extensions, broken or missing sync) are skipped, never fatal
/// for the rest of the segment. The PAT/PMT are read first, since streams
/// may send packets before them (RTL's segments start with the ID3 packet).
/// Only the first audio stream is taken, so a second language track can't
/// interleave with the first. Without a PMT, the first PES stream with an
/// MPEG audio stream id (0xC0-0xDF) is used.
pub fn demux_ts_segment_with_metadata(ts_data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    // First pass: find the audio and metadata PIDs
    let mut pmt_pids: HashSet<u16> = HashSet::new();
    let mut audio_pid: Option<u16> = None;
    let mut metadata_pids: HashSet<u16> = HashSet::new();
    for (pid, unit_start, payload) in ts_packets(ts_data) {
        if !unit_start {
            continue;
        }
        if pid == 0 {
            pmt_pids.extend(parse_pat(payload));
        } else if pmt_pids.contains(&pid) {
            if let Some((audio, meta)) = parse_pmt(payload) {
                metadata_pids.extend(meta);
                if audio.is_some() {
                    audio_pid = audio;
                    break;
                }
            }
        }
    }

    // Second pass: collect the audio and metadata payloads
    let mut audio_data = Vec::with_capacity(ts_data.len());
    let mut metadata: Vec<Vec<u8>> = Vec::new();
    // A metadata PES that is still receiving continuation packets
    let mut metadata_open = false;
    for (pid, unit_start, payload) in ts_packets(ts_data) {
        if pid == 0 || pmt_pids.contains(&pid) {
            continue;
        }
        if metadata_pids.contains(&pid) {
            if unit_start {
                metadata_open = false;
                if let Some(data) = pes_payload(payload) {
                    metadata.push(data.to_vec());
                    metadata_open = true;
                }
            } else if metadata_open {
                if let Some(last) = metadata.last_mut() {
                    last.extend_from_slice(payload);
                }
            }
            continue;
        }
        if audio_pid.is_none() && unit_start && is_audio_pes(payload) {
            // No PMT: take the first MPEG audio PES stream
            audio_pid = Some(pid);
        }
        if audio_pid == Some(pid) {
            if unit_start {
                if let Some(data) = pes_payload(payload) {
                    audio_data.extend_from_slice(data);
                }
            } else {
                audio_data.extend_from_slice(payload);
            }
        }
    }

    (audio_data, metadata)
}

/// The TS packets in `ts_data` as (PID, payload unit start, payload).
/// Resyncs on lost sync; skips corrupt packets and ones without a payload.
fn ts_packets(ts_data: &[u8]) -> impl Iterator<Item = (u16, bool, &[u8])> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        while pos + TS_PACKET_LEN <= ts_data.len() {
            if ts_data[pos] != 0x47 {
                // Lost sync: find the next sync byte
                pos += 1;
                continue;
            }
            let packet = &ts_data[pos..pos + TS_PACKET_LEN];
            pos += TS_PACKET_LEN;

            let transport_error = packet[1] & 0x80 != 0;
            let unit_start = packet[1] & 0x40 != 0;
            let pid = u16::from(packet[1] & 0x1F) << 8 | u16::from(packet[2]);
            let adaptation_control = (packet[3] >> 4) & 0x03;
            if transport_error || adaptation_control & 0x01 == 0 {
                continue; // corrupt, or adaptation field only
            }
            let mut start = 4;
            if adaptation_control & 0x02 != 0 {
                start += 1 + usize::from(packet[4]);
            }
            if start < TS_PACKET_LEN {
                return Some((pid, unit_start, &packet[start..]));
            }
        }
        None
    })
}

/// The body of the PSI section starting in `payload` (after the pointer
/// field), up to but excluding the CRC. `None` if it doesn't fit the packet.
fn psi_section(payload: &[u8], table_id: u8) -> Option<&[u8]> {
    let pointer = usize::from(*payload.first()?);
    let section = payload.get(1 + pointer..)?;
    if *section.first()? != table_id {
        return None;
    }
    let length = usize::from(section.get(1)? & 0x0F) << 8 | usize::from(*section.get(2)?);
    // header (3 bytes) + length, minus the 4-byte CRC
    section.get(3..(3 + length).checked_sub(4)?)
}

/// PMT PIDs listed in a PAT
fn parse_pat(payload: &[u8]) -> Vec<u16> {
    let Some(section) = psi_section(payload, 0x00) else {
        return Vec::new();
    };
    // transport_stream_id(2) version(1) section_number(1) last_section(1)
    section
        .get(5..)
        .unwrap_or_default()
        .chunks_exact(4)
        .filter(|entry| entry[0] != 0 || entry[1] != 0) // program 0 = network PID
        .map(|entry| u16::from(entry[2] & 0x1F) << 8 | u16::from(entry[3]))
        .collect()
}

/// The audio PID and the ID3 metadata PIDs listed in a PMT
fn parse_pmt(payload: &[u8]) -> Option<(Option<u16>, Vec<u16>)> {
    let section = psi_section(payload, 0x02)?;
    // program_number(2) version(1) section_number(1) last_section(1) PCR_PID(2)
    let program_info_len = usize::from(section.get(7)? & 0x0F) << 8 | usize::from(*section.get(8)?);
    let mut es = section.get(9 + program_info_len..)?;
    let mut streams = Vec::new();
    while es.len() >= 5 {
        let stream_type = es[0];
        let pid = u16::from(es[1] & 0x1F) << 8 | u16::from(es[2]);
        let info_len = usize::from(es[3] & 0x0F) << 8 | usize::from(es[4]);
        streams.push((stream_type, pid));
        es = es.get(5 + info_len..).unwrap_or_default();
    }
    let pick = |types: &[u8]| {
        types
            .iter()
            .find_map(|t| streams.iter().find(|(st, _)| st == t).map(|&(_, pid)| pid))
    };
    let audio = pick(&TS_AUDIO_TYPES).or_else(|| pick(&TS_FALLBACK_AUDIO_TYPES));
    let metadata = streams
        .iter()
        .filter(|(st, _)| *st == TS_METADATA_TYPE)
        .map(|&(_, pid)| pid)
        .collect();
    Some((audio, metadata))
}

/// True if `payload` starts a PES packet with an MPEG audio stream id
fn is_audio_pes(payload: &[u8]) -> bool {
    payload.len() >= 4 && payload[..3] == [0, 0, 1] && (0xC0..=0xDF).contains(&payload[3])
}

/// The data of the PES packet starting in `payload`, after its header
fn pes_payload(payload: &[u8]) -> Option<&[u8]> {
    if payload.len() < 9 || payload[..3] != [0, 0, 1] {
        return None;
    }
    match payload[3] {
        // Stream ids without the optional PES header
        0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF => payload.get(6..),
        _ => payload.get(9 + usize::from(payload[8])..),
    }
}

/// Split a downloaded segment into its audio bytes and the song info it carries.
///
/// `is_fmp4` is true when the playlist declares an init segment
/// (`EXT-X-MAP`); fMP4 audio is passed through untouched (the caller
/// prepends the init segment).
pub fn split_segment(data: &[u8], is_fmp4: bool) -> (Vec<u8>, Vec<StreamMetadata>) {
    if !data.is_empty() && data[0] == 0x47 {
        // MPEG-TS: demux audio and the ID3 metadata stream
        let (audio, payloads) = demux_ts_segment_with_metadata(data);
        let meta = payloads
            .iter()
            .flat_map(|p| parse_id3v2_payload(p))
            .collect();
        (audio, meta)
    } else if is_fmp4 || looks_like_fmp4(data) {
        // fMP4/CMAF: ID3 in emsg boxes
        let meta = fmp4_emsg_id3(data)
            .into_iter()
            .flat_map(parse_id3v2_payload)
            .collect();
        (data.to_vec(), meta)
    } else {
        // Packed audio (raw AAC/ADTS or MP3): cut out ID3 tags
        let mut scanner = Id3Scanner::new();
        let mut audio = Vec::with_capacity(data.len());
        let mut meta = Vec::new();
        scanner.push(data, &mut audio, &mut meta);
        scanner.flush(&mut audio, &mut meta);
        (audio, meta)
    }
}

/// Parse playlist text with m3u8-rs, after removing what trips it up: a
/// UTF-8 byte order mark, and `#EXTINF` titles. m3u8-rs ends a title at its
/// first comma and reads the rest of the line as a segment URI; the titles
/// are read from the raw text by [`extinf_titles`] instead.
pub fn parse_hls_playlist(content: &[u8]) -> std::result::Result<Playlist, String> {
    let text = String::from_utf8_lossy(content);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    let mut clean = String::with_capacity(text.len());
    for line in text.lines() {
        let line = line.trim_end();
        match line.strip_prefix("#EXTINF:") {
            Some(rest) => {
                let duration = rest
                    .split(',')
                    .next()
                    .and_then(|d| d.split_whitespace().next())
                    .unwrap_or("0");
                clean.push_str("#EXTINF:");
                clean.push_str(duration);
                clean.push(',');
            }
            None => clean.push_str(&integer_tag_without_decimals(line)),
        }
        clean.push('\n');
    }
    m3u8_rs::parse_playlist_res(clean.as_bytes()).map_err(|_| "Invalid HLS playlist".to_string())
}

/// m3u8-rs reads these tags' values as integers and stops at a '.', leaving
/// the rest (`.0`) to be taken as a segment URI. Some packagers write
/// `#EXT-X-TARGETDURATION:6.0`, so drop the fraction.
fn integer_tag_without_decimals(line: &str) -> std::borrow::Cow<'_, str> {
    const INTEGER_TAGS: [&str; 4] = [
        "#EXT-X-TARGETDURATION:",
        "#EXT-X-VERSION:",
        "#EXT-X-MEDIA-SEQUENCE:",
        "#EXT-X-DISCONTINUITY-SEQUENCE:",
    ];
    for tag in INTEGER_TAGS {
        if let Some(value) = line.strip_prefix(tag) {
            if let Some((whole, _)) = value.split_once('.') {
                return format!("{tag}{whole}").into();
            }
        }
    }
    line.into()
}

/// GET `url`, returning the final URL (after redirects) and the body.
/// Errors are short, user-facing reasons.
fn http_get(
    client: &reqwest::blocking::Client,
    url: &str,
) -> std::result::Result<(Url, Vec<u8>), String> {
    let response = client
        .get(url)
        .send()
        .map_err(|e| RadioError::from(e).to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let final_url = response.url().clone();
    let body = response
        .bytes()
        .map_err(|e| RadioError::from(e).to_string())?;
    Ok((final_url, body.to_vec()))
}

/// Resolve an HLS URL — follows master playlists to find the media playlist.
///
/// Returns the media playlist's final URL, after any HTTP redirects, since
/// relative segment URIs are relative to where the playlist really is.
pub fn resolve_hls_url(url: &str) -> Result<String> {
    let lower = url.to_lowercase();
    if !lower.ends_with(".m3u8") && !lower.contains(".m3u8?") {
        return Err(RadioError::Stream("Not an HLS URL".to_string()));
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(SEGMENT_TIMEOUT_SECS))
        .build()?;

    let mut url = url.to_string();
    for _ in 0..5 {
        let response = client.get(&url).send()?;
        if !response.status().is_success() {
            return Err(RadioError::Stream(format!("HTTP {}", response.status())));
        }
        let playlist_url = response.url().clone();
        let content = response.bytes()?;

        match parse_hls_playlist(&content).map_err(RadioError::Stream)? {
            Playlist::MasterPlaylist(master) => {
                let variant = master
                    .variants
                    .iter()
                    .find(|v| !v.is_i_frame)
                    .ok_or_else(|| {
                        RadioError::Stream("No variants in master playlist".to_string())
                    })?;
                url = playlist_url
                    .join(variant.uri.trim())
                    .map_err(|e| RadioError::Stream(format!("Bad variant URI: {e}")))?
                    .to_string();
            }
            Playlist::MediaPlaylist(_) => return Ok(playlist_url.to_string()),
        }
    }
    Err(RadioError::Stream(
        "HLS playlist nesting too deep".to_string(),
    ))
}

use super::backoff_sleep;

/// Why a playlist can't be played, if it is encrypted
fn encryption_method(playlist: &MediaPlaylist) -> Option<String> {
    // m3u8-rs attaches EXT-X-KEY to the first segment after it only
    playlist.segments.iter().find_map(|s| {
        s.key
            .as_ref()
            .filter(|k| k.method != m3u8_rs::KeyMethod::None)
            .map(|k| k.method.to_string())
    })
}

/// Background segment downloader
///
/// Uses URL-based deduplication to track which segments have been downloaded.
/// This handles both compliant and non-compliant HLS servers — some servers
/// keep `EXT-X-MEDIA-SEQUENCE` at 0 across playlist refreshes even as they
/// rotate segment URLs, which breaks sequence-number-based dedup.
///
/// Segment URIs are resolved against the playlist's final URL (after
/// redirects) with RFC 3986 rules, so `/abs/path`, `../dir/seg` and
/// `//host/seg` work. Problems are written to `problem`, which
/// [`HlsReader::new`] reports if no audio arrives. Once playing, the
/// downloader gives up after `give_up` without a new segment.
struct SegmentDownloader {
    playlist_url: Url,
    sender: Sender<Vec<u8>>,
    metadata_sink: MetadataSink,
    stop_flag: Arc<AtomicBool>,
    bytes_received: Arc<AtomicU64>,
    segments_downloaded: Arc<AtomicU64>,
    problem: Arc<Mutex<Option<String>>>,
    end: StreamEnd,
    give_up: Duration,
}

impl SegmentDownloader {
    fn run(self) {
        match self.download() {
            Ok(()) => self.end.finish(),
            Err(reason) => {
                self.report(reason.clone());
                self.end.fail(reason);
            }
        }
    }

    fn report(&self, reason: String) {
        if let Ok(mut problem) = self.problem.lock() {
            *problem = Some(reason);
        }
    }

    fn stopped(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }

    /// Sleep for `duration` unless stopped first. Returns false if stopped.
    fn sleep(&self, duration: Duration) -> bool {
        let now = std::time::Instant::now();
        let deadline = now
            .checked_add(duration)
            .unwrap_or(now + Duration::from_secs(3600));
        loop {
            if self.stopped() {
                return false;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return true;
            }
            thread::sleep(left.min(Duration::from_millis(250)));
        }
    }

    /// Download until stopped or the stream ends. `Err` is a reason the
    /// stream can't be played at all.
    fn download(&self) -> std::result::Result<(), String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(SEGMENT_TIMEOUT_SECS * 2))
            .build()
            .map_err(|e| format!("HTTP client error: {}", e))?;

        // URL-based dedup: tracks already-downloaded segment URLs
        let mut downloaded_urls: HashSet<String> = HashSet::new();
        let mut first_fetch = true;
        let mut sent_first = false;
        // fMP4 init segment (EXT-X-MAP) and its URL, until the first segment
        let mut init_segment: Option<(String, Vec<u8>)> = None;
        let mut consecutive_failures: u32 = 0;
        // Audio bytes sent to the reader so far (stream offset for song info)
        let mut bytes_sent: u64 = 0;
        // When the last segment was sent, and what went wrong since
        let mut last_audio = Instant::now();
        let mut last_problem: Option<String> = None;

        loop {
            if self.stopped() {
                return Ok(());
            }

            // Fetch the playlist
            let fetched = http_get(&client, self.playlist_url.as_str()).and_then(|(url, body)| {
                parse_hls_playlist(&body).map(|playlist| (url, body, playlist))
            });
            let (playlist_url, content, playlist) = match fetched {
                Ok((url, body, Playlist::MediaPlaylist(pl))) => (url, body, pl),
                Ok((_, _, Playlist::MasterPlaylist(_))) => {
                    return Err("Expected an HLS media playlist, got a master playlist".into())
                }
                Err(reason) => {
                    let reason = format!("HLS playlist {}: {reason}", self.playlist_url);
                    self.report(reason.clone());
                    if sent_first && last_audio.elapsed() >= self.give_up {
                        return Err(gave_up(self.give_up, &reason));
                    }
                    last_problem = Some(reason);
                    consecutive_failures += 1;
                    if !backoff_sleep(consecutive_failures, &self.stop_flag) {
                        return Ok(());
                    }
                    continue;
                }
            };

            // Playlist fetched successfully — reset backoff
            consecutive_failures = 0;

            if let Some(method) = encryption_method(&playlist) {
                return Err(format!(
                    "Encrypted HLS streams ({method}) are not supported"
                ));
            }

            // m3u8-rs cuts #EXTINF titles at the first comma, so read the full
            // titles (which may carry title="…",artist="…") from the raw text
            let segment_titles = extinf_titles(&content);

            let is_live = !playlist.end_list;
            // Untrusted: a bogus huge value would overflow `Instant + Duration`
            // (a panic, which aborts the release build) in `sleep`
            let target_duration = playlist.target_duration.min(MAX_TARGET_DURATION_SECS);

            // fMP4 (EXT-X-MAP): fetch the init segment before the first media
            // segment, since the decoder can't start without it
            let map_url = playlist
                .segments
                .iter()
                .find_map(|s| s.map.as_ref())
                .and_then(|map| playlist_url.join(map.uri.trim()).ok())
                .map(String::from);
            if !sent_first {
                if let Some(map_url) = &map_url {
                    if init_segment.as_ref().map(|(url, _)| url) != Some(map_url) {
                        match http_get(&client, map_url) {
                            Ok((_, data)) => init_segment = Some((map_url.clone(), data)),
                            Err(reason) => {
                                let reason = format!("HLS init segment {map_url}: {reason}");
                                self.report(reason.clone());
                                last_problem = Some(reason);
                                consecutive_failures += 1;
                                if !backoff_sleep(consecutive_failures, &self.stop_flag) {
                                    return Ok(());
                                }
                                continue;
                            }
                        }
                    }
                }
            }

            // Segments with their absolute URLs
            let segments: Vec<_> = playlist
                .segments
                .iter()
                .filter(|s| is_valid_segment_uri(&s.uri))
                .filter_map(|s| {
                    let url = playlist_url.join(s.uri.trim()).ok()?;
                    Some((String::from(url), s))
                })
                .collect();

            // On first fetch, set starting position.
            // For live: start a few segments back from the end to fill the pipeline.
            // Starting at only the last segment would mean 1 segment in the pipeline,
            // and new segments only arrive each playlist refresh — buffer starves.
            // Starting SEGMENT_BUFFER_SIZE back fills the bounded channel immediately.
            // For VOD: start from the beginning.
            let start_idx = if first_fetch && is_live {
                segments.len().saturating_sub(SEGMENT_BUFFER_SIZE)
            } else {
                0
            };
            // Mark skipped segments as "already seen" to prevent out-of-order
            // delivery on subsequent fetches when start_idx returns to 0
            for (url, _) in segments.iter().take(start_idx) {
                downloaded_urls.insert(url.clone());
            }
            // An empty live playlist (encoder just started) doesn't count:
            // the next fetch must still start near the live edge rather
            // than at the oldest segment of a long window
            if !segments.is_empty() {
                first_fetch = false;
            }

            let mut fetched_new = false;
            // Why the segments of this pass gave no audio
            let mut last_failure: Option<String> = None;

            // Process segments, skipping already-downloaded URLs
            for (segment_url, segment) in segments.iter().skip(start_idx) {
                if self.stopped() {
                    return Ok(());
                }

                // URL-based dedup: skip segments we've already downloaded.
                // Failed segments count as downloaded too: the content is
                // time-sensitive and retrying would cause a glitch anyway.
                if !downloaded_urls.insert(segment_url.clone()) {
                    continue;
                }
                fetched_new = true;

                let data = match http_get(&client, segment_url) {
                    Ok((_, data)) => data,
                    Err(reason) => {
                        last_failure = Some(format!("HLS segment {segment_url}: {reason}"));
                        continue;
                    }
                };
                self.bytes_received
                    .fetch_add(data.len() as u64, Ordering::Relaxed);

                let (mut audio_data, segment_meta) = split_segment(&data, map_url.is_some());
                if audio_data.is_empty() {
                    last_failure = Some(format!(
                        "HLS segment {segment_url}: no audio found in {} bytes ({:?})",
                        data.len(),
                        detect_segment_format(&data, segment_url)
                    ));
                    continue;
                }
                if !sent_first {
                    // fMP4: prepend the init segment to the first segment
                    if let Some((_, mut init)) = init_segment.take() {
                        init.append(&mut audio_data);
                        audio_data = init;
                    }
                }

                if self.stopped() {
                    return Ok(());
                }

                // Song info applies from the start of this segment
                let segment_start = bytes_sent;
                bytes_sent += audio_data.len() as u64;
                if self.sender.send(audio_data).is_err() {
                    return Ok(());
                }
                self.segments_downloaded.fetch_add(1, Ordering::Relaxed);
                sent_first = true;
                last_audio = Instant::now();
                last_problem = None;

                // Song info: playlist attributes first, since ID3
                // from the segment itself takes priority over them
                if let Some(meta) = segment_titles
                    .get(segment.uri.trim())
                    .and_then(|t| parse_extinf_title(t))
                {
                    self.metadata_sink.offer_at(meta, segment_start);
                }
                for meta in segment_meta {
                    self.metadata_sink.offer_at(meta, segment_start);
                }
            }

            if let Some(reason) = last_failure {
                if !sent_first {
                    // Every segment tried so far failed: this won't play
                    return Err(reason);
                }
                self.report(reason.clone());
                last_problem = Some(reason);
            } else if !sent_first && segments.is_empty() && !playlist.segments.is_empty() {
                self.report("HLS playlist has no usable segment URIs".to_string());
            }

            // Bound the HashSet: keep only URLs still in the current playlist.
            // Old URLs that scrolled off the live window are removed.
            let current_urls: HashSet<&str> = segments.iter().map(|(u, _)| u.as_str()).collect();
            downloaded_urls.retain(|url| current_urls.contains(url.as_str()));

            // A live playlist that stops adding segments, or whose segments
            // keep failing, is given up on. Before the first segment
            // `HlsReader::new` decides.
            if sent_first && last_audio.elapsed() >= self.give_up {
                let problem = last_problem
                    .as_deref()
                    .unwrap_or("the playlist stopped adding segments");
                return Err(gave_up(self.give_up, problem));
            }

            // VOD: done after all segments downloaded
            if !is_live
                && segments
                    .iter()
                    .all(|(url, _)| downloaded_urls.contains(url))
            {
                return Ok(());
            }

            // RFC 8216 Section 6.3.4 — playlist reload timing:
            // - After playlist changed (new segments found): wait target_duration
            // - After playlist unchanged (no new segments): wait target_duration / 2
            let wait = if is_live {
                let base = target_duration.max(2);
                if fetched_new {
                    Duration::from_secs(base)
                } else {
                    Duration::from_secs(base / 2)
                }
            } else {
                Duration::from_secs(1)
            };
            if !self.sleep(wait) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::timeouts::{MAX_BACKOFF_SECS, RETRY_BASE_DELAY_SECS};
    use crate::stream::backoff_delay;

    // --- backoff_delay ---

    #[test]
    fn backoff_first_failure_is_base_delay() {
        assert_eq!(backoff_delay(1), Duration::from_secs(RETRY_BASE_DELAY_SECS));
    }

    #[test]
    fn backoff_exponential_growth() {
        // base=2: 2, 4, 8, 10(capped), 10, ...
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(4), Duration::from_secs(MAX_BACKOFF_SECS));
    }

    #[test]
    fn backoff_caps_at_max() {
        // 2^5 * 2 = 64, but capped at MAX_BACKOFF_SECS (10)
        assert_eq!(backoff_delay(6), Duration::from_secs(MAX_BACKOFF_SECS));
        assert_eq!(backoff_delay(10), Duration::from_secs(MAX_BACKOFF_SECS));
        assert_eq!(backoff_delay(100), Duration::from_secs(MAX_BACKOFF_SECS));
    }

    #[test]
    fn backoff_zero_failures_is_base() {
        // Edge case: 0 failures (shouldn't happen, but be safe)
        assert_eq!(backoff_delay(0), Duration::from_secs(RETRY_BASE_DELAY_SECS));
    }

    // --- detect_segment_format ---

    #[test]
    fn detect_ts_by_sync_byte() {
        let data = vec![0x47, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            detect_segment_format(&data, "unknown"),
            HlsSegmentFormat::MpegTs
        );
    }

    #[test]
    fn detect_fmp4_by_ftyp() {
        let mut data = vec![0x00, 0x00, 0x00, 0x20]; // size
        data.extend_from_slice(b"ftyp");
        assert_eq!(
            detect_segment_format(&data, "unknown"),
            HlsSegmentFormat::Fmp4
        );
    }

    #[test]
    fn detect_fmp4_by_moof() {
        let mut data = vec![0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(b"moof");
        assert_eq!(
            detect_segment_format(&data, "unknown"),
            HlsSegmentFormat::Fmp4
        );
    }

    #[test]
    fn detect_fmp4_by_moov() {
        let mut data = vec![0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(b"moov");
        assert_eq!(
            detect_segment_format(&data, "unknown"),
            HlsSegmentFormat::Fmp4
        );
    }

    #[test]
    fn detect_ts_by_url_extension() {
        let data = vec![0xFF, 0xFB]; // not a TS sync byte
        assert_eq!(
            detect_segment_format(&data, "http://example.com/seg001.ts"),
            HlsSegmentFormat::MpegTs
        );
    }

    #[test]
    fn detect_ts_by_url_with_query() {
        let data = vec![0xFF];
        assert_eq!(
            detect_segment_format(&data, "http://example.com/seg.ts?token=abc"),
            HlsSegmentFormat::MpegTs
        );
    }

    #[test]
    fn detect_m4s_by_url() {
        let data = vec![0xFF];
        assert_eq!(
            detect_segment_format(&data, "http://example.com/seg001.m4s"),
            HlsSegmentFormat::Fmp4
        );
    }

    #[test]
    fn detect_raw_unknown() {
        let data = vec![0xFF, 0xFB, 0x90, 0x00]; // MP3 frame header
        assert_eq!(
            detect_segment_format(&data, "http://example.com/audio"),
            HlsSegmentFormat::Raw
        );
    }

    #[test]
    fn detect_empty_data() {
        assert_eq!(
            detect_segment_format(&[], "http://example.com/seg.aac"),
            HlsSegmentFormat::Raw
        );
    }

    #[test]
    fn detect_short_data() {
        let data = vec![0x47]; // TS sync byte but only 1 byte
        assert_eq!(
            detect_segment_format(&data, "unknown"),
            HlsSegmentFormat::MpegTs
        );
    }

    // --- is_valid_segment_uri ---

    #[test]
    fn valid_absolute_url() {
        assert!(is_valid_segment_uri("http://example.com/seg001.ts"));
    }

    #[test]
    fn valid_https_url() {
        assert!(is_valid_segment_uri("https://cdn.example.com/seg.aac"));
    }

    #[test]
    fn valid_relative_ts() {
        assert!(is_valid_segment_uri("segment001.ts"));
    }

    #[test]
    fn valid_relative_m4s() {
        assert!(is_valid_segment_uri("chunk_001.m4s"));
    }

    #[test]
    fn valid_relative_with_path() {
        assert!(is_valid_segment_uri("media/segment001.aac"));
    }

    #[test]
    fn invalid_empty_uri() {
        assert!(!is_valid_segment_uri(""));
    }

    #[test]
    fn invalid_whitespace() {
        assert!(!is_valid_segment_uri("   "));
    }

    #[test]
    fn invalid_metadata_line() {
        assert!(!is_valid_segment_uri("METHOD=\"AES-128\""));
    }

    #[test]
    fn invalid_metadata_single_quotes() {
        assert!(!is_valid_segment_uri("KEY='value'"));
    }

    #[test]
    fn valid_bare_name_no_extension() {
        // RFC 8216: any non-tag line is a URI; servers use extensionless names
        assert!(is_valid_segment_uri("somename"));
    }

    // --- demux_ts_segment ---

    #[test]
    fn demux_empty_data() {
        let result = demux_ts_segment(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn demux_invalid_data() {
        let result = demux_ts_segment(&[0xFF; 100]);
        assert!(result.is_empty());
    }

    // --- HlsSegmentFormat ---

    #[test]
    fn segment_format_equality() {
        assert_eq!(HlsSegmentFormat::MpegTs, HlsSegmentFormat::MpegTs);
        assert_eq!(HlsSegmentFormat::Fmp4, HlsSegmentFormat::Fmp4);
        assert_eq!(HlsSegmentFormat::Raw, HlsSegmentFormat::Raw);
        assert_ne!(HlsSegmentFormat::MpegTs, HlsSegmentFormat::Fmp4);
    }

    #[test]
    fn segment_format_debug() {
        assert_eq!(format!("{:?}", HlsSegmentFormat::MpegTs), "MpegTs");
        assert_eq!(format!("{:?}", HlsSegmentFormat::Fmp4), "Fmp4");
        assert_eq!(format!("{:?}", HlsSegmentFormat::Raw), "Raw");
    }

    // --- HlsReader from test channel ---

    #[test]
    fn hls_reader_read_initial_data() {
        let (_tx, rx) = bounded(4);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![10, 20, 30]);

        let mut buf = [0u8; 3];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf, [10, 20, 30]);
    }

    #[test]
    fn hls_reader_read_from_channel() {
        let (tx, rx) = bounded(4);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![1, 2]);

        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();

        tx.send(vec![3, 4, 5]).unwrap();
        let mut buf2 = [0u8; 3];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf2, [3, 4, 5]);
    }

    #[test]
    fn hls_reader_seek() {
        let (_tx, rx) = bounded(4);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![1, 2, 3, 4, 5]);

        // Read all
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).unwrap();

        // Seek back
        let pos = reader.seek(SeekFrom::Start(2)).unwrap();
        assert_eq!(pos, 2);

        let mut buf2 = [0u8; 3];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf2, [3, 4, 5]);
    }

    #[test]
    fn read_returns_interrupted_while_waiting_for_a_segment() {
        let (tx, rx) = bounded::<Vec<u8>>(8);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, Vec::new());
        let start = std::time::Instant::now();
        let mut buf = [0u8; 16];
        let err = reader.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(2));

        tx.send(vec![7, 8]).unwrap();
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], &[7, 8]);
    }

    #[test]
    fn hls_reader_drop_sets_stop_flag() {
        let (_tx, rx) = bounded(4);
        let stop_flag;
        {
            let (reader, stop) = HlsReader::from_test_channel(rx, vec![1, 2, 3]);
            stop_flag = stop.clone();
            assert!(!stop_flag.load(Ordering::SeqCst));
            drop(reader);
        }
        assert!(stop_flag.load(Ordering::SeqCst));
    }

    // --- detect_segment_format edge cases ---

    #[test]
    fn detect_data_priority_over_url() {
        // Data says TS (0x47 sync byte) even though URL says .m4s
        let data = vec![0x47, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            detect_segment_format(&data, "http://example.com/seg.m4s"),
            HlsSegmentFormat::MpegTs
        );
    }

    #[test]
    fn detect_fmp4_priority_over_url_ts() {
        // Data says fMP4 even though URL says .ts
        let mut data = vec![0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(b"ftyp");
        assert_eq!(
            detect_segment_format(&data, "http://example.com/seg.ts"),
            HlsSegmentFormat::Fmp4
        );
    }

    #[test]
    fn detect_exactly_7_bytes_no_fmp4_check() {
        // Less than 8 bytes → can't check fMP4 magic, falls through
        let data = vec![0x00, 0x00, 0x00, 0x08, b'f', b't', b'y'];
        assert_eq!(
            detect_segment_format(&data, "http://example.com/audio"),
            HlsSegmentFormat::Raw
        );
    }

    #[test]
    fn detect_adts_header_as_raw() {
        // ADTS frame header (0xFFF) is not TS or fMP4
        let data = vec![0xFF, 0xF1, 0x50, 0x80, 0x02, 0x00, 0x00, 0x00];
        assert_eq!(
            detect_segment_format(&data, "http://example.com/audio"),
            HlsSegmentFormat::Raw
        );
    }

    #[test]
    fn detect_id3_header_as_raw() {
        // ID3 header (0x49 0x44 0x33) is not TS or fMP4
        let mut data = b"ID3".to_vec();
        data.extend_from_slice(&[0x04, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            detect_segment_format(&data, "http://example.com/audio"),
            HlsSegmentFormat::Raw
        );
    }

    #[test]
    fn detect_m4s_url_with_query() {
        let data = vec![0xFF]; // not fMP4 by data
        assert_eq!(
            detect_segment_format(&data, "http://example.com/seg.m4s?token=xyz"),
            HlsSegmentFormat::Fmp4
        );
    }

    #[test]
    fn detect_case_insensitive_url() {
        let data = vec![0xFF];
        assert_eq!(
            detect_segment_format(&data, "http://example.com/seg001.TS"),
            HlsSegmentFormat::MpegTs
        );
    }

    // --- is_valid_segment_uri edge cases ---

    #[test]
    fn valid_relative_mp4() {
        assert!(is_valid_segment_uri("media/chunk.mp4"));
    }

    #[test]
    fn valid_relative_m4a() {
        assert!(is_valid_segment_uri("audio.m4a"));
    }

    #[test]
    fn valid_aac_with_query() {
        assert!(is_valid_segment_uri("seg.aac?start=0"));
    }

    #[test]
    fn valid_ts_with_query() {
        assert!(is_valid_segment_uri("seg.ts?token=abc"));
    }

    #[test]
    fn valid_deep_relative_path() {
        assert!(is_valid_segment_uri("a/b/c/segment.ts"));
    }

    #[test]
    fn invalid_key_value_with_uri() {
        assert!(!is_valid_segment_uri("URI=\"init.mp4\""));
    }

    #[test]
    fn invalid_iv_metadata() {
        assert!(!is_valid_segment_uri("IV='0x12345678'"));
    }

    #[test]
    fn valid_absolute_url_no_extension() {
        // Absolute URLs are always valid regardless of extension
        assert!(is_valid_segment_uri("http://cdn.example.com/audio/raw"));
    }

    #[test]
    fn valid_single_word() {
        assert!(is_valid_segment_uri("BANDWIDTH"));
    }

    #[test]
    fn valid_number() {
        assert!(is_valid_segment_uri("12345"));
    }

    // --- demux_ts_segment edge cases ---

    #[test]
    fn demux_single_ts_sync_byte() {
        // Just the sync byte, not enough for a packet
        let result = demux_ts_segment(&[0x47]);
        assert!(result.is_empty());
    }

    #[test]
    fn demux_short_ts_data() {
        // Less than one TS packet (188 bytes)
        let mut data = vec![0x47]; // sync byte
        data.extend_from_slice(&[0u8; 100]); // not a full packet
        let result = demux_ts_segment(&data);
        assert!(result.is_empty());
    }

    #[test]
    fn demux_null_packets() {
        // 188-byte TS null packet (PID 0x1FFF)
        let mut packet = vec![0x47, 0x1F, 0xFF, 0x10]; // sync, PID=0x1FFF, no adaptation
        packet.resize(188, 0xFF); // padding

        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend_from_slice(&packet);
        }
        let result = demux_ts_segment(&data);
        // Null packets have no audio data
        assert!(result.is_empty());
    }

    // --- HlsSegmentFormat edge cases ---

    #[test]
    fn segment_format_clone() {
        let f = HlsSegmentFormat::MpegTs;
        let cloned = f;
        assert_eq!(f, cloned);
    }

    #[test]
    fn segment_format_all_ne_combinations() {
        assert_ne!(HlsSegmentFormat::MpegTs, HlsSegmentFormat::Raw);
        assert_ne!(HlsSegmentFormat::Fmp4, HlsSegmentFormat::Raw);
        assert_ne!(HlsSegmentFormat::Fmp4, HlsSegmentFormat::MpegTs);
    }

    // --- HlsReader edge cases ---

    #[test]
    fn hls_reader_partial_read() {
        let (_tx, rx) = bounded(4);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![1, 2, 3, 4, 5]);

        let mut buf = [0u8; 2];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, [1, 2]);

        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, [3, 4]);

        let mut buf1 = [0u8; 1];
        let n = reader.read(&mut buf1).unwrap();
        assert_eq!(n, 1);
        assert_eq!(buf1, [5]);
    }

    #[test]
    fn hls_reader_seek_current() {
        let (_tx, rx) = bounded(4);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![10, 20, 30, 40, 50]);

        // Seek forward 3 from start
        let pos = reader.seek(SeekFrom::Current(3)).unwrap();
        assert_eq!(pos, 3);

        let mut buf = [0u8; 2];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, [40, 50]);
    }

    #[test]
    fn hls_reader_seek_end() {
        let (_tx, rx) = bounded(4);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![1, 2, 3, 4, 5]);

        let pos = reader.seek(SeekFrom::End(-2)).unwrap();
        assert_eq!(pos, 3);

        let mut buf = [0u8; 2];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, [4, 5]);
    }

    #[test]
    fn hls_reader_zero_length_read() {
        let (_tx, rx) = bounded(4);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![1, 2, 3]);

        let mut buf = [0u8; 0];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn hls_reader_multiple_segments_from_channel() {
        let (tx, rx) = bounded(8);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![1, 2]);

        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2]);

        // Send multiple segments
        tx.send(vec![3, 4]).unwrap();
        tx.send(vec![5, 6]).unwrap();

        // Read first segment
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [3, 4]);

        // Read second segment
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [5, 6]);
    }

    #[test]
    fn hls_reader_drop_with_pending_data() {
        let (tx, rx) = bounded(4);
        let stop_flag;
        {
            let (reader, stop) = HlsReader::from_test_channel(rx, vec![1, 2, 3]);
            stop_flag = stop.clone();
            tx.send(vec![4, 5, 6]).unwrap();
            drop(reader);
        }
        assert!(stop_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn hls_reader_detected_format_ts_data() {
        let (_tx, rx) = bounded(4);
        // Initial data with TS sync byte
        let (reader, _stop) = HlsReader::from_test_channel(rx, vec![0x47, 0x00, 0x00, 0x00]);
        assert_eq!(reader.detected_format, HlsSegmentFormat::MpegTs);
    }

    #[test]
    fn hls_reader_detected_format_fmp4_data() {
        let (_tx, rx) = bounded(4);
        let mut data = vec![0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(b"ftyp");
        let (reader, _stop) = HlsReader::from_test_channel(rx, data);
        assert_eq!(reader.detected_format, HlsSegmentFormat::Fmp4);
    }

    #[test]
    fn hls_reader_detected_format_raw_data() {
        let (_tx, rx) = bounded(4);
        // from_test_channel passes url "test.ts" which has .ts extension → MpegTs
        // Use data that won't match TS sync byte or fMP4
        let (reader, _stop) = HlsReader::from_test_channel(rx, vec![0xFF, 0xFB, 0x90]);
        // "test.ts" URL extension makes it detect as MpegTs via URL fallback
        assert_eq!(reader.detected_format, HlsSegmentFormat::MpegTs);
    }

    #[test]
    fn hls_reader_channel_disconnect_after_read() {
        let (tx, rx) = bounded(4);
        let (mut reader, _stop) = HlsReader::from_test_channel(rx, vec![1, 2]);

        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2]);

        // Send one more and disconnect
        tx.send(vec![3, 4]).unwrap();
        drop(tx);

        // Should still read pending data
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [3, 4]);
    }

    // --- ID3 song info in segments ---

    mod segment_metadata {
        use super::*;
        use crate::stream::id3::test_util::{frame, id3v2_song, id3v2_tag};
        use crate::stream::metadata::MetadataSource;
        use mpeg2ts::es::{StreamId, StreamType};
        use mpeg2ts::pes::PesHeader;
        use mpeg2ts::time::Timestamp;
        use mpeg2ts::ts::payload::{Bytes, Pat, Pes, Pmt};
        use mpeg2ts::ts::{
            ContinuityCounter, EsInfo, Pid, ProgramAssociation, TransportScramblingControl,
            TsHeader, TsPacket, TsPacketWriter, TsPayload, VersionNumber, WriteTsPacket,
        };

        const PMT_PID: u16 = 0x1000;
        const AUDIO_PID: u16 = 0x101;
        const META_PID: u16 = 0x102;

        fn header(pid: u16) -> TsHeader {
            TsHeader {
                transport_error_indicator: false,
                transport_priority: false,
                pid: Pid::new(pid).unwrap(),
                transport_scrambling_control: TransportScramblingControl::NotScrambled,
                continuity_counter: ContinuityCounter::new(),
            }
        }

        fn packet(pid: u16, payload: TsPayload) -> TsPacket {
            TsPacket {
                header: header(pid),
                adaptation_field: None,
                payload: Some(payload),
            }
        }

        /// PES packets carrying `data`, split to fit TS packets
        fn pes_packets(pid: u16, stream_id: u8, data: &[u8]) -> Vec<TsPacket> {
            const FIRST: usize = 160;
            let first = &data[..data.len().min(FIRST)];
            let mut packets = vec![packet(
                pid,
                TsPayload::PesStart(Pes {
                    header: PesHeader {
                        stream_id: StreamId::new(stream_id),
                        priority: false,
                        data_alignment_indicator: true,
                        copyright: false,
                        original_or_copy: false,
                        pts: Some(Timestamp::new(90_000).unwrap()),
                        dts: None,
                        escr: None,
                    },
                    pes_packet_len: 0,
                    data: Bytes::new(first).unwrap(),
                }),
            )];
            for chunk in data[first.len()..].chunks(Bytes::MAX_SIZE) {
                packets.push(packet(
                    pid,
                    TsPayload::PesContinuation(Bytes::new(chunk).unwrap()),
                ));
            }
            packets
        }

        /// A TS segment with an AAC audio stream and an ID3 metadata stream
        fn ts_segment(audio: &[u8], tags: &[Vec<u8>]) -> Vec<u8> {
            let mut packets = vec![
                packet(
                    0,
                    TsPayload::Pat(Pat {
                        transport_stream_id: 1,
                        version_number: VersionNumber::new(),
                        table: vec![ProgramAssociation {
                            program_num: 1,
                            program_map_pid: Pid::new(PMT_PID).unwrap(),
                        }],
                    }),
                ),
                packet(
                    PMT_PID,
                    TsPayload::Pmt(Pmt {
                        program_num: 1,
                        pcr_pid: None,
                        version_number: VersionNumber::new(),
                        program_info: vec![],
                        es_info: vec![
                            EsInfo {
                                stream_type: StreamType::AdtsAac,
                                elementary_pid: Pid::new(AUDIO_PID).unwrap(),
                                descriptors: vec![],
                            },
                            EsInfo {
                                stream_type: StreamType::PacketizedMetadata,
                                elementary_pid: Pid::new(META_PID).unwrap(),
                                descriptors: vec![],
                            },
                        ],
                    }),
                ),
            ];
            for tag in tags {
                packets.extend(pes_packets(META_PID, 0xBD, tag));
            }
            packets.extend(pes_packets(AUDIO_PID, 0xC0, audio));

            let mut writer = TsPacketWriter::new(Vec::new());
            for p in &packets {
                writer.write_ts_packet(p).unwrap();
            }
            writer.into_stream()
        }

        fn titles(meta: &[StreamMetadata]) -> Vec<&str> {
            meta.iter().filter_map(|m| m.title.as_deref()).collect()
        }

        #[test]
        fn ts_segment_yields_audio_and_id3() {
            let audio = frame(400);
            let seg = ts_segment(&audio, &[id3v2_song("Artist", "Song")]);
            let (out, meta) = split_segment(&seg, false);
            assert_eq!(out, audio);
            assert_eq!(titles(&meta), vec!["Song"]);
            assert_eq!(meta[0].artist.as_deref(), Some("Artist"));
            assert_eq!(meta[0].source, MetadataSource::Id3v2);
        }

        #[test]
        fn ts_segment_with_large_tag_across_packets() {
            let long_title = "x".repeat(600);
            let seg = ts_segment(&frame(200), &[id3v2_song("Artist", &long_title)]);
            let (_, meta) = split_segment(&seg, false);
            assert_eq!(titles(&meta), vec![long_title.as_str()]);
        }

        #[test]
        fn ts_segment_with_several_tags() {
            let seg = ts_segment(
                &frame(200),
                &[id3v2_song("A", "One"), id3v2_song("B", "Two")],
            );
            let (_, meta) = split_segment(&seg, false);
            assert_eq!(titles(&meta), vec!["One", "Two"]);
        }

        #[test]
        fn ts_segment_without_metadata_stream_is_unchanged() {
            let audio = frame(300);
            let seg = ts_segment(&audio, &[]);
            let (out, meta) = split_segment(&seg, false);
            assert_eq!(out, audio);
            assert_eq!(out, demux_ts_segment(&seg));
            assert!(meta.is_empty());
        }

        #[test]
        fn packed_audio_segment_strips_leading_tags() {
            // Timestamp-style tag with no song info, then a song tag, then ADTS audio
            let audio = frame(500);
            let seg = [
                id3v2_tag(&[(b"TXXX", "\0cue")]),
                id3v2_song("Artist", "Song"),
                audio.clone(),
            ]
            .concat();
            let (out, meta) = split_segment(&seg, false);
            assert_eq!(out, audio);
            assert_eq!(titles(&meta), vec!["Song"]);
        }

        #[test]
        fn packed_audio_without_tags_is_unchanged() {
            let audio = frame(500);
            let (out, meta) = split_segment(&audio, false);
            assert_eq!(out, audio);
            assert!(meta.is_empty());
        }

        #[test]
        fn fmp4_segment_reads_emsg_and_keeps_bytes() {
            let tag = id3v2_song("Artist", "Song");
            let mut body = vec![1, 0, 0, 0];
            body.extend_from_slice(&[0u8; 20]);
            body.extend_from_slice(b"https://aomedia.org/emsg/ID3\0\0");
            body.extend_from_slice(&tag);
            let mut seg = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            seg.extend_from_slice(b"emsg");
            seg.extend(body);
            seg.extend_from_slice(&[0, 0, 0, 8, b'm', b'o', b'o', b'f']);
            let (out, meta) = split_segment(&seg, true);
            assert_eq!(out, seg);
            assert_eq!(titles(&meta), vec!["Song"]);
            // Detected from the data even when the URL is not .m4s
            let (out, meta) = split_segment(&seg, false);
            assert_eq!(out, seg);
            assert_eq!(titles(&meta), vec!["Song"]);
        }

        #[test]
        fn hls_reader_sends_song_info() {
            use crate::stream::test_server::{Route, TestServer};

            let server = TestServer::start();
            let playlist = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n\
                #EXT-X-MEDIA-SEQUENCE:1\n\
                #EXTINF:2.0,title=\"From Playlist\",artist=\"Pl\"\nseg1.ts\n\
                #EXTINF:2.0,\nseg2.ts\n#EXT-X-ENDLIST\n";
            server.route("/live.m3u8", Route::new(playlist));
            server.route("/seg1.ts", Route::new(ts_segment(&frame(300), &[])));
            server.route(
                "/seg2.ts",
                Route::new(ts_segment(&frame(300), &[id3v2_song("Artist", "From ID3")])),
            );

            let (_reader, rx) = HlsReader::new(&server.url("/live.m3u8"), None).unwrap();
            let got: Vec<_> = (0..2)
                .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
                .collect();
            assert_eq!(got[0].title.as_deref(), Some("From Playlist"));
            assert_eq!(got[0].source, MetadataSource::HlsPlaylist);
            assert_eq!(got[1].title.as_deref(), Some("From ID3"));
            assert_eq!(got[1].source, MetadataSource::Id3v2);
        }

        #[test]
        fn hls_song_info_waits_for_playback() {
            use crate::stream::test_server::{Route, TestServer};
            use std::sync::atomic::AtomicU64;

            let server = TestServer::start();
            let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n\
                #EXTINF:2.0,\nseg1.ts\n#EXTINF:2.0,\nseg2.ts\n#EXT-X-ENDLIST\n";
            let seg1_audio = frame(300);
            server.route("/live.m3u8", Route::new(playlist));
            server.route(
                "/seg1.ts",
                Route::new(ts_segment(&seg1_audio, &[id3v2_song("A", "First")])),
            );
            server.route(
                "/seg2.ts",
                Route::new(ts_segment(&frame(300), &[id3v2_song("B", "Second")])),
            );

            let position = Arc::new(AtomicU64::new(0));
            let (_reader, rx) =
                HlsReader::new(&server.url("/live.m3u8"), Some(position.clone())).unwrap();
            let title = |ms| {
                rx.recv_timeout(Duration::from_millis(ms))
                    .ok()
                    .and_then(|m| m.title)
            };
            // Both segments are downloaded, but only the first is playing
            assert_eq!(title(2000), Some("First".to_string()));
            assert_eq!(title(500), None);
            // Playback reaches the second segment
            position.store(seg1_audio.len() as u64, Ordering::Relaxed);
            assert_eq!(title(2000), Some("Second".to_string()));
        }
    }

    // --- Segment downloader against real-world stream shapes ---
    //
    // Each of these used to end in "Stream resolution timed out" (no segment
    // ever reached the reader) or in a silent stall.

    mod downloader {
        use super::*;
        use crate::stream::id3::test_util::{frame, id3v2_song};
        use crate::stream::test_server::{Route, TestServer};
        use std::time::Instant;

        /// MPEG-2 CRC32 for PSI sections
        fn crc32_mpeg(data: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for &byte in data {
                crc ^= u32::from(byte) << 24;
                for _ in 0..8 {
                    crc = if crc & 0x8000_0000 != 0 {
                        (crc << 1) ^ 0x04C1_1DB7
                    } else {
                        crc << 1
                    };
                }
            }
            crc
        }

        /// One TS packet; short payloads are padded with adaptation-field
        /// stuffing, as real muxers do
        fn ts_packet(pid: u16, unit_start: bool, payload: &[u8]) -> Vec<u8> {
            assert!(payload.len() <= 184);
            let mut p = vec![
                0x47,
                (u8::from(unit_start) << 6) | (pid >> 8) as u8,
                pid as u8,
            ];
            if payload.len() == 184 {
                p.push(0x10);
            } else {
                let af_len = 183 - payload.len();
                p.push(0x30);
                p.push(af_len as u8);
                if af_len > 0 {
                    p.push(0x00);
                    p.resize(p.len() + af_len - 1, 0xFF);
                }
            }
            p.extend_from_slice(payload);
            assert_eq!(p.len(), 188);
            p
        }

        /// A PSI section in a TS packet payload (pointer field included)
        fn psi(table_id: u8, body: &[u8]) -> Vec<u8> {
            let len = body.len() + 4;
            let mut section = vec![table_id, 0xB0 | (len >> 8) as u8, len as u8];
            section.extend_from_slice(body);
            let crc = crc32_mpeg(&section);
            section.extend_from_slice(&crc.to_be_bytes());
            [vec![0], section].concat()
        }

        fn pat(pmt_pid: u16) -> Vec<u8> {
            let body = [
                0x00,
                0x01,
                0xC1,
                0x00,
                0x00,
                0x00,
                0x01,
                0xE0 | (pmt_pid >> 8) as u8,
                pmt_pid as u8,
            ];
            ts_packet(0, true, &psi(0x00, &body))
        }

        /// A PMT listing `(stream_type, pid)` streams
        fn pmt(pmt_pid: u16, streams: &[(u8, u16)]) -> Vec<u8> {
            let pcr = streams.first().map_or(0x1FFF, |s| s.1);
            let mut body = vec![
                0x00,
                0x01,
                0xC1,
                0x00,
                0x00,
                0xE0 | (pcr >> 8) as u8,
                pcr as u8,
                0xF0,
                0x00,
            ];
            for &(stream_type, pid) in streams {
                body.extend_from_slice(&[
                    stream_type,
                    0xE0 | (pid >> 8) as u8,
                    pid as u8,
                    0xF0,
                    0x00,
                ]);
            }
            ts_packet(pmt_pid, true, &psi(0x02, &body))
        }

        /// PES packets (with a PTS) carrying `data` on `pid`
        fn pes(pid: u16, stream_id: u8, data: &[u8]) -> Vec<u8> {
            let mut pes = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
            pes.extend_from_slice(data);
            let mut out = Vec::new();
            for (i, chunk) in pes.chunks(184).enumerate() {
                out.extend(ts_packet(pid, i == 0, chunk));
            }
            out
        }

        /// A TS segment with one AAC stream
        fn plain_ts(audio: &[u8]) -> Vec<u8> {
            [
                pat(0x1000),
                pmt(0x1000, &[(0x0F, 0x100)]),
                pes(0x100, 0xC0, audio),
            ]
            .concat()
        }

        /// A TS segment whose only audio (AAC, PID 0x100) sits next to a
        /// stream type the old demuxer rejected (MPEG-H 0x2D), which made it
        /// drop the whole segment
        fn ts_segment_unusual(audio: &[u8]) -> Vec<u8> {
            [
                pat(0x1000),
                pmt(0x1000, &[(0x2D, 0x102), (0x0F, 0x100)]),
                pes(0x100, 0xC0, audio),
            ]
            .concat()
        }

        /// Read everything the reader has buffered so far
        fn first_chunk(reader: &mut HlsReader) -> Vec<u8> {
            let mut buf = vec![0u8; 64 * 1024];
            let n = reader.read(&mut buf).unwrap();
            buf.truncate(n);
            buf
        }

        fn vod(segments: &[&str]) -> String {
            let mut pl = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n");
            for s in segments {
                pl.push_str(&format!("#EXTINF:4.0,\n{s}\n"));
            }
            pl.push_str("#EXT-X-ENDLIST\n");
            pl
        }

        #[test]
        fn plays_extensionless_and_query_segment_names() {
            for name in [
                "chunk_1727384723",
                "seg-1.mp4?session=abc",
                "seg.m4a?t=1",
                "seg.mp3",
            ] {
                let server = TestServer::start();
                let audio = frame(300);
                let path = format!("/live/{name}");
                server.route("/live/index.m3u8", Route::new(vod(&[name])));
                server.route(&path, Route::new(plain_ts(&audio)));
                let (mut reader, _) = HlsReader::new(&server.url("/live/index.m3u8"), None)
                    .unwrap_or_else(|e| {
                        panic!("{name}: {e}");
                    });
                assert_eq!(first_chunk(&mut reader), audio, "{name}");
            }
        }

        #[test]
        fn resolves_root_relative_and_dot_segment_uris() {
            for (uri, served_at) in [
                ("/cdn/audio/seg1.ts", "/cdn/audio/seg1.ts"),
                ("../other/seg1.ts", "/radio/other/seg1.ts"),
            ] {
                let server = TestServer::start();
                let audio = frame(300);
                server.route("/radio/live/index.m3u8", Route::new(vod(&[uri])));
                server.route(served_at, Route::new(plain_ts(&audio)));
                let (mut reader, _) =
                    HlsReader::new(&server.url("/radio/live/index.m3u8"), None).unwrap();
                assert_eq!(first_chunk(&mut reader), audio, "{uri}");
            }
        }

        #[test]
        fn resolves_segments_against_redirected_playlist() {
            let server = TestServer::start();
            let audio = frame(300);
            server.route("/live.m3u8", Route::redirect("/edge/42/live.m3u8"));
            server.route("/edge/42/live.m3u8", Route::new(vod(&["seg1.ts"])));
            server.route("/edge/42/seg1.ts", Route::new(plain_ts(&audio)));
            let (mut reader, _) = HlsReader::new(&server.url("/live.m3u8"), None).unwrap();
            assert_eq!(first_chunk(&mut reader), audio);
            assert_eq!(server.hits("/seg1.ts"), 0);
        }

        #[test]
        fn resolve_hls_url_follows_redirects_and_relative_variants() {
            let server = TestServer::start();
            let master = "#EXTM3U\n\
                #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1000,URI=\"iframes.m3u8\"\n\
                #EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.5\"\n\
                /cdn/audio-64000/index.m3u8\n";
            server.route("/radio.m3u8", Route::redirect("/geo/radio.m3u8"));
            server.route("/geo/radio.m3u8", Route::new(master));
            server.route("/cdn/audio-64000/index.m3u8", Route::new(vod(&["seg1.ts"])));
            assert_eq!(
                resolve_hls_url(&server.url("/radio.m3u8")).unwrap(),
                server.url("/cdn/audio-64000/index.m3u8")
            );
        }

        #[test]
        fn demuxes_audio_next_to_unknown_stream_types() {
            let audio = frame(700);
            assert_eq!(demux_ts_segment(&ts_segment_unusual(&audio)), audio);
        }

        #[test]
        fn demuxes_audio_without_pat_or_pmt() {
            let audio = frame(500);
            assert_eq!(demux_ts_segment(&pes(0x44, 0xC0, &audio)), audio);
        }

        #[test]
        fn demux_takes_only_the_first_audio_track() {
            let (main, second) = (frame(300), vec![0x11; 300]);
            let seg = [
                pat(0x1000),
                pmt(0x1000, &[(0x0F, 0x100), (0x0F, 0x101)]),
                pes(0x100, 0xC0, &main),
                pes(0x101, 0xC1, &second),
            ]
            .concat();
            assert_eq!(demux_ts_segment(&seg), main);
        }

        #[test]
        fn demux_resyncs_after_garbage() {
            let audio = frame(400);
            let seg = [vec![0x00, 0x12, 0x34], ts_segment_unusual(&audio)].concat();
            assert_eq!(demux_ts_segment(&seg), audio);
        }

        /// A TS segment laid out like RTL's (Quortex): the ID3 song info
        /// packet comes first, before the PAT and PMT. The old demuxer
        /// failed on that first packet ("Unknown PID") and returned no audio
        /// for every segment, so RTL never started.
        fn ts_segment_rtl_layout(audio: &[u8], tag: &[u8]) -> Vec<u8> {
            [
                pes(0x3E8, 0xBD, tag),
                pat(0x1000),
                pmt(0x1000, &[(0x0F, 0x100), (0x15, 0x3E8)]),
                pes(0x100, 0xC0, audio),
            ]
            .concat()
        }

        #[test]
        fn demuxes_rtl_layout_with_id3_before_pat() {
            let audio = frame(900);
            let seg = ts_segment_rtl_layout(&audio, &id3v2_song("Georges Lang", "W RTL Country"));
            let (out, meta) = split_segment(&seg, false);
            assert_eq!(out, audio);
            assert_eq!(meta.len(), 1);
            assert_eq!(meta[0].artist.as_deref(), Some("Georges Lang"));
            assert_eq!(meta[0].title.as_deref(), Some("W RTL Country"));
        }

        #[test]
        fn plays_rtl_quortex_stream_end_to_end() {
            let server = TestServer::start();
            // Master playlist from the redirector: one variant on another
            // path, with a signed query string
            let master = format!(
                "#EXTM3U\n#EXT-X-VERSION:7\n## RedirectorRewritten\n\
                 #EXT-X-SESSION-DATA:DATA-ID=\"fr.rtl.hls.cdn\",VALUE=\"qtx\"\n\
                 #EXT-X-INDEPENDENT-SEGMENTS\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=64000,FRAME-RATE=25,CODECS=\"mp4a.40.29\"\n\
                 {}\n",
                server.url("/radio/rtl/audio-64000/index.m3u8?Policy=abc&Signature=x~y")
            );
            // Media playlist as served by Quortex (live, trimmed to 3 segments)
            let media = "#EXTM3U\n#EXT-X-VERSION:7\n\
                ## Just In Time Delivered by Quortex Solution\n\
                #EXT-X-TARGETDURATION:6\n#EXT-X-DISCONTINUITY-SEQUENCE:0\n\
                #EXT-X-MEDIA-SEQUENCE:310843946\n\n\
                #EXT-X-PROGRAM-DATE-TIME:2026-09-26T22:18:48.960000Z\n\
                #EXTINF:5.760,\nsegment_310843946.ts\n#EXTINF:5.760,\nsegment_310843947.ts\n\
                #EXT-X-PROGRAM-DATE-TIME:2026-09-26T22:19:00.480000Z\n\
                #EXTINF:5.760,\nsegment_310843948.ts\n";
            server.route(
                "/webXYZ/grouprtl/audio-64000/index.m3u8",
                Route::new(master),
            );
            server.route(
                "/radio/rtl/audio-64000/index.m3u8?Policy=abc&Signature=x~y",
                Route::new(media),
            );
            let audio = frame(300);
            for n in 46..=48 {
                server.route(
                    &format!("/radio/rtl/audio-64000/segment_3108439{n}.ts"),
                    Route::new(ts_segment_rtl_layout(
                        &audio,
                        &id3v2_song("Georges Lang", "W RTL Country"),
                    )),
                );
            }

            let media_url =
                resolve_hls_url(&server.url("/webXYZ/grouprtl/audio-64000/index.m3u8")).unwrap();
            let (mut reader, rx) = HlsReader::new(&media_url, None).unwrap();
            assert_eq!(reader.detected_format, HlsSegmentFormat::Raw);
            assert_eq!(first_chunk(&mut reader)[..audio.len()], audio[..]);
            let meta = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(meta.title.as_deref(), Some("W RTL Country"));
        }

        #[test]
        fn plays_unusual_ts_segments() {
            let server = TestServer::start();
            let audio = frame(300);
            server.route("/index.m3u8", Route::new(vod(&["seg1.ts"])));
            server.route("/seg1.ts", Route::new(ts_segment_unusual(&audio)));
            let (mut reader, _) = HlsReader::new(&server.url("/index.m3u8"), None).unwrap();
            assert_eq!(first_chunk(&mut reader), audio);
        }

        #[test]
        fn fmp4_segments_of_any_name_get_the_init_segment() {
            let server = TestServer::start();
            let init = [&[0, 0, 0, 16][..], b"ftypiso6", &[0, 0, 0, 0]].concat();
            let media = [&[0, 0, 0, 8][..], b"moof", &[0x42; 32]].concat();
            let playlist = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n\
                #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg1.mp4\n#EXT-X-ENDLIST\n";
            server.route("/index.m3u8", Route::new(playlist));
            server.route("/init.mp4", Route::new(init.clone()));
            server.route("/seg1.mp4", Route::new(media.clone()));
            let (mut reader, _) = HlsReader::new(&server.url("/index.m3u8"), None).unwrap();
            assert_eq!(reader.detected_format, HlsSegmentFormat::Fmp4);
            assert_eq!(first_chunk(&mut reader), [init, media].concat());
        }

        #[test]
        fn init_segment_is_fetched_once_on_live_streams() {
            let server = TestServer::start();
            let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n\
                #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1.0,\nseg1.m4s\n";
            server.route("/index.m3u8", Route::new(playlist));
            server.route(
                "/init.mp4",
                Route::new([&[0, 0, 0, 8][..], b"ftyp"].concat()),
            );
            server.route(
                "/seg1.m4s",
                Route::new([&[0, 0, 0, 8][..], b"moof"].concat()),
            );
            let (_reader, _) = HlsReader::new(&server.url("/index.m3u8"), None).unwrap();
            // Wait for at least one playlist refresh (every 2 s at minimum)
            let start = Instant::now();
            while server.hits("/index.m3u8") < 2 && start.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(50));
            }
            assert!(server.hits("/index.m3u8") >= 2);
            assert_eq!(server.hits("/init.mp4"), 1);
        }

        #[test]
        fn failing_segments_report_the_http_status_quickly() {
            let server = TestServer::start();
            server.route("/index.m3u8", Route::new(vod(&["seg1.ts", "seg2.ts"])));
            server.route("/seg1.ts", Route::status(403));
            server.route("/seg2.ts", Route::status(404));
            let start = Instant::now();
            let err = HlsReader::new(&server.url("/index.m3u8"), None)
                .err()
                .unwrap()
                .to_string();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "took {:?}",
                start.elapsed()
            );
            assert!(err.contains("HTTP 404"), "{err}");
            assert!(err.contains("seg2.ts"), "{err}");
        }

        #[test]
        fn segments_without_audio_are_reported() {
            let server = TestServer::start();
            server.route("/index.m3u8", Route::new(vod(&["seg1.ts"])));
            // PAT and PMT only
            let seg = [pat(0x1000), pmt(0x1000, &[(0x1B, 0x100)])].concat();
            server.route("/seg1.ts", Route::new(seg));
            let err = HlsReader::new(&server.url("/index.m3u8"), None)
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains("no audio found"), "{err}");
        }

        #[test]
        fn encrypted_streams_are_reported() {
            let server = TestServer::start();
            let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n\
                #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg1.ts\n#EXT-X-ENDLIST\n";
            server.route("/index.m3u8", Route::new(playlist));
            let err = HlsReader::new(&server.url("/index.m3u8"), None)
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains("AES-128"), "{err}");
            assert_eq!(server.hits("/seg1.ts"), 0);
        }

        #[test]
        fn huge_target_duration_does_not_kill_the_downloader() {
            // `Instant + Duration::from_secs(u64::MAX)` panics, and the
            // release build aborts on panic: one bad playlist crashed the app
            let server = TestServer::start();
            let audio = frame(400);
            let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:18446744073709551615\n\
                #EXTINF:5.0,\nseg1.ts\n";
            server.route("/index.m3u8", Route::new(playlist));
            server.route("/seg1.ts", Route::new(plain_ts(&audio)));
            let (mut reader, _) = HlsReader::new(&server.url("/index.m3u8"), None).unwrap();
            assert_eq!(first_chunk(&mut reader), audio);
            // Give the downloader time to reach its reload wait
            thread::sleep(Duration::from_millis(500));
            let handle = reader._handle.as_ref().unwrap();
            assert!(!handle.is_finished(), "downloader thread died");
        }

        #[test]
        fn empty_first_playlist_still_starts_near_the_live_edge() {
            let server = TestServer::start();
            let audio = frame(400);
            // The encoder just started: a live playlist with no segments yet
            server.route(
                "/index.m3u8",
                Route::new("#EXTM3U\n#EXT-X-TARGETDURATION:2\n"),
            );
            let names: Vec<String> = (1..=10).map(|i| format!("seg{i}.ts")).collect();
            for name in &names {
                server.route(&format!("/{name}"), Route::new(plain_ts(&audio)));
            }
            let url = server.url("/index.m3u8");
            let opening = thread::spawn(move || HlsReader::new(&url, None).map(|(r, _)| r));

            // Next reload: a long window of ten segments
            thread::sleep(Duration::from_millis(300));
            let mut playlist = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:2\n");
            for name in &names {
                playlist.push_str(&format!("#EXTINF:2.0,\n{name}\n"));
            }
            server.route("/index.m3u8", Route::new(playlist));

            let _reader = opening.join().unwrap().unwrap();
            assert_eq!(server.hits("/seg1.ts"), 0, "started at the oldest segment");
            assert_eq!(
                server.hits("/seg8.ts"),
                1,
                "didn't start near the live edge"
            );
        }

        #[test]
        fn failed_start_stops_the_downloader() {
            let server = TestServer::start();
            // A live playlist that never lists a segment the reader can use
            let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\nseg1.ts\n";
            server.route("/index.m3u8", Route::new(playlist));
            server.route("/seg1.ts", Route::status(404));
            assert!(HlsReader::new(&server.url("/index.m3u8"), None).is_err());
            let hits = server.hits("/index.m3u8");
            thread::sleep(Duration::from_millis(2500));
            assert_eq!(server.hits("/index.m3u8"), hits);
        }

        /// Read until the stream ends, skipping the waits between segments.
        /// Returns the audio and how the stream ended.
        fn read_until_end(reader: &mut HlsReader) -> (Vec<u8>, io::Result<()>) {
            let mut audio = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            let start = Instant::now();
            loop {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "the stream should have ended"
                );
                match reader.read(&mut buf) {
                    Ok(0) => return (audio, Ok(())),
                    Ok(n) => audio.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return (audio, Err(e)),
                }
            }
        }

        /// A live playlist (no ENDLIST) with one segment
        const LIVE: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\nseg1.ts\n";

        #[test]
        fn a_live_playlist_that_stops_adding_segments_is_given_up_on() {
            let server = TestServer::start();
            let audio = frame(300);
            server.route("/index.m3u8", Route::new(LIVE));
            server.route("/seg1.ts", Route::new(plain_ts(&audio)));
            let (mut reader, _) =
                HlsReader::open(&server.url("/index.m3u8"), None, Duration::from_secs(1)).unwrap();

            let (played, end) = read_until_end(&mut reader);
            assert_eq!(played, audio);
            let err = end.expect_err("a live stream that stalls ends with an error");
            assert!(
                err.to_string()
                    .contains("No audio for 1 s: the playlist stopped adding segments"),
                "{err}"
            );
            let hits = server.hits("/index.m3u8");
            thread::sleep(Duration::from_millis(2500));
            assert_eq!(server.hits("/index.m3u8"), hits, "still polling");
        }

        #[test]
        fn a_playlist_that_starts_failing_stops_with_its_status() {
            let server = TestServer::start();
            let audio = frame(300);
            server.route("/index.m3u8", Route::new(LIVE));
            server.route("/seg1.ts", Route::new(plain_ts(&audio)));
            let (mut reader, _) =
                HlsReader::open(&server.url("/index.m3u8"), None, Duration::from_secs(1)).unwrap();
            server.route("/index.m3u8", Route::status(404));

            let (played, end) = read_until_end(&mut reader);
            assert_eq!(played, audio);
            let err = end.expect_err("a playlist that is gone ends the stream");
            assert!(
                err.to_string().contains("No audio for 1 s: HLS playlist"),
                "{err}"
            );
            assert!(err.to_string().contains("404"), "{err}");
        }

        #[test]
        fn a_finished_playlist_ends_without_an_error() {
            let server = TestServer::start();
            let audio = frame(300);
            server.route("/index.m3u8", Route::new(vod(&["seg1.ts"])));
            server.route("/seg1.ts", Route::new(plain_ts(&audio)));
            let (mut reader, _) = HlsReader::new(&server.url("/index.m3u8"), None).unwrap();

            let (played, end) = read_until_end(&mut reader);
            assert_eq!(played, audio);
            assert!(
                end.is_ok(),
                "the end of a VOD playlist is not an error: {end:?}"
            );
        }

        // --- parse_hls_playlist ---

        fn uris(content: &str) -> Vec<String> {
            match parse_hls_playlist(content.as_bytes()).unwrap() {
                Playlist::MediaPlaylist(pl) => pl.segments.into_iter().map(|s| s.uri).collect(),
                Playlist::MasterPlaylist(_) => panic!("expected a media playlist"),
            }
        }

        #[test]
        fn decimal_integer_tags_do_not_become_segments() {
            let pl = "#EXTM3U\n#EXT-X-VERSION:3.0\n#EXT-X-TARGETDURATION:6.0\n\
                #EXT-X-MEDIA-SEQUENCE:120.0\n#EXTINF:6.0,\nseg120.aac\n";
            assert_eq!(uris(pl), vec!["seg120.aac"]);
            match parse_hls_playlist(pl.as_bytes()).unwrap() {
                Playlist::MediaPlaylist(pl) => {
                    assert_eq!(pl.target_duration, 6);
                    assert_eq!(pl.media_sequence, 120);
                }
                Playlist::MasterPlaylist(_) => panic!("expected a media playlist"),
            }
        }

        #[test]
        fn titles_with_commas_do_not_become_segments() {
            let pl = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n\
                #EXTINF:10.0,title=\"Hello, Goodbye\",artist=\"The Beatles\"\nseg1.aac\n\
                #EXTINF:10.0,Artist - Song, Live\nseg2.aac\n\
                #EXTINF:-1 tvg-id=\"x\",Name\nseg3.aac\n";
            assert_eq!(uris(pl), vec!["seg1.aac", "seg2.aac", "seg3.aac"]);
        }

        #[test]
        fn parses_bom_crlf_and_extinf_without_comma() {
            let pl = "\u{feff}#EXTM3U\r\n#EXT-X-TARGETDURATION:4\r\n\
                #EXTINF:4\r\nseg1.ts\r\n#EXTINF:4.0,\r\n\r\nseg2.ts\r\n";
            assert_eq!(uris(pl), vec!["seg1.ts", "seg2.ts"]);
        }

        #[test]
        fn keeps_low_latency_and_unknown_tags_out_of_segments() {
            let pl =
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES\n\
                #EXT-X-PROGRAM-DATE-TIME:2026-09-26T21:52:05.123+0000\n\
                #EXT-X-PART:DURATION=1.0,URI=\"part1.mp4\"\n#EXTINF:4.0,\nseg1.mp4\n\
                #EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part2.mp4\"\n";
            assert_eq!(uris(pl), vec!["seg1.mp4"]);
        }
    }
}
