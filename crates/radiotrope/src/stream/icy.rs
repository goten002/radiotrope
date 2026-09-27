//! ICY stream reader
//!
//! Connects to Icecast/Shoutcast streams, extracts ICY metadata,
//! and provides a Read+Seek interface for the audio engine.
//!
//! MP3/AAC streams are also scanned for embedded ID3 tags (stations that
//! pipe whole files send one per track). The tags are cut out of the audio
//! and their song info is used when the station has no ICY title.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::config::network::{READ_TIMEOUT_SECS, USER_AGENT};
use crate::config::timeouts::{CONNECT_TIMEOUT_SECS, RECONNECT_GIVE_UP_SECS};
use crate::error::{RadioError, Result};
use crate::stream::id3::Id3Scanner;
use crate::stream::metadata::{extract_icy_title, MetadataSink, MetadataSource, StreamMetadata};
use crate::stream::resolver::StreamResolver;

use super::cancel::{StreamCancel, Waited};
use super::{backoff_sleep, gave_up, StreamEnd, READ_POLL_INTERVAL};

const AUDIO_CHANNEL_BOUND: usize = 32;

/// Headers parsed from an ICY stream response
#[derive(Debug, Clone)]
pub struct IcyHeaders {
    pub metaint: usize,
    pub station_name: Option<String>,
    pub content_type: Option<String>,
    pub bitrate: Option<u32>,
}

/// ICY stream reader that extracts metadata while passing audio through.
///
/// Uses a single-chunk design: holds at most one channel message at a time,
/// eliminating the redundant copy through an accumulation buffer.
pub struct IcyReader {
    current_chunk: Vec<u8>,
    chunk_pos: usize,
    receiver: Receiver<Vec<u8>>,
    /// Stops the background thread and this reader's waits
    cancel: StreamCancel,
    /// How the background thread ended, once it has
    end: StreamEnd,
    _handle: Option<JoinHandle<()>>,
    pub headers: IcyHeaders,
    /// Total bytes received from the network (updated by background thread)
    pub bytes_received: Arc<AtomicU64>,
}

// Safe: IcyReader is only accessed from one thread at a time (the audio engine thread).
// The Receiver and buffer are not shared; the background thread communicates via channel.
unsafe impl Sync for IcyReader {}

impl IcyReader {
    /// Connect to a URL with ICY metadata support and start background reading.
    ///
    /// Returns the reader and a channel that receives metadata updates. With
    /// `playback_position` (bytes of this reader's output the decoder has
    /// read), updates are held until playback reaches them.
    ///
    /// If the connection drops, the reader reconnects until audio flows
    /// again. After [`RECONNECT_GIVE_UP_SECS`] without audio it gives up,
    /// and reading returns an error with the reason.
    pub fn new(
        url: &str,
        playback_position: Option<Arc<AtomicU64>>,
    ) -> Result<(Self, Receiver<StreamMetadata>)> {
        Self::new_cancellable(url, playback_position, StreamCancel::new())
    }

    /// [`IcyReader::new`], stopped by `cancel` (or by dropping the reader):
    /// the background thread ends, and a read waiting for data returns end
    /// of stream at once
    pub fn new_cancellable(
        url: &str,
        playback_position: Option<Arc<AtomicU64>>,
        cancel: StreamCancel,
    ) -> Result<(Self, Receiver<StreamMetadata>)> {
        Self::open(
            url,
            playback_position,
            Duration::from_secs(RECONNECT_GIVE_UP_SECS),
            cancel,
        )
    }

    fn open(
        url: &str,
        playback_position: Option<Arc<AtomicU64>>,
        give_up: Duration,
        cancel: StreamCancel,
    ) -> Result<(Self, Receiver<StreamMetadata>)> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(READ_TIMEOUT_SECS))
            .build()?;

        let response = client.get(url).header("Icy-MetaData", "1").send()?;

        if !response.status().is_success() {
            return Err(RadioError::Stream(format!("HTTP {}", response.status())));
        }

        let headers = parse_icy_headers(&response);

        let metaint = headers.metaint;
        let scan_id3 = scans_for_id3(url, headers.content_type.as_deref());

        // Channels for audio data and metadata
        let (audio_tx, audio_rx) = bounded::<Vec<u8>>(AUDIO_CHANNEL_BOUND);
        let (metadata_sink, metadata_rx) = match playback_position {
            Some(position) => MetadataSink::synced(position),
            None => MetadataSink::channel(),
        };

        let bytes_received = Arc::new(AtomicU64::new(0));
        let bytes_clone = bytes_received.clone();
        let end = StreamEnd::default();

        let stream = IcyStream {
            url: url.to_string(),
            metadata_sink: metadata_sink.clone(),
            audio_out: AudioOut::new(audio_tx, metadata_sink, scan_id3, cancel.clone()),
            cancel: cancel.clone(),
            bytes_received: bytes_clone,
            end: end.clone(),
            give_up,
        };
        let handle = thread::spawn(move || stream.run(response, metaint));

        // Wait for initial data. On failure stop the thread, which would
        // otherwise keep reconnecting for the life of the process.
        let waited = cancel.recv(&audio_rx, Some(Duration::from_secs(READ_TIMEOUT_SECS)));
        let initial_data = match waited {
            Waited::Got(data) => data,
            failed => {
                cancel.cancel();
                return Err(match (failed, end.failure()) {
                    (Waited::Cancelled, _) => RadioError::Cancelled,
                    (_, Some(reason)) => RadioError::Stream(reason),
                    (Waited::TimedOut, None) => {
                        RadioError::Timeout("Timeout waiting for stream data".to_string())
                    }
                    (_, None) => {
                        RadioError::Stream("The stream ended before any audio".to_string())
                    }
                });
            }
        };

        Ok((
            Self {
                current_chunk: initial_data,
                chunk_pos: 0,
                receiver: audio_rx,
                cancel,
                end,
                _handle: Some(handle),
                headers,
                bytes_received,
            },
            metadata_rx,
        ))
    }

    /// Create an IcyReader from a test channel (bypasses HTTP)
    #[cfg(test)]
    pub fn from_test_channel(
        receiver: Receiver<Vec<u8>>,
        initial_data: Vec<u8>,
    ) -> (Self, StreamCancel) {
        let cancel = StreamCancel::new();
        (
            Self {
                current_chunk: initial_data,
                chunk_pos: 0,
                receiver,
                cancel: cancel.clone(),
                end: StreamEnd::default(),
                _handle: None,
                headers: IcyHeaders {
                    metaint: 0,
                    station_name: None,
                    content_type: None,
                    bitrate: None,
                },
                bytes_received: Arc::new(AtomicU64::new(0)),
            },
            cancel,
        )
    }
}

