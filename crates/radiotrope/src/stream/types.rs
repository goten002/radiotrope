//! Stream types
//!
//! Core types for stream resolution and handling.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crossbeam_channel::Receiver;

use crate::audio::types::ReadSeek;
use crate::stream::metadata::StreamMetadata;

/// Type of resolved stream
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    Direct,
    Hls,
}

/// Information about a resolved stream
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub original_url: String,
    pub resolved_url: String,
    pub stream_type: StreamType,
    pub format_hint: Option<String>,
    pub content_type: Option<String>,
    pub station_name: Option<String>,
    pub bitrate: Option<u32>,
}

/// A fully resolved stream ready for the audio engine
pub struct ResolvedStream {
    pub reader: Box<dyn ReadSeek>,
    pub metadata_rx: Option<Receiver<StreamMetadata>>,
    pub info: StreamInfo,
    /// Atomic counter tracking bytes received from the network
    pub bytes_received: Option<Arc<AtomicU64>>,
    /// Atomic counter tracking HLS segments downloaded (None for non-HLS)
    pub segments_downloaded: Option<Arc<AtomicU64>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // --- StreamType ---

    #[test]
    fn stream_type_equality() {
        assert_eq!(StreamType::Direct, StreamType::Direct);
        assert_eq!(StreamType::Hls, StreamType::Hls);
        assert_ne!(StreamType::Direct, StreamType::Hls);
    }

    #[test]
    fn stream_type_debug() {
        assert_eq!(format!("{:?}", StreamType::Direct), "Direct");
        assert_eq!(format!("{:?}", StreamType::Hls), "Hls");
    }

    #[test]
    fn stream_type_clone() {
        let t = StreamType::Direct;
        let cloned = t;
        assert_eq!(t, cloned);
    }

    // --- StreamInfo ---

    #[test]
    fn stream_info_clone() {
        let info = StreamInfo {
            original_url: "http://example.com/stream".to_string(),
            resolved_url: "http://example.com/actual".to_string(),
            stream_type: StreamType::Direct,
            format_hint: Some("mp3".to_string()),
            content_type: Some("audio/mpeg".to_string()),
            station_name: Some("Test FM".to_string()),
            bitrate: None,
        };
        let cloned = info.clone();
        assert_eq!(cloned.original_url, "http://example.com/stream");
        assert_eq!(cloned.resolved_url, "http://example.com/actual");
        assert_eq!(cloned.stream_type, StreamType::Direct);
        assert_eq!(cloned.format_hint, Some("mp3".to_string()));
        assert_eq!(cloned.content_type, Some("audio/mpeg".to_string()));
        assert_eq!(cloned.station_name, Some("Test FM".to_string()));
    }

    #[test]
    fn stream_info_debug() {
        let info = StreamInfo {
            original_url: "http://test.com".to_string(),
            resolved_url: "http://test.com".to_string(),
            stream_type: StreamType::Hls,
            format_hint: None,
            content_type: None,
            station_name: None,
            bitrate: None,
        };
        let debug = format!("{:?}", info);
        assert!(debug.contains("Hls"));
        assert!(debug.contains("test.com"));
    }

    #[test]
    fn stream_info_with_all_none_optionals() {
        let info = StreamInfo {
            original_url: String::new(),
            resolved_url: String::new(),
            stream_type: StreamType::Direct,
            format_hint: None,
            content_type: None,
            station_name: None,
            bitrate: None,
        };
        assert!(info.format_hint.is_none());
        assert!(info.content_type.is_none());
        assert!(info.station_name.is_none());
    }

    // --- ResolvedStream ---

