//! ICY stream reader
//!
//! Connects to Icecast/Shoutcast streams, extracts ICY metadata,
//! and provides a Read+Seek interface for the audio engine.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::config::network::{READ_TIMEOUT_SECS, USER_AGENT};
use crate::config::timeouts::CONNECT_TIMEOUT_SECS;
use crate::error::{RadioError, Result};
use crate::stream::metadata::{extract_icy_title, StreamMetadata};

use super::backoff_sleep;

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
    stop_flag: Arc<AtomicBool>,
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
    /// Returns the reader and a channel that receives metadata updates.
    pub fn new(url: &str) -> Result<(Self, Receiver<StreamMetadata>)> {
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

        // Channels for audio data and metadata
        let (audio_tx, audio_rx) = bounded::<Vec<u8>>(AUDIO_CHANNEL_BOUND);
        let (metadata_tx, metadata_rx) = crossbeam_channel::unbounded::<StreamMetadata>();

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let bytes_received = Arc::new(AtomicU64::new(0));
        let bytes_clone = bytes_received.clone();

        let url_owned = url.to_string();
        let handle = thread::spawn(move || {
            read_icy_stream(
                &url_owned,
                response,
                metaint,
                metadata_tx,
                audio_tx,
                stop_clone,
                bytes_clone,
            );
        });

        // Wait for initial data
        let initial_data = audio_rx
            .recv_timeout(Duration::from_secs(READ_TIMEOUT_SECS))
            .map_err(|_| RadioError::Timeout("Timeout waiting for stream data".to_string()))?;

        Ok((
            Self {
                current_chunk: initial_data,
                chunk_pos: 0,
                receiver: audio_rx,
                stop_flag,
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
    ) -> (Self, Arc<AtomicBool>) {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        (
            Self {
                current_chunk: initial_data,
                chunk_pos: 0,
                receiver,
                stop_flag,
                _handle: None,
                headers: IcyHeaders {
                    metaint: 0,
                    station_name: None,
                    content_type: None,
                    bitrate: None,
                },
                bytes_received: Arc::new(AtomicU64::new(0)),
            },
            stop_clone,
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
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "ICY stream ended",
                    ));
                }
            }

            // Blocking wait — loop on timeout while background thread
            // is still alive (it may be reconnecting with backoff).
            // Only give up on channel disconnect (thread exited) or stop flag.
            match self
                .receiver
                .recv_timeout(Duration::from_secs(READ_TIMEOUT_SECS))
            {
                Ok(chunk) => {
                    self.current_chunk = chunk;
                    self.chunk_pos = 0;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if self.stop_flag.load(Ordering::Relaxed) {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "ICY stream stopped",
                        ));
                    }
                    // Background thread still alive, keep waiting
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "ICY stream ended",
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
        self.stop_flag.store(true, Ordering::SeqCst);
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

/// Background thread function to read from ICY stream and extract metadata.
/// Reconnects with exponential backoff on network errors.
fn read_icy_stream(
    url: &str,
    mut response: reqwest::blocking::Response,
    metaint: usize,
    metadata_tx: Sender<StreamMetadata>,
    audio_tx: Sender<Vec<u8>>,
    stop_flag: Arc<AtomicBool>,
    bytes_received: Arc<AtomicU64>,
) {
    let mut bytes_until_meta = metaint;
    let mut last_title = String::new();
    let mut chunk_buffer = vec![0u8; 8192];
    let mut consecutive_failures: u32 = 0;

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            return;
        }

        let read_result = if metaint == 0 {
            read_chunk_no_meta(&mut response, &mut chunk_buffer, &audio_tx, &bytes_received)
        } else {
            read_chunk_with_meta(
                &mut response,
                &mut chunk_buffer,
                &mut bytes_until_meta,
                metaint,
                &mut last_title,
                &metadata_tx,
                &audio_tx,
                &bytes_received,
            )
        };

        match read_result {
            ReadResult::Ok => {
                consecutive_failures = 0;
            }
            ReadResult::Eof | ReadResult::Error => {
                // Network error or server closed connection.
                // Keep retrying with exponential backoff until reconnected or stopped.
                loop {
                    consecutive_failures += 1;
                    if stop_flag.load(Ordering::SeqCst) {
                        return;
                    }
                    if !backoff_sleep(consecutive_failures, &stop_flag) {
                        return; // Stopped during sleep
                    }
                    if try_reconnect(
                        url,
                        &mut response,
                        metaint,
                        &mut bytes_until_meta,
                        &mut consecutive_failures,
                        &stop_flag,
                    ) {
                        break; // Reconnected — resume main read loop
                    }
                }
            }
            ReadResult::ChannelClosed => return,
        }
    }
}