impl Read for IcyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            // Serve from current chunk
            let remaining = self.current_chunk.len() - self.chunk_pos;
            if remaining > 0 {
                let n = buf.len().min(remaining);
                buf[..n].copy_from_slice(&self.current_chunk[self.chunk_pos..self.chunk_pos + n]);
                self.chunk_pos += n;
                // Drop chunk when fully consumed
                if self.chunk_pos >= self.current_chunk.len() {
                    self.current_chunk = Vec::new();
                    self.chunk_pos = 0;
                }
                return Ok(n);
            }

            // Try non-blocking first (drain one pending chunk)
            match self.receiver.try_recv() {
                Ok(chunk) => {
                    self.current_chunk = chunk;
                    self.chunk_pos = 0;
                    continue;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    return self.end.read_result("ICY stream ended");
                }
            }

            // Wait for the background thread, which may be reconnecting with
            // backoff. Cancelling the stream ends the wait at once. After a
            // while return `Interrupted` rather than blocking on: a caller
            // that stops this reader some other way (the stream buffer's
            // producer, with a stop of its own) checks it and reads again.
            match self.cancel.recv(&self.receiver, Some(READ_POLL_INTERVAL)) {
                Waited::Got(chunk) => {
                    self.current_chunk = chunk;
                    self.chunk_pos = 0;
                }
                Waited::Closed => return self.end.read_result("ICY stream ended"),
                Waited::Cancelled => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "ICY stream stopped",
                    ));
                }
                Waited::TimedOut => {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "waiting for ICY stream data",
                    ));
                }
            }
        }
    }
}

impl Seek for IcyReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = self.current_chunk.len();
        let new_pos = match pos {
            SeekFrom::Start(p) => p as usize,
            SeekFrom::Current(p) => {
                if p >= 0 {
                    self.chunk_pos.saturating_add(p as usize)
                } else {
                    self.chunk_pos.saturating_sub((-p) as usize)
                }
            }
            SeekFrom::End(p) => {
                if p >= 0 {
                    len
                } else {
                    len.saturating_sub((-p) as usize)
                }
            }
        };

        self.chunk_pos = new_pos.min(len);
        Ok(self.chunk_pos as u64)
    }
}

impl Drop for IcyReader {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// True if the stream may carry embedded ID3 tags: MP3 or AAC, or a format
/// we can't tell. The scanner only cuts well-formed tags, so scanning an
/// unknown format is safe.
fn scans_for_id3(url: &str, content_type: Option<&str>) -> bool {
    matches!(
        StreamResolver::detect_format_hint(url, content_type).as_deref(),
        None | Some("mp3") | Some("aac")
    )
}

/// Where the reader thread sends audio. For MP3/AAC streams it first cuts
/// out embedded ID3 tags and offers their song info to the metadata sink.
struct AudioOut {
    tx: Sender<Vec<u8>>,
    cancel: StreamCancel,
    sink: MetadataSink,
    id3: Option<Id3Scanner>,
    /// Audio bytes sent so far: the stream offset song info is stamped with
    sent: u64,
}

impl AudioOut {
    fn new(tx: Sender<Vec<u8>>, sink: MetadataSink, scan_id3: bool, cancel: StreamCancel) -> Self {
        Self {
            tx,
            cancel,
            sink,
            id3: scan_id3.then(Id3Scanner::new),
            sent: 0,
        }
    }

    /// Send audio bytes. Returns false if the receiver is gone or the
    /// stream was cancelled.
    fn send(&mut self, bytes: &[u8]) -> bool {
        let Some(scanner) = &mut self.id3 else {
            self.sent += bytes.len() as u64;
            return self.cancel.send(&self.tx, bytes.to_vec());
        };
        let mut audio = Vec::with_capacity(bytes.len());
        let mut songs = Vec::new();
        scanner.push(bytes, &mut audio, &mut songs);
        // Tags sit between audio frames; stamping them at the end of this
        // chunk is within one read (a fraction of a second) of exact
        let at_byte = self.sent + audio.len() as u64;
        for song in songs {
            self.sink.offer_at(song, at_byte);
        }
        self.sent += audio.len() as u64;
        audio.is_empty() || self.cancel.send(&self.tx, audio)
    }