    #[test]
    fn resolved_stream_construction_with_cursor() {
        let cursor = Cursor::new(vec![1u8, 2, 3, 4]);
        let stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: None,
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: "http://test.com/stream".to_string(),
                resolved_url: "http://test.com/stream".to_string(),
                stream_type: StreamType::Direct,
                format_hint: Some("mp3".to_string()),
                content_type: None,
                station_name: None,
                bitrate: None,
            },
        };
        assert!(stream.metadata_rx.is_none());
        assert_eq!(stream.info.stream_type, StreamType::Direct);
    }

    #[test]
    fn resolved_stream_with_metadata_channel() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let cursor = Cursor::new(vec![0u8; 10]);
        let stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: Some(rx),
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: "http://test.com".to_string(),
                resolved_url: "http://test.com".to_string(),
                stream_type: StreamType::Direct,
                format_hint: None,
                content_type: None,
                station_name: None,
                bitrate: None,
            },
        };
        assert!(stream.metadata_rx.is_some());

        // Send metadata through channel
        use crate::stream::metadata::MetadataSource;
        tx.send(StreamMetadata {
            title: Some("Test".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        })
        .unwrap();

        let meta = stream.metadata_rx.unwrap().recv().unwrap();
        assert_eq!(meta.title, Some("Test".to_string()));
    }

    #[test]
    fn resolved_stream_reader_is_readable() {
        use std::io::Read;
        let data = vec![10u8, 20, 30];
        let cursor = Cursor::new(data);
        let mut stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: None,
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: String::new(),
                resolved_url: String::new(),
                stream_type: StreamType::Direct,
                format_hint: None,
                content_type: None,
                station_name: None,
                bitrate: None,
            },
        };
        let mut buf = [0u8; 3];
        let n = stream.reader.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf, [10, 20, 30]);
    }

    #[test]
    fn resolved_stream_hls_type() {
        let cursor = Cursor::new(Vec::<u8>::new());
        let stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: None,
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: "http://example.com/live.m3u8".to_string(),
                resolved_url: "http://example.com/media.m3u8".to_string(),
                stream_type: StreamType::Hls,
                format_hint: Some("aac".to_string()),
                content_type: Some("application/vnd.apple.mpegurl".to_string()),
                station_name: Some("HLS Radio".to_string()),
                bitrate: None,
            },
        };
        assert_eq!(stream.info.stream_type, StreamType::Hls);
        assert_eq!(stream.info.format_hint, Some("aac".to_string()));
    }

    // --- Reader seek behavior ---

    #[test]
    fn resolved_stream_reader_is_seekable() {
        use std::io::{Read, Seek, SeekFrom};
        let data = vec![10u8, 20, 30, 40, 50];
        let cursor = Cursor::new(data);
        let mut stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: None,
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: String::new(),
                resolved_url: String::new(),
                stream_type: StreamType::Direct,
                format_hint: None,
                content_type: None,
                station_name: None,
                bitrate: None,
            },
        };

        // Read first 3 bytes
        let mut buf = [0u8; 3];
        stream.reader.read(&mut buf).unwrap();
        assert_eq!(buf, [10, 20, 30]);

        // Seek back to start
        let pos = stream.reader.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(pos, 0);

        // Re-read
        stream.reader.read(&mut buf).unwrap();
        assert_eq!(buf, [10, 20, 30]);
    }

    #[test]
    fn resolved_stream_reader_eof_returns_zero() {
        use std::io::Read;
        let cursor = Cursor::new(vec![1u8, 2]);
        let mut stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: None,
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: String::new(),
                resolved_url: String::new(),
                stream_type: StreamType::Direct,
                format_hint: None,
                content_type: None,
                station_name: None,
                bitrate: None,
            },
        };
        let mut buf = [0u8; 10];
        let n = stream.reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        // Read at EOF
        let n = stream.reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn resolved_stream_reader_zero_length_read() {
        use std::io::Read;
        let cursor = Cursor::new(vec![1u8, 2, 3]);
        let mut stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: None,
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: String::new(),
                resolved_url: String::new(),
                stream_type: StreamType::Direct,
                format_hint: None,
                content_type: None,
                station_name: None,
                bitrate: None,
            },
        };
        let mut buf = [0u8; 0];
        let n = stream.reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    // --- Metadata channel behavior ---

    #[test]
    fn resolved_stream_multiple_metadata_sends() {
        use crate::stream::metadata::MetadataSource;
        let (tx, rx) = crossbeam_channel::unbounded();
        let cursor = Cursor::new(Vec::<u8>::new());
        let stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: Some(rx),
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: String::new(),
                resolved_url: String::new(),
                stream_type: StreamType::Direct,
                format_hint: None,
                content_type: None,
                station_name: None,
                bitrate: None,
            },
        };

        // Send multiple metadata updates
        for i in 0..5 {
            tx.send(StreamMetadata {
                title: Some(format!("Song {}", i)),
                artist: Some(format!("Artist {}", i)),
                source: MetadataSource::Icy,
            })
            .unwrap();
        }

        let rx = stream.metadata_rx.unwrap();
        for i in 0..5 {
            let meta = rx.recv().unwrap();
            assert_eq!(meta.title, Some(format!("Song {}", i)));
            assert_eq!(meta.artist, Some(format!("Artist {}", i)));
        }
    }

    #[test]
    fn resolved_stream_metadata_channel_disconnected() {
        use crate::stream::metadata::MetadataSource;
        let (tx, rx) = crossbeam_channel::unbounded();
        let cursor = Cursor::new(Vec::<u8>::new());
        let stream = ResolvedStream {
            reader: Box::new(cursor),
            metadata_rx: Some(rx),
            bytes_received: None,
            segments_downloaded: None,
            info: StreamInfo {
                original_url: String::new(),
                resolved_url: String::new(),
                stream_type: StreamType::Direct,
                format_hint: None,
                content_type: None,
                station_name: None,
                bitrate: None,
            },
        };

        tx.send(StreamMetadata {
            title: Some("Last".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        })
        .unwrap();
        drop(tx); // Disconnect sender

        let rx = stream.metadata_rx.unwrap();
        let meta = rx.recv().unwrap();
        assert_eq!(meta.title, Some("Last".to_string()));

        // Next recv should fail (disconnected)
        assert!(rx.recv().is_err());
    }

    // --- StreamInfo edge cases ---

    #[test]
    fn stream_info_with_empty_strings() {
        let info = StreamInfo {
            original_url: String::new(),
            resolved_url: String::new(),
            stream_type: StreamType::Direct,
            format_hint: Some(String::new()),
            content_type: Some(String::new()),
            station_name: Some(String::new()),
            bitrate: None,
        };
        assert_eq!(info.format_hint, Some(String::new()));
        assert_eq!(info.content_type, Some(String::new()));
        assert_eq!(info.station_name, Some(String::new()));
    }

    #[test]
    fn stream_info_urls_can_differ() {
        let info = StreamInfo {
            original_url: "http://original.com/radio.pls".to_string(),
            resolved_url: "http://actual-stream.com:8000/live".to_string(),
            stream_type: StreamType::Direct,
            format_hint: Some("mp3".to_string()),
            content_type: None,
            station_name: None,
            bitrate: None,
        };
        assert_ne!(info.original_url, info.resolved_url);
    }
}