enum ReadResult {
    Ok,
    Eof,
    Error,
    ChannelClosed,
}

/// Read one chunk without ICY metadata
fn read_chunk_no_meta(
    response: &mut reqwest::blocking::Response,
    chunk_buffer: &mut [u8],
    audio_tx: &Sender<Vec<u8>>,
    bytes_received: &Arc<AtomicU64>,
) -> ReadResult {
    match response.read(chunk_buffer) {
        Ok(0) => ReadResult::Eof,
        Ok(n) => {
            bytes_received.fetch_add(n as u64, Ordering::Relaxed);
            if audio_tx.send(chunk_buffer[..n].to_vec()).is_err() {
                ReadResult::ChannelClosed
            } else {
                ReadResult::Ok
            }
        }
        Err(_) => ReadResult::Error,
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
    metadata_tx: &Sender<StreamMetadata>,
    audio_tx: &Sender<Vec<u8>>,
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
                if audio_tx.send(chunk_buffer[..n].to_vec()).is_err() {
                    return ReadResult::ChannelClosed;
                }
                *bytes_until_meta -= n;
            }
            Err(_) => return ReadResult::Error,
        }
    }

    if *bytes_until_meta == 0 {
        // Read metadata length byte
        let mut len_byte = [0u8; 1];
        if response.read_exact(&mut len_byte).is_err() {
            return ReadResult::Error;
        }

        let meta_len = len_byte[0] as usize * 16;
        if meta_len > 0 {
            let mut meta_buf = vec![0u8; meta_len];
            if response.read_exact(&mut meta_buf).is_err() {
                return ReadResult::Error;
            }

            if let Some(title) = extract_icy_title(&meta_buf) {
                if *title != *last_title {
                    *last_title = title.clone();
                    let metadata = StreamMetadata::from_icy_title(&title);
                    let _ = metadata_tx.send(metadata);
                }
            }
        }

        *bytes_until_meta = metaint;
    }

    ReadResult::Ok
}

/// Attempt to reconnect to the ICY stream.
/// Backoff sleep is handled by the caller via `backoff_sleep()`.
/// Returns true if reconnection succeeded, false if we should retry.
fn try_reconnect(
    url: &str,
    response: &mut reqwest::blocking::Response,
    metaint: usize,
    bytes_until_meta: &mut usize,
    consecutive_failures: &mut u32,
    stop_flag: &Arc<AtomicBool>,
) -> bool {
    if stop_flag.load(Ordering::SeqCst) {
        return false;
    }

    let client = match reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(READ_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    match client.get(url).header("Icy-MetaData", "1").send() {
        Ok(resp) if resp.status().is_success() => {
            *response = resp;
            *bytes_until_meta = metaint;
            *consecutive_failures = 0;
            true
        }
        _ => {
            // Reconnect failed — caller will increment failures and retry
            false
        }
    }
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
        reader.read(&mut buf).unwrap();
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
        reader.read(&mut buf).unwrap();

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
    fn drop_sets_stop_flag() {
        let (_tx, rx) = bounded(8);
        let stop_flag;
        {
            let (reader, stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
            stop_flag = stop.clone();
            assert!(!stop_flag.load(Ordering::SeqCst));
            drop(reader);
        }
        assert!(stop_flag.load(Ordering::SeqCst));
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
        let pos = reader.seek(SeekFrom::Current(0)).unwrap();
        assert_eq!(pos, 2);

        let mut buf = [0u8; 1];
        reader.read(&mut buf).unwrap();
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
        reader.read(&mut buf).unwrap();
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
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, [1, 2]);

        tx.send(vec![3, 4]).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, [3, 4]);

        tx.send(vec![5, 6]).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, [5, 6]);
    }

    // --- Drop behavior ---

    #[test]
    fn drop_with_pending_channel_data() {
        let (tx, rx) = bounded(8);
        let stop_flag;
        {
            let (reader, stop) = IcyReader::from_test_channel(rx, vec![1, 2, 3]);
            stop_flag = stop.clone();
            tx.send(vec![4, 5, 6]).unwrap();
            drop(reader);
        }
        assert!(stop_flag.load(Ordering::SeqCst));
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
}