    /// The connection was replaced: drop any partial tag from the old one
    fn reset(&mut self) {
        if let Some(scanner) = &mut self.id3 {
            *scanner = Id3Scanner::new();
        }
    }
}

fn parse_icy_headers(response: &reqwest::blocking::Response) -> IcyHeaders {
    let headers = response.headers();

    let metaint = headers
        .get("icy-metaint")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let station_name = headers
        .get("icy-name")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let bitrate = headers
        .get("icy-br")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok());

    IcyHeaders {
        metaint,
        station_name,
        content_type,
        bitrate,
    }
}

/// The background thread that reads an ICY stream: it cuts out the ICY
/// metadata, sends the audio to the reader, and reconnects when the
/// connection drops.
struct IcyStream {
    url: String,
    metadata_sink: MetadataSink,
    audio_out: AudioOut,
    cancel: StreamCancel,
    bytes_received: Arc<AtomicU64>,
    end: StreamEnd,
    /// How long to keep reconnecting without getting any audio
    give_up: Duration,
}

impl IcyStream {
    /// Read until stopped, until a finite body is complete, or until no
    /// audio has arrived for `give_up`. Records how it ended in `end`.
    fn run(mut self, mut response: reqwest::blocking::Response, mut metaint: usize) {
        let mut finite = is_finite_body(&response);
        let mut bytes_until_meta = metaint;
        let mut last_title = String::new();
        let mut chunk_buffer = vec![0u8; 8192];
        let mut consecutive_failures: u32 = 0;
        // When audio stopped arriving, while it stays away
        let mut outage_since: Option<Instant> = None;

        loop {
            if self.cancel.is_cancelled() {
                return;
            }

            let read_result = if metaint == 0 {
                read_chunk_no_meta(
                    &mut response,
                    &mut chunk_buffer,
                    &mut self.audio_out,
                    &self.bytes_received,
                )
            } else {
                read_chunk_with_meta(
                    &mut response,
                    &mut chunk_buffer,
                    &mut bytes_until_meta,
                    metaint,
                    &mut last_title,
                    &self.metadata_sink,
                    &mut self.audio_out,
                    &self.bytes_received,
                )
            };

            let mut problem = match read_result {
                ReadResult::Ok => {
                    consecutive_failures = 0;
                    outage_since = None;
                    continue;
                }
                ReadResult::ChannelClosed => return,
                // A whole file: replaying it from the start would loop it
                ReadResult::Eof if finite => {
                    self.end.finish();
                    return;
                }
                ReadResult::Eof => "the server closed the connection".to_string(),
                ReadResult::Error(e) => format!("connection lost ({e})"),
            };

            // Reconnect with backoff. The failure count and the outage only
            // reset once audio arrives, so a server that accepts and then
            // closes right away is not hammered, and is given up on too.
            let since = *outage_since.get_or_insert_with(Instant::now);
            loop {
                if since.elapsed() >= self.give_up {
                    self.end.fail(gave_up(self.give_up, &problem));
                    return;
                }
                consecutive_failures += 1;
                if !backoff_sleep(consecutive_failures, &self.cancel) {
                    return; // Stopped during sleep
                }
                match reconnect(&self.url) {
                    Ok(new_response) => {
                        // A reconnect can land on another server or mount:
                        // take its metadata interval, not the first one's
                        metaint = parse_icy_headers(&new_response).metaint;
                        bytes_until_meta = metaint;
                        finite = is_finite_body(&new_response);
                        response = new_response;
                        self.audio_out.reset();
                        break;
                    }
                    Err(reason) => problem = reason,
                }
            }
        }
    }
}

enum ReadResult {
    Ok,
    Eof,
    Error(String),
    ChannelClosed,
}

/// True for a plain HTTP file: it has a length and no ICY headers. Once its
/// body is complete the stream is over. ICY servers are live even when they
/// send a (made-up) length.
fn is_finite_body(response: &reqwest::blocking::Response) -> bool {
    response.content_length().is_some()
        && !response
            .headers()
            .keys()
            .any(|name| name.as_str().starts_with("icy-"))
}

/// Read one chunk without ICY metadata
fn read_chunk_no_meta(
    response: &mut reqwest::blocking::Response,
    chunk_buffer: &mut [u8],
    audio_out: &mut AudioOut,
    bytes_received: &Arc<AtomicU64>,
) -> ReadResult {
    match response.read(chunk_buffer) {
        Ok(0) => ReadResult::Eof,
        Ok(n) => {
            bytes_received.fetch_add(n as u64, Ordering::Relaxed);
            if !audio_out.send(&chunk_buffer[..n]) {
                ReadResult::ChannelClosed
            } else {
                ReadResult::Ok
            }
        }
        Err(e) => ReadResult::Error(e.to_string()),
    }
}

