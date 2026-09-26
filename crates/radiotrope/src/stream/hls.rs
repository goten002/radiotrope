//! HLS stream reader
//!
//! Downloads HLS segments in the background, handles MPEG-TS demuxing and
//! fMP4 init segments, and provides a Read+Seek interface for the audio engine.
//! Song info found in the segments (ID3 timed metadata) or on the playlist's
//! `#EXTINF` lines is sent on a metadata channel.

use std::collections::HashSet;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use m3u8_rs::Playlist;
use mpeg2ts::ts::{ReadTsPacket, TsPacketReader, TsPayload};

use crate::config::hls::{SEGMENT_BUFFER_SIZE, SEGMENT_TIMEOUT_SECS};
use crate::config::network::USER_AGENT;
use crate::config::timeouts::CONNECT_TIMEOUT_SECS;
use crate::error::{RadioError, Result};
use crate::stream::hls_metadata::{
    extinf_titles, fmp4_emsg_id3, looks_like_fmp4, parse_extinf_title,
};
use crate::stream::id3::{parse_id3v2_payload, Id3Scanner};
use crate::stream::metadata::{MetadataSink, StreamMetadata};
use crate::stream::playlist::{get_base_url, make_absolute_url};

/// Detected segment container format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsSegmentFormat {
    MpegTs,
    Fmp4,
    Raw,
}