/// Read one chunk with ICY metadata extraction
#[allow(clippy::too_many_arguments)]
fn read_chunk_with_meta(
    response: &mut reqwest::blocking::Response,
    chunk_buffer: &mut [u8],
    bytes_until_meta: &mut usize,
    metaint: usize,
    last_title: &mut String,
    metadata_sink: &MetadataSink,
    audio_out: &mut AudioOut,
    bytes_received: &Arc<AtomicU64>,
) -> ReadResult {
    let to_read = if *bytes_until_meta > 0 {
        chunk_buffer.len().min(*bytes_until_meta)
    } else {
        0
    };

    if to_read > 0 {
        match response.read(&mut chunk_buffer[..to_read]) {
            Ok(0) => return ReadResult::Eof,
            Ok(n) => {
                bytes_received.fetch_add(n as u64, Ordering::Relaxed);
                if !audio_out.send(&chunk_buffer[..n]) {
                    return ReadResult::ChannelClosed;
                }
                *bytes_until_meta -= n;
            }
            Err(e) => return ReadResult::Error(e.to_string()),
        }
    }

    if *bytes_until_meta == 0 {
        // Read metadata length byte
        let mut len_byte = [0u8; 1];
        if let Err(e) = response.read_exact(&mut len_byte) {
            return ReadResult::Error(e.to_string());
        }

        let meta_len = len_byte[0] as usize * 16;
        if meta_len > 0 {
            let mut meta_buf = vec![0u8; meta_len];
            if let Err(e) = response.read_exact(&mut meta_buf) {
                return ReadResult::Error(e.to_string());
            }

            match extract_icy_title(&meta_buf) {
                Some(title) => {
                    if *title != *last_title {
                        *last_title = title.clone();
                        metadata_sink
                            .offer_at(StreamMetadata::from_icy_title(&title), audio_out.sent);
                    }
                }
                None => {
                    // Blank or missing StreamTitle: let embedded ID3 song info show
                    if !last_title.is_empty() {
                        last_title.clear();
                        metadata_sink.offer_at(
                            StreamMetadata::new(None, None, MetadataSource::Icy),
                            audio_out.sent,
                        );
                    }
                }
            }
        }

        *bytes_until_meta = metaint;
    }

    ReadResult::Ok
}

/// Connect to the ICY stream again. Backoff sleep is handled by the caller.
/// `Err` says why this attempt failed.
fn reconnect(url: &str) -> std::result::Result<reqwest::blocking::Response, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(READ_TIMEOUT_SECS))
        .build()
        .map_err(|e| e.to_string())?;

    let response = client
        .get(url)
        .header("Icy-MetaData", "1")
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                "the server did not answer".to_string()
            } else if e.is_connect() {
                "could not connect to the server".to_string()
            } else {
                e.without_url().to_string()
            }
        })?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    // An error or parking page served with 200 OK must not reach the decoder
    let is_web_page = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"));
    if is_web_page {
        return Err("the server sent a web page instead of audio".to_string());
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- IcyHeaders ---

    #[test]
    fn icy_headers_debug() {
        let h = IcyHeaders {
            metaint: 16000,
            station_name: Some("Test FM".to_string()),
            content_type: Some("audio/mpeg".to_string()),
            bitrate: Some(128),
        };
        let debug = format!("{:?}", h);
        assert!(debug.contains("16000"));
        assert!(debug.contains("Test FM"));
    }

    #[test]
    fn icy_headers_clone() {
        let h = IcyHeaders {
            metaint: 8000,
            station_name: None,
            content_type: None,
            bitrate: None,
        };
        let cloned = h.clone();
        assert_eq!(cloned.metaint, 8000);
        assert!(cloned.station_name.is_none());
    }

    // --- IcyReader from test channel ---

    #[test]
    fn read_from_channel() {
        let (tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3, 4]);

        let mut buf = [0u8; 4];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(buf, [1, 2, 3, 4]);

        // Send more data
        tx.send(vec![5, 6]).unwrap();
        let mut buf2 = [0u8; 2];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf2, [5, 6]);
    }

    #[test]
    fn read_partial() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![10, 20, 30, 40, 50]);

        // Read less than available
        let mut buf = [0u8; 2];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, [10, 20]);

        // Read rest
        let mut buf2 = [0u8; 10];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf2[..3], &[30, 40, 50]);
    }

    #[test]
    fn seek_within_current_chunk() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3, 4, 5]);

        // Read 3 bytes (partial consumption)
        let mut buf = [0u8; 3];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3]);

        // Seek back to start of current chunk
        let pos = reader.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(pos, 0);

        // Read again from start
        let mut buf2 = [0u8; 5];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 5);
        assert_eq!(buf2, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn seek_current_forward() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![10, 20, 30, 40, 50]);

        // Seek forward from start within current chunk
        let pos = reader.seek(SeekFrom::Current(3)).unwrap();
        assert_eq!(pos, 3);

        let mut buf = [0u8; 2];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, [40, 50]);
    }

    #[test]
    fn seek_current_backward() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3, 4, 5]);

        // Read 4 bytes
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();

        // Seek back 2
        let pos = reader.seek(SeekFrom::Current(-2)).unwrap();
        assert_eq!(pos, 2);

        let mut buf2 = [0u8; 3];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf2, [3, 4, 5]);
    }

    #[test]
    fn seek_end() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3, 4, 5]);

        // Seek to 2 bytes before end of current chunk
        let pos = reader.seek(SeekFrom::End(-2)).unwrap();
        assert_eq!(pos, 3);

        let mut buf = [0u8; 2];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, [4, 5]);
    }

    #[test]
    fn seek_clamps_to_chunk_length() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);

        // Seek past end of current chunk
        let pos = reader.seek(SeekFrom::Start(100)).unwrap();
        assert_eq!(pos, 3);
    }

    #[test]
    fn read_returns_interrupted_while_waiting_for_data() {
        // The producer relies on this to check its stop flag while the
        // background thread reconnects, instead of blocking indefinitely.
        let (_tx, rx) = bounded::<Vec<u8>>(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, Vec::new());
        let start = std::time::Instant::now();
        let mut buf = [0u8; 16];
        let err = reader.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn read_after_interrupted_gets_the_next_chunk() {
        let (tx, rx) = bounded::<Vec<u8>>(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, Vec::new());
        let mut buf = [0u8; 16];
        assert_eq!(
            reader.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        tx.send(vec![4, 5, 6]).unwrap();
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], &[4, 5, 6]);
    }

    #[test]
    fn drop_cancels_the_stream() {
        let (_tx, rx) = bounded(8);
        let (reader, cancel) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
        assert!(!cancel.is_cancelled());
        drop(reader);
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn a_cancelled_read_ends_at_once() {
        let (_tx, rx) = bounded::<Vec<u8>>(8);
        let (mut reader, cancel) = IcyReader::from_test_channel(rx, Vec::new());
        let (done_tx, done) = bounded(1);
        let reading = thread::spawn(move || {
            let mut buf = [0u8; 16];
            // Keep reading through `Interrupted`, as the stream buffer does
            let result = loop {
                match reader.read(&mut buf) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    other => break other,
                }
            };
            let _ = done_tx.send(());
            result.map_err(|e| e.kind())
        });
        thread::sleep(Duration::from_millis(100));
        let cancelled_at = std::time::Instant::now();
        cancel.cancel();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(100),
            "the read waited for its poll interval: {:?}",
            cancelled_at.elapsed()
        );
        assert_eq!(reading.join().unwrap(), Err(io::ErrorKind::UnexpectedEof));
    }

    #[test]
    fn empty_channel_returns_available_data() {
        let (tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2]);

        // Read initial data
        let mut buf = [0u8; 2];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);

        // Send more data before reading
        tx.send(vec![3, 4, 5]).unwrap();

        let mut buf2 = [0u8; 3];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf2, [3, 4, 5]);
    }

    #[test]
    fn reader_handles_multiple_chunks() {
        let (tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![]);

        tx.send(vec![1, 2]).unwrap();
        tx.send(vec![3, 4]).unwrap();
        tx.send(vec![5, 6]).unwrap();

        // Single-chunk design: each read serves at most one chunk
        let mut buf = [0u8; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], &[1, 2]);

        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], &[3, 4]);

        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], &[5, 6]);
    }

    #[test]
    fn icy_headers_with_all_fields() {
        let h = IcyHeaders {
            metaint: 16000,
            station_name: Some("Classic FM".to_string()),
            content_type: Some("audio/mpeg".to_string()),
            bitrate: Some(320),
        };
        assert_eq!(h.metaint, 16000);
        assert_eq!(h.station_name.as_deref(), Some("Classic FM"));
        assert_eq!(h.content_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(h.bitrate, Some(320));
    }

    #[test]
    fn icy_headers_no_optional_fields() {
        let h = IcyHeaders {
            metaint: 0,
            station_name: None,
            content_type: None,
            bitrate: None,
        };
        assert_eq!(h.metaint, 0);
        assert!(h.station_name.is_none());
        assert!(h.content_type.is_none());
        assert!(h.bitrate.is_none());
    }

    // --- Seek edge cases ---

    #[test]
    fn seek_start_zero_on_empty_chunk() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![]);
        let pos = reader.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(pos, 0);
    }

    #[test]
    fn seek_end_positive_clamps() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
        // SeekFrom::End(0) should be at position 3 (clamped to len)
        let pos = reader.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(pos, 3);
    }

    #[test]
    fn seek_end_beyond_clamps() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
        // SeekFrom::End(10) → len + 10, clamped to len
        let pos = reader.seek(SeekFrom::End(10)).unwrap();
        assert_eq!(pos, 3);
    }

    #[test]
    fn seek_current_negative_past_zero_saturates() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
        // position=0, seek -10 → saturates to 0
        let pos = reader.seek(SeekFrom::Current(-10)).unwrap();
        assert_eq!(pos, 0);
    }

    #[test]
    fn seek_then_read_correct_data() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![10, 20, 30, 40, 50, 60]);

        // Seek to position 2
        reader.seek(SeekFrom::Start(2)).unwrap();

        let mut buf = [0u8; 3];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf, [30, 40, 50]);
    }

    #[test]
    fn multiple_seeks_in_sequence() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3, 4, 5]);

        reader.seek(SeekFrom::Start(3)).unwrap();
        reader.seek(SeekFrom::Current(-1)).unwrap();
        let pos = reader.stream_position().unwrap();
        assert_eq!(pos, 2);

        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], 3);
    }

    // --- Read edge cases ---

    #[test]
    fn read_zero_length_buffer() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
        let mut buf = [0u8; 0];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn read_exactly_available_amount() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
        let mut buf = [0u8; 3];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf, [1, 2, 3]);
    }

    #[test]
    fn read_more_than_available() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2]);
        let mut buf = [0u8; 100];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], &[1, 2]);
    }

    #[test]
    fn seek_back_within_partially_consumed_chunk() {
        let (_tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3, 4, 5]);

        // Read 3 of 5 bytes (chunk still held)
        let mut buf = [0u8; 3];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3]);

        // Seek back to start of current chunk, re-read
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf2 = [0u8; 5];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 5);
        assert_eq!(buf2, [1, 2, 3, 4, 5]);
    }

    // --- Channel interaction edge cases ---

    #[test]
    fn channel_chunks_served_one_at_a_time() {
        let (tx, rx) = bounded(16);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![]);

        // Send many small chunks
        for i in 0u8..10 {
            tx.send(vec![i]).unwrap();
        }

        // Each read serves one chunk at a time
        let mut buf = [0u8; 10];
        for i in 0u8..10 {
            let n = reader.read(&mut buf).unwrap();
            assert_eq!(n, 1);
            assert_eq!(buf[0], i);
        }
    }

    #[test]
    fn read_interleaved_with_channel_sends() {
        let (tx, rx) = bounded(8);
        let (mut reader, _stop) = IcyReader::from_test_channel(rx, vec![1, 2]);

        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2]);

        tx.send(vec![3, 4]).unwrap();
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [3, 4]);

        tx.send(vec![5, 6]).unwrap();
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [5, 6]);
    }

    // --- Drop behavior ---

    #[test]
    fn drop_with_pending_channel_data() {
        let (tx, rx) = bounded(8);
        let (reader, cancel) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
        tx.send(vec![4, 5, 6]).unwrap();
        drop(reader);
        assert!(cancel.is_cancelled());
    }

    // --- IcyHeaders edge cases ---

    #[test]
    fn icy_headers_large_metaint() {
        let h = IcyHeaders {
            metaint: usize::MAX,
            station_name: None,
            content_type: None,
            bitrate: None,
        };
        assert_eq!(h.metaint, usize::MAX);
    }

    #[test]
    fn icy_headers_unicode_station_name() {
        let h = IcyHeaders {
            metaint: 16000,
            station_name: Some("Радио Россия 🎵".to_string()),
            content_type: None,
            bitrate: None,
        };
        assert_eq!(h.station_name.as_deref(), Some("Радио Россия 🎵"));
    }

    // --- Embedded ID3 tags ---

    mod embedded_id3 {
        use super::*;
        use crate::stream::id3::test_util::{frame, id3v1_song, id3v2_song};
        use crate::stream::test_server::{Route, TestServer};
        use std::sync::atomic::AtomicBool;

        const METAINT: usize = 256;

        /// Interleave ICY metadata blocks into `audio` every METAINT bytes.
        /// `titles[i]` is the block after the i-th audio run (None = no change).
        fn with_icy(audio: &[u8], titles: &[Option<&str>]) -> Vec<u8> {
            let mut out = Vec::new();
            for (i, run) in audio.chunks(METAINT).enumerate() {
                out.extend_from_slice(run);
                if run.len() < METAINT {
                    break;
                }
                match titles.get(i).copied().flatten() {
                    Some(title) => {
                        let mut block = format!("StreamTitle='{title}';").into_bytes();
                        block.resize(block.len().div_ceil(16) * 16, 0);
                        out.push((block.len() / 16) as u8);
                        out.extend(block);
                    }
                    None => out.push(0),
                }
            }
            out
        }

        fn read_audio(reader: &mut IcyReader, len: usize) -> Vec<u8> {
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).unwrap();
            buf
        }

        fn next_title(rx: &Receiver<StreamMetadata>) -> Option<(String, MetadataSource)> {
            rx.recv_timeout(Duration::from_secs(3))
                .ok()
                .map(|m| (m.title.unwrap_or_default(), m.source))
        }

        /// Two "tracks" as a file-piping station sends them
        fn two_tracks() -> (Vec<u8>, Vec<u8>) {
            let a = frame(600);
            let b = frame(600);
            let stream = [
                id3v2_song("Artist", "Track One"),
                a.clone(),
                id3v1_song("Artist", "Track One"),
                id3v2_song("Artist", "Track Two"),
                b.clone(),
            ]
            .concat();
            (stream, [a, b].concat())
        }

        #[test]
        fn mp3_stream_without_icy_uses_id3() {
            let (stream, audio) = two_tracks();
            let server = TestServer::start();
            server.route(
                "/live",
                Route::new(stream).header("Content-Type", "audio/mpeg"),
            );
            let (mut reader, rx) = IcyReader::new(&server.url("/live"), None).unwrap();
            assert_eq!(read_audio(&mut reader, audio.len()), audio);
            assert_eq!(
                next_title(&rx),
                Some(("Track One".to_string(), MetadataSource::Id3v2))
            );
            assert_eq!(
                next_title(&rx),
                Some(("Track Two".to_string(), MetadataSource::Id3v2))
            );
        }

        #[test]
        fn stopping_a_station_that_went_down_ends_its_threads() {
            use crate::stream::buffer::{BufferStatus, StreamBuffer};
            use std::sync::Mutex;

            // The station sends a little audio, closes, and every reconnect
            // gets a 404: the ICY thread keeps retrying with backoff.
            let server = TestServer::start();
            server.route(
                "/live",
                Route::new(frame(600))
                    .without_length()
                    .header("Content-Type", "audio/mpeg"),
            );
            let (reader, _rx) = IcyReader::new(&server.url("/live"), None).unwrap();
            server.route("/live", Route::status(404));

            let status = Arc::new(Mutex::new(BufferStatus::default()));
            let probing = Arc::new(AtomicBool::new(false));
            let (mut consumer, producer, stop) =
                StreamBuffer::new(Box::new(reader), status, probing);

            // The decoder's read waits for the buffer to fill, which it never
            // will: the station is gone and the ICY thread keeps reconnecting
            let (tx, read_done) = bounded(1);
            thread::spawn(move || {
                let mut buf = vec![0u8; 4096];
                loop {
                    match consumer.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                let _ = tx.send(());
            });
            thread::sleep(Duration::from_millis(500));
            stop.cancel();

            read_done
                .recv_timeout(Duration::from_secs(3))
                .expect("the decoder's read must end when the station is stopped");
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !producer.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                producer.is_finished(),
                "the producer must let go of a station that is down"
            );

            // Dropping the reader stopped the ICY thread: no more reconnects
            thread::sleep(Duration::from_millis(500));
            let hits = server.hits("/live");
            thread::sleep(Duration::from_secs(3));
            assert_eq!(server.hits("/live"), hits, "ICY thread still reconnecting");
        }

        #[test]
        fn icy_title_wins_over_id3() {
            let (stream, audio) = two_tracks();
            let body = with_icy(&stream, &[Some("Icy Artist - Icy Song")]);
            let server = TestServer::start();
            server.route(
                "/live",
                Route::new(body)
                    .header("Content-Type", "audio/mpeg")
                    .header("icy-metaint", &METAINT.to_string()),
            );
            let (mut reader, rx) = IcyReader::new(&server.url("/live"), None).unwrap();
            assert_eq!(read_audio(&mut reader, audio.len()), audio);
            let titles: Vec<_> = rx.try_iter().map(|m| (m.title, m.source)).collect();
            // Track One's ID3 arrives before the first ICY block, then ICY
            // takes over and Track Two's ID3 is ignored
            assert_eq!(
                titles,
                vec![
                    (Some("Track One".to_string()), MetadataSource::Id3v2),
                    (Some("Icy Song".to_string()), MetadataSource::Icy),
                ]
            );
        }

        #[test]
        fn blank_icy_title_falls_back_to_id3() {
            let a = frame(700);
            let b = frame(700);
            let stream = [a.clone(), id3v2_song("Artist", "From ID3"), b.clone()].concat();
            // ICY title first, then the station blanks it
            let body = with_icy(&stream, &[Some("Icy - Song"), Some("")]);
            let server = TestServer::start();
            server.route(
                "/live",
                Route::new(body)
                    .header("Content-Type", "audio/mpeg")
                    .header("icy-metaint", &METAINT.to_string()),
            );
            let (mut reader, rx) = IcyReader::new(&server.url("/live"), None).unwrap();
            read_audio(&mut reader, a.len() + b.len());
            let titles: Vec<_> = rx.try_iter().map(|m| m.title.unwrap()).collect();
            assert_eq!(titles, vec!["Song".to_string(), "From ID3".to_string()]);
        }

        #[test]
        fn ogg_stream_is_not_scanned() {
            // Bytes that would be a tag in an MP3 stream pass through untouched
            let body = [frame(100), id3v2_song("A", "B"), frame(100)].concat();
            let server = TestServer::start();
            server.route(
                "/live",
                Route::new(body.clone()).header("Content-Type", "audio/ogg"),
            );
            let (mut reader, rx) = IcyReader::new(&server.url("/live"), None).unwrap();
            assert_eq!(read_audio(&mut reader, body.len()), body);
            assert!(rx.try_recv().is_err());
        }

        #[test]
        fn scan_decision_by_format() {
            assert!(scans_for_id3("http://x/live", Some("audio/mpeg")));
            assert!(scans_for_id3("http://x/live", Some("audio/aacp")));
            assert!(scans_for_id3("http://x/live", None));
            assert!(scans_for_id3("http://x/live.mp3", None));
            assert!(!scans_for_id3("http://x/live", Some("audio/ogg")));
            assert!(!scans_for_id3("http://x/live", Some("audio/flac")));
            assert!(!scans_for_id3("http://x/live.opus", None));
        }
    }

    // --- Reconnecting, and giving up ---

    mod reconnect {
        use super::*;
        use crate::stream::buffer::{BufferStatus, StreamBuffer};
        use crate::stream::id3::test_util::frame;
        use crate::stream::test_server::{Route, TestServer};
        use std::sync::atomic::AtomicBool;
        use std::sync::Mutex;

        const GIVE_UP: Duration = Duration::from_secs(1);

        /// A live station: no length, ICY headers
        fn live(body: Vec<u8>) -> Route {
            Route::new(body)
                .without_length()
                .header("Content-Type", "audio/mpeg")
                .header("icy-name", "Test FM")
        }

        /// `audio` with an ICY metadata block every `metaint` bytes, the
        /// first one carrying `title`
        fn icy_body(audio: &[u8], metaint: usize, title: &str) -> Vec<u8> {
            let mut out = Vec::new();
            for (i, run) in audio.chunks(metaint).enumerate() {
                out.extend_from_slice(run);
                if run.len() < metaint {
                    break;
                }
                if i == 0 {
                    let mut block = format!("StreamTitle='{title}';").into_bytes();
                    block.resize(block.len().div_ceil(16) * 16, 0);
                    out.push((block.len() / 16) as u8);
                    out.extend(block);
                } else {
                    out.push(0);
                }
            }
            out
        }

        /// Read `reader` on a thread until the stream ends. Returns the
        /// audio and how the stream ended; panics if it doesn't end in time.
        fn read_until_end(
            mut reader: impl Read + Send + 'static,
            limit: Duration,
        ) -> (Vec<u8>, io::Result<()>) {
            let (tx, rx) = bounded(1);
            thread::spawn(move || {
                let mut audio = Vec::new();
                let mut buf = vec![0u8; 4096];
                let end = loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break Ok(()),
                        Ok(n) => audio.extend_from_slice(&buf[..n]),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => break Err(e),
                    }
                };
                let _ = tx.send((audio, end));
            });
            rx.recv_timeout(limit)
                .expect("the stream should have ended, not kept reconnecting")
        }

        /// Wait out a backoff and check the station is left alone
        fn assert_no_more_requests(server: &TestServer, path: &str) {
            let hits = server.hits(path);
            thread::sleep(Duration::from_millis(2500));
            assert_eq!(server.hits(path), hits, "still reconnecting");
        }

        #[test]
        fn a_station_that_stays_down_stops_with_the_reason() {
            let server = TestServer::start();
            let audio = frame(600);
            server.route("/live", live(audio.clone()));
            let (reader, _rx) =
                IcyReader::open(&server.url("/live"), None, GIVE_UP, StreamCancel::new()).unwrap();
            server.route("/live", Route::status(404));

            // Through the stream buffer, as the decoder reads it
            let status = Arc::new(Mutex::new(BufferStatus::default()));
            let probing = Arc::new(AtomicBool::new(false));
            let (consumer, _producer, _stop) = StreamBuffer::new(Box::new(reader), status, probing);
            let (played, end) = read_until_end(consumer, Duration::from_secs(10));

            assert_eq!(played, audio);
            let err = end.expect_err("a dead station ends with an error");
            assert!(
                err.to_string().contains("No audio for 1 s: HTTP 404"),
                "{err}"
            );
            assert_no_more_requests(&server, "/live");
        }

        #[test]
        fn a_server_that_answers_and_hangs_up_is_given_up_on() {
            // Connecting works, but no audio ever comes: the old reader
            // reset its backoff on every connect and retried forever
            let server = TestServer::start();
            let audio = frame(600);
            server.route("/live", live(audio.clone()));
            let (reader, _rx) =
                IcyReader::open(&server.url("/live"), None, GIVE_UP, StreamCancel::new()).unwrap();
            server.route("/live", live(Vec::new()));

            let (played, end) = read_until_end(reader, Duration::from_secs(10));
            assert_eq!(played, audio);
            let err = end.expect_err("a station that sends nothing ends with an error");
            assert!(
                err.to_string()
                    .contains("No audio for 1 s: the server closed the connection"),
                "{err}"
            );
            assert_no_more_requests(&server, "/live");
        }

        #[test]
        fn a_web_page_on_reconnect_is_not_played() {
            let server = TestServer::start();
            let audio = frame(600);
            server.route("/live", live(audio.clone()));
            let (reader, _rx) =
                IcyReader::open(&server.url("/live"), None, GIVE_UP, StreamCancel::new()).unwrap();
            server.route(
                "/live",
                Route::new("<html><body>Station offline</body></html>")
                    .header("Content-Type", "text/html; charset=utf-8"),
            );

            let (played, end) = read_until_end(reader, Duration::from_secs(10));
            assert_eq!(played, audio, "the page must not reach the decoder");
            let err = end.expect_err("an offline page ends the stream");
            assert!(err.to_string().contains("web page"), "{err}");
        }

        #[test]
        fn a_reconnect_takes_the_new_servers_metadata_interval() {
            // The reconnect lands on a server with another interval: reading
            // it with the first one's would cut audio into metadata and back
            let server = TestServer::start();
            let first = frame(600);
            let second = frame(700);
            server.route(
                "/live",
                live(icy_body(&first, 256, "One")).header("icy-metaint", "256"),
            );
            let (mut reader, titles) = IcyReader::open(
                &server.url("/live"),
                None,
                Duration::from_secs(30),
                StreamCancel::new(),
            )
            .unwrap();
            server.route(
                "/live",
                live(icy_body(&second, 100, "Two")).header("icy-metaint", "100"),
            );

            let (tx, rx) = bounded(1);
            let total = first.len() + second.len();
            thread::spawn(move || {
                let mut audio = vec![0u8; total];
                let _ = tx.send(reader.read_exact(&mut audio).map(|_| audio));
            });
            let audio = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the reconnected stream should play")
                .unwrap();
            assert!(audio == [first, second].concat(), "audio was corrupted");

            let title = |rx: &Receiver<StreamMetadata>| {
                rx.recv_timeout(Duration::from_secs(3))
                    .ok()
                    .and_then(|m| m.title)
            };
            assert_eq!(title(&titles).as_deref(), Some("One"));
            assert_eq!(title(&titles).as_deref(), Some("Two"));
        }

        #[test]
        fn a_whole_file_ends_instead_of_looping() {
            // A plain HTTP file (a length, no ICY headers) is played once
            let server = TestServer::start();
            let audio = frame(600);
            server.route(
                "/clip.mp3",
                Route::new(audio.clone()).header("Content-Type", "audio/mpeg"),
            );
            let (reader, _rx) =
                IcyReader::open(&server.url("/clip.mp3"), None, GIVE_UP, StreamCancel::new())
                    .unwrap();

            let (played, end) = read_until_end(reader, Duration::from_secs(10));
            assert_eq!(played, audio);
            assert!(end.is_ok(), "a finished file is not an error: {end:?}");
            assert_eq!(server.hits("/clip.mp3"), 1);
            assert_no_more_requests(&server, "/clip.mp3");
        }

        #[test]
        fn a_live_stream_with_a_made_up_length_keeps_reconnecting() {
            // Some ICY servers send a Content-Length on a live stream; the
            // end of that body is a dropped connection, not the end
            let server = TestServer::start();
            let audio = frame(600);
            server.route(
                "/live",
                Route::new(audio.clone())
                    .header("Content-Type", "audio/mpeg")
                    .header("icy-name", "Test FM"),
            );
            let (mut reader, _rx) = IcyReader::open(
                &server.url("/live"),
                None,
                Duration::from_secs(30),
                StreamCancel::new(),
            )
            .unwrap();

            let (tx, rx) = bounded(1);
            thread::spawn(move || {
                let mut twice = vec![0u8; 1200];
                let _ = tx.send(reader.read_exact(&mut twice).map(|_| twice));
            });
            let played = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the station should have reconnected")
                .unwrap();
            assert_eq!(played, [audio.clone(), audio].concat());
            assert_eq!(server.hits("/live"), 2);
        }
    }
}