/// HLS stream reader — downloads segments in background, provides Read+Seek
pub struct HlsReader {
    buffer: Cursor<Vec<u8>>,
    receiver: Receiver<Vec<u8>>,
    stop_flag: Arc<AtomicBool>,
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
    pub fn new(
        media_url: &str,
        base_url: &str,
        playback_position: Option<Arc<AtomicU64>>,
    ) -> Result<(Self, Receiver<StreamMetadata>)> {
        let (sender, receiver) = bounded::<Vec<u8>>(SEGMENT_BUFFER_SIZE);
        let (metadata_sink, metadata_rx) = match playback_position {
            Some(position) => MetadataSink::synced(position),
            None => MetadataSink::channel(),
        };
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let bytes_received = Arc::new(AtomicU64::new(0));
        let bytes_clone = bytes_received.clone();
        let segments_downloaded = Arc::new(AtomicU64::new(0));
        let segments_clone = segments_downloaded.clone();

        let media_url_owned = media_url.to_string();
        let base_url_owned = base_url.to_string();

        let handle = thread::spawn(move || {
            let _ = segment_downloader(
                &media_url_owned,
                &base_url_owned,
                sender,
                metadata_sink,
                stop_clone,
                bytes_clone,
                segments_clone,
            );
        });

        // Wait for first segment
        let initial_data = receiver
            .recv_timeout(Duration::from_secs(SEGMENT_TIMEOUT_SECS * 2))
            .map_err(|_| {
                RadioError::Timeout("Timeout waiting for first HLS segment".to_string())
            })?;

        let detected_format = detect_segment_format(&initial_data, media_url);

        Ok((
            Self {
                buffer: Cursor::new(initial_data),
                receiver,
                stop_flag,
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

        // If buffer exhausted, wait for more.
        // Loop on timeout — the segment_downloader is resilient and keeps
        // retrying on network errors, so we should wait for it to recover
        // rather than giving up after a single timeout.
        let remaining = self
            .buffer
            .get_ref()
            .len()
            .saturating_sub(self.buffer.position() as usize);
        if remaining == 0 {
            loop {
                // Check stop flag between waits
                if self.stop_flag.load(Ordering::Relaxed) {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "HLS stream stopped",
                    ));
                }

                match self
                    .receiver
                    .recv_timeout(Duration::from_secs(SEGMENT_TIMEOUT_SECS))
                {
                    Ok(data) => {
                        self.buffer.get_mut().clear();
                        self.buffer.set_position(0);
                        self.buffer.get_mut().extend(data);
                        break;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // Downloader is still alive but no segment yet.
                        // Keep waiting — network may recover.
                        continue;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        // Downloader exited — stream is truly over.
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "HLS stream ended",
                        ));
                    }
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

/// Check if a segment URI looks valid (not metadata or garbage)
pub fn is_valid_segment_uri(uri: &str) -> bool {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Skip metadata lines (key="value" patterns)
    if trimmed.contains("=\"") || trimmed.contains("='") {
        return false;
    }

    let is_absolute = trimmed.starts_with("http://") || trimmed.starts_with("https://");
    if is_absolute {
        return true;
    }

    // For relative URIs, check for media extensions or path separators
    let has_media_extension = trimmed.ends_with(".ts")
        || trimmed.ends_with(".aac")
        || trimmed.ends_with(".m4s")
        || trimmed.ends_with(".mp4")
        || trimmed.ends_with(".m4a")
        || trimmed.contains(".ts?")
        || trimmed.contains(".aac?")
        || trimmed.contains(".m4s?");
    let has_path = trimmed.contains('/');

    has_media_extension || has_path
}

/// Demux an MPEG-TS segment and extract audio data
pub fn demux_ts_segment(ts_data: &[u8]) -> Vec<u8> {
    demux_ts_segment_with_metadata(ts_data).0
}

/// Demux an MPEG-TS segment, returning its audio data and the payload of
/// each timed-metadata PES packet (PMT stream type 0x15, which HLS uses for
/// ID3 tags).
pub fn demux_ts_segment_with_metadata(ts_data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut audio_data = Vec::new();
    let mut metadata = Vec::new();
    let mut audio_pids: HashSet<u16> = HashSet::new();
    let mut metadata_pids: HashSet<u16> = HashSet::new();

    // First pass: find audio PIDs from PMT
    let mut reader = TsPacketReader::new(Cursor::new(ts_data));
    loop {
        match reader.read_ts_packet() {
            Ok(Some(packet)) => {
                if let Some(TsPayload::Pmt(pmt)) = packet.payload {
                    for es in &pmt.es_info {
                        let stream_type = es.stream_type as u8;
                        // Audio stream types: MPEG1/2 (0x03, 0x04), AAC ADTS (0x0F),
                        // AAC LOAS (0x11), AC-3 (0x81), private (0x80)
                        if matches!(stream_type, 0x03 | 0x04 | 0x0F | 0x11 | 0x81 | 0x80) {
                            audio_pids.insert(es.elementary_pid.as_u16());
                        } else if stream_type == 0x15 {
                            // Packetized metadata: ID3 timed metadata in HLS
                            metadata_pids.insert(es.elementary_pid.as_u16());
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Fallback: common default audio PIDs
    if audio_pids.is_empty() {
        audio_pids.insert(257); // 0x101
        audio_pids.insert(258); // 0x102
    }

    // Second pass: extract audio data from identified PIDs
    let mut reader = TsPacketReader::new(Cursor::new(ts_data));
    loop {
        match reader.read_ts_packet() {
            Ok(Some(packet)) => {
                let pid = packet.header.pid.as_u16();
                if audio_pids.contains(&pid) {
                    if let Some(payload) = packet.payload {
                        match payload {
                            TsPayload::PesStart(pes) => {
                                audio_data.extend_from_slice(pes.data.as_ref());
                            }
                            TsPayload::PesContinuation(data) => {
                                audio_data.extend_from_slice(data.as_ref());
                            }
                            TsPayload::Raw(data) => {
                                audio_data.extend_from_slice(data.as_ref());
                            }
                            _ => {}
                        }
                    }
                } else if metadata_pids.contains(&pid) {
                    match packet.payload {
                        Some(TsPayload::PesStart(pes)) => metadata.push(pes.data.as_ref().to_vec()),
                        Some(TsPayload::PesContinuation(data)) => {
                            if let Some(last) = metadata.last_mut() {
                                last.extend_from_slice(data.as_ref());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    (audio_data, metadata)
}

/// Split a downloaded segment into its audio bytes and the song info it carries.
///
/// `is_fmp4` is the downloader's URL-based fMP4 decision; fMP4 audio is
/// passed through untouched (the caller prepends the init segment).
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

/// Resolve an HLS URL — follows master playlists to find the media playlist
pub fn resolve_hls_url(url: &str) -> Result<(String, String)> {
    let lower = url.to_lowercase();
    if !lower.ends_with(".m3u8") && !lower.contains(".m3u8?") {
        return Err(RadioError::Stream("Not an HLS URL".to_string()));
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(SEGMENT_TIMEOUT_SECS))
        .build()?;

    resolve_hls_recursive(url, &client, 5)
}

fn resolve_hls_recursive(
    url: &str,
    client: &reqwest::blocking::Client,
    depth: usize,
) -> Result<(String, String)> {
    if depth == 0 {
        return Err(RadioError::Stream(
            "HLS playlist nesting too deep".to_string(),
        ));
    }

    let response = client.get(url).send()?;
    if !response.status().is_success() {
        return Err(RadioError::Stream(format!("HTTP {}", response.status())));
    }

    let content = response.bytes()?;
    let base_url = get_base_url(url);

    match m3u8_rs::parse_playlist(&content) {
        Ok((_, Playlist::MasterPlaylist(master))) => {
            if let Some(variant) = master.variants.first() {
                let media_url = make_absolute_url(&variant.uri, &base_url);
                resolve_hls_recursive(&media_url, client, depth - 1)
            } else {
                Err(RadioError::Stream(
                    "No variants in master playlist".to_string(),
                ))
            }
        }
        Ok((_, Playlist::MediaPlaylist(_))) => Ok((url.to_string(), base_url)),
        Err(e) => Err(RadioError::Stream(format!("Playlist parse error: {:?}", e))),
    }
}

use super::backoff_sleep;

/// Background segment downloader
///
/// Uses URL-based deduplication to track which segments have been downloaded.
/// This handles both compliant and non-compliant HLS servers — some servers
/// keep `EXT-X-MEDIA-SEQUENCE` at 0 across playlist refreshes even as they
/// rotate segment URLs, which breaks sequence-number-based dedup.
fn segment_downloader(
    media_url: &str,
    base_url: &str,
    sender: Sender<Vec<u8>>,
    metadata_sink: MetadataSink,
    stop_flag: Arc<AtomicBool>,
    bytes_received: Arc<AtomicU64>,
    segments_downloaded: Arc<AtomicU64>,
) -> std::result::Result<(), String> {
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
    let mut init_segment: Option<Vec<u8>> = None;
    let mut init_segment_url: Option<String> = None;
    let mut consecutive_failures: u32 = 0;
    // Audio bytes sent to the reader so far (stream offset for song info)
    let mut bytes_sent: u64 = 0;

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Fetch playlist
        let response = match client.get(media_url).send() {
            Ok(r) => r,
            Err(_) => {
                consecutive_failures += 1;
                if !backoff_sleep(consecutive_failures, &stop_flag) {
                    return Ok(());
                }
                continue;
            }
        };

        if !response.status().is_success() {
            consecutive_failures += 1;
            if !backoff_sleep(consecutive_failures, &stop_flag) {
                return Ok(());
            }
            continue;
        }

        let content = match response.bytes() {
            Ok(b) => b,
            Err(_) => {
                consecutive_failures += 1;
                if !backoff_sleep(consecutive_failures, &stop_flag) {
                    return Ok(());
                }
                continue;
            }
        };

        let playlist = match m3u8_rs::parse_playlist(&content) {
            Ok((_, Playlist::MediaPlaylist(pl))) => pl,
            Ok(_) => return Err("Expected media playlist".to_string()),
            Err(_) => {
                consecutive_failures += 1;
                if !backoff_sleep(consecutive_failures, &stop_flag) {
                    return Ok(());
                }
                continue;
            }
        };

        // Playlist fetched successfully — reset backoff
        consecutive_failures = 0;

        // m3u8-rs cuts #EXTINF titles at the first comma, so read the full
        // titles (which may carry title="…",artist="…") from the raw text
        let segment_titles = extinf_titles(&content);

        let is_live = !playlist.end_list;
        let target_duration = playlist.target_duration as u64;

        // Handle fMP4 init segment (EXT-X-MAP)
        if init_segment.is_none() {
            if let Some(first_seg) = playlist.segments.first() {
                if let Some(ref map) = first_seg.map {
                    let map_url = make_absolute_url(&map.uri, base_url);
                    if init_segment_url.as_ref() != Some(&map_url) {
                        if let Ok(resp) = client.get(&map_url).send() {
                            if resp.status().is_success() {
                                if let Ok(data) = resp.bytes() {
                                    init_segment = Some(data.to_vec());
                                    init_segment_url = Some(map_url);
                                }
                            }
                        }
                    }
                }
            }
        }

        // On first fetch, set starting position.
        // For live: start a few segments back from the end to fill the pipeline.
        // Starting at only the last segment would mean 1 segment in the pipeline,
        // and new segments only arrive each playlist refresh — buffer starves.
        // Starting SEGMENT_BUFFER_SIZE back fills the bounded channel immediately.
        // For VOD: start from the beginning.
        let start_idx = if first_fetch && is_live {
            let start_back = SEGMENT_BUFFER_SIZE.min(playlist.segments.len());
            playlist.segments.len().saturating_sub(start_back)
        } else {
            0
        };
        // Mark skipped segments as "already seen" to prevent out-of-order
        // delivery on subsequent fetches when start_idx returns to 0
        for segment in playlist.segments.iter().take(start_idx) {
            if is_valid_segment_uri(&segment.uri) {
                downloaded_urls.insert(make_absolute_url(&segment.uri, base_url));
            }
        }
        first_fetch = false;

        let mut fetched_new = false;

        // Process segments, skipping already-downloaded URLs
        for segment in playlist.segments.iter().skip(start_idx) {
            if stop_flag.load(Ordering::SeqCst) {
                return Ok(());
            }

            if !is_valid_segment_uri(&segment.uri) {
                continue;
            }

            let segment_url = make_absolute_url(&segment.uri, base_url);

            // URL-based dedup: skip segments we've already downloaded
            if downloaded_urls.contains(&segment_url) {
                continue;
            }

            match client.get(&segment_url).send() {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(data) = resp.bytes() {
                        bytes_received.fetch_add(data.len() as u64, Ordering::Relaxed);
                        fetched_new = true;

                        let is_fmp4 =
                            segment_url.ends_with(".m4s") || segment_url.contains(".m4s?");
                        downloaded_urls.insert(segment_url);

                        let (mut audio_data, segment_meta) = split_segment(&data, is_fmp4);
                        if is_fmp4 && !sent_first {
                            // fMP4: prepend init segment for first segment
                            if let Some(ref init) = init_segment {
                                let mut combined = init.clone();
                                combined.append(&mut audio_data);
                                audio_data = combined;
                            }
                        }

                        if audio_data.is_empty() {
                            continue;
                        }

                        if stop_flag.load(Ordering::SeqCst) {
                            return Ok(());
                        }

                        // Song info applies from the start of this segment
                        let segment_start = bytes_sent;
                        bytes_sent += audio_data.len() as u64;
                        if sender.send(audio_data).is_err() {
                            return Ok(());
                        }
                        segments_downloaded.fetch_add(1, Ordering::Relaxed);

                        // Song info: playlist attributes first, since ID3
                        // from the segment itself takes priority over them
                        if let Some(meta) = segment_titles
                            .get(segment.uri.trim())
                            .and_then(|t| parse_extinf_title(t))
                        {
                            metadata_sink.offer_at(meta, segment_start);
                        }
                        for meta in segment_meta {
                            metadata_sink.offer_at(meta, segment_start);
                        }
                        if !sent_first {
                            sent_first = true;
                            // Init segment only needed for first fMP4 segment — free it
                            init_segment = None;
                            init_segment_url = None;
                        }
                    }
                }
                Ok(_) | Err(_) => {
                    // Segment failed (404, network error, etc.)
                    // Mark as downloaded to skip on next refresh — the content is
                    // time-sensitive and retrying would cause a glitch anyway.
                    downloaded_urls.insert(segment_url);
                    fetched_new = true;
                }
            }
        }

        // Bound the HashSet: keep only URLs still in the current playlist.
        // Old URLs that scrolled off the live window are removed.
        let current_urls: HashSet<String> = playlist
            .segments
            .iter()
            .map(|s| make_absolute_url(&s.uri, base_url))
            .collect();
        downloaded_urls.retain(|url| current_urls.contains(url));

        // VOD: done after all segments downloaded
        if !is_live {
            let all_done = playlist
                .segments
                .iter()
                .all(|s| downloaded_urls.contains(&make_absolute_url(&s.uri, base_url)));
            if all_done {
                return Ok(());
            }
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
        thread::sleep(wait);
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
    fn invalid_bare_name_no_extension() {
        assert!(!is_valid_segment_uri("somename"));
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
    fn invalid_single_word() {
        assert!(!is_valid_segment_uri("BANDWIDTH"));
    }

    #[test]
    fn invalid_number() {
        assert!(!is_valid_segment_uri("12345"));
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
            TsHeader, TsPacket, TsPacketWriter, VersionNumber, WriteTsPacket,
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

            let (_reader, rx) =
                HlsReader::new(&server.url("/live.m3u8"), &server.base_url, None).unwrap();
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
            let (_reader, rx) = HlsReader::new(
                &server.url("/live.m3u8"),
                &server.base_url,
                Some(position.clone()),
            )
            .unwrap();
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
}
