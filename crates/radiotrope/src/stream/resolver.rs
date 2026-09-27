//! Stream resolver
//!
//! Orchestrates URL resolution: follows playlists, detects HLS,
//! and returns a `ResolvedStream` ready for the audio engine.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::error::{RadioError, Result};
use crate::stream::cancel::StreamCancel;
use crate::stream::hls::{HlsReader, HlsSegmentFormat};
use crate::stream::icy::IcyReader;
use crate::stream::playlist::{check_playlist_type, resolve_playlist, PlaylistCheck};
use crate::stream::types::{ResolvedStream, StreamInfo, StreamType};

/// Resolves a station URL into a playable stream
pub struct StreamResolver;

impl StreamResolver {
    /// Resolve a URL to a `ResolvedStream`.
    ///
    /// 1. Follow PLS/M3U playlist chains
    /// 2. If `.m3u8` → resolve HLS → HlsReader
    /// 3. Otherwise → IcyReader (works for ICY and non-ICY servers)
    pub fn resolve(url: &str) -> Result<ResolvedStream> {
        Self::resolve_cancellable(url, &StreamCancel::new())
    }

    /// [`StreamResolver::resolve`], stopped by `cancel`: cancelling it while
    /// resolving ends the resolve with [`RadioError::Cancelled`] (at once
    /// while waiting for the first audio, otherwise after the request in
    /// flight), and once resolved it stops the stream
    /// ([`ResolvedStream::cancel`] is `cancel`).
    pub fn resolve_cancellable(url: &str, cancel: &StreamCancel) -> Result<ResolvedStream> {
        let still_wanted = || match cancel.is_cancelled() {
            true => Err(RadioError::Cancelled),
            false => Ok(()),
        };
        still_wanted()?;
        let resolved_url = resolve_playlist(url, cancel)?;
        still_wanted()?;
        let playback_position = Arc::new(AtomicU64::new(0));

        match check_playlist_type(&resolved_url) {
            PlaylistCheck::Hls => {
                let media_url = crate::stream::hls::resolve_hls(&resolved_url, cancel)?;
                still_wanted()?;

                let (hls_reader, metadata_rx) = HlsReader::new_cancellable(
                    &media_url,
                    Some(playback_position.clone()),
                    cancel.clone(),
                )?;

                let format_hint = match hls_reader.detected_format {
                    HlsSegmentFormat::Fmp4 => Some("mp4".to_string()),
                    _ => Some("aac".to_string()),
                };

                let bytes_received = hls_reader.bytes_received.clone();
                let segments_downloaded = hls_reader.segments_downloaded.clone();

                Ok(ResolvedStream {
                    reader: Box::new(hls_reader),
                    metadata_rx: Some(metadata_rx),
                    info: StreamInfo {
                        original_url: url.to_string(),
                        resolved_url: media_url,
                        stream_type: StreamType::Hls,
                        format_hint,
                        content_type: None,
                        station_name: None,
                        bitrate: None,
                    },
                    bytes_received: Some(bytes_received),
                    segments_downloaded: Some(segments_downloaded),
                    playback_position: Some(playback_position),
                    cancel: cancel.clone(),
                })
            }
            _ => {
                let (icy_reader, metadata_rx) = IcyReader::new_cancellable(
                    &resolved_url,
                    Some(playback_position.clone()),
                    cancel.clone(),
                )?;

                let content_type = icy_reader.headers.content_type.clone();
                let station_name = icy_reader.headers.station_name.clone();
                let bitrate = icy_reader.headers.bitrate;
                let format_hint = Self::detect_format_hint(&resolved_url, content_type.as_deref());
                let bytes_received = icy_reader.bytes_received.clone();

                Ok(ResolvedStream {
                    reader: Box::new(icy_reader),
                    metadata_rx: Some(metadata_rx),
                    info: StreamInfo {
                        original_url: url.to_string(),
                        resolved_url,
                        stream_type: StreamType::Direct,
                        format_hint,
                        content_type,
                        station_name,
                        bitrate,
                    },
                    bytes_received: Some(bytes_received),
                    segments_downloaded: None,
                    playback_position: Some(playback_position),
                    cancel: cancel.clone(),
                })
            }
        }
    }

    /// Detect a format hint from content-type and/or URL extension
    pub fn detect_format_hint(url: &str, content_type: Option<&str>) -> Option<String> {
        // Content-type takes priority
        if let Some(ct) = content_type {
            let ct_lower = ct.to_lowercase();
            if ct_lower.contains("audio/mpeg") || ct_lower.contains("audio/mp3") {
                return Some("mp3".to_string());
            }
            if ct_lower.contains("audio/aac") || ct_lower.contains("audio/aacp") {
                return Some("aac".to_string());
            }
            if ct_lower.contains("audio/ogg") || ct_lower.contains("application/ogg") {
                return Some("ogg".to_string());
            }
            if ct_lower.contains("audio/flac") {
                return Some("flac".to_string());
            }
            if ct_lower.contains("audio/opus") {
                return Some("opus".to_string());
            }
            if ct_lower.contains("audio/x-mpegurl")
                || ct_lower.contains("application/vnd.apple.mpegurl")
            {
                return Some("m3u8".to_string());
            }
        }

        // Fallback to URL extension
        let lower = url.to_lowercase();
        let path = lower.split('?').next().unwrap_or(&lower);
        if let Some(ext) = path.rsplit('.').next() {
            match ext {
                "mp3" => return Some("mp3".to_string()),
                "aac" | "adts" => return Some("aac".to_string()),
                "ogg" | "oga" => return Some("ogg".to_string()),
                "opus" => return Some("opus".to_string()),
                "flac" => return Some("flac".to_string()),
                "m4a" | "mp4" => return Some("mp4".to_string()),
                _ => {}
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- detect_format_hint ---

    #[test]
    fn hint_from_content_type_mpeg() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", Some("audio/mpeg")),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_from_content_type_mp3() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", Some("audio/mp3")),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_from_content_type_aac() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", Some("audio/aac")),
            Some("aac".to_string())
        );
    }

    #[test]
    fn hint_from_content_type_aacp() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", Some("audio/aacp")),
            Some("aac".to_string())
        );
    }

    #[test]
    fn hint_from_content_type_ogg() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", Some("audio/ogg")),
            Some("ogg".to_string())
        );
    }

    #[test]
    fn hint_from_content_type_application_ogg() {
        assert_eq!(
            StreamResolver::detect_format_hint(
                "http://example.com/stream",
                Some("application/ogg")
            ),
            Some("ogg".to_string())
        );
    }

    #[test]
    fn hint_from_content_type_flac() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", Some("audio/flac")),
            Some("flac".to_string())
        );
    }

    #[test]
    fn hint_from_content_type_opus() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", Some("audio/opus")),
            Some("opus".to_string())
        );
    }

    #[test]
    fn hint_from_url_mp3() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.mp3", None),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_from_url_aac() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.aac", None),
            Some("aac".to_string())
        );
    }

    #[test]
    fn hint_from_url_ogg() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.ogg", None),
            Some("ogg".to_string())
        );
    }

    #[test]
    fn hint_from_url_flac() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.flac", None),
            Some("flac".to_string())
        );
    }

    #[test]
    fn hint_from_url_m4a() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.m4a", None),
            Some("mp4".to_string())
        );
    }

    #[test]
    fn hint_from_url_with_query() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.mp3?sid=1", None),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_content_type_overrides_url() {
        // Content-type says AAC but URL says .mp3 → AAC wins
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.mp3", Some("audio/aac")),
            Some("aac".to_string())
        );
    }

    #[test]
    fn hint_unknown_returns_none() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", None),
            None
        );
    }

    #[test]
    fn hint_unknown_content_type_falls_to_url() {
        assert_eq!(
            StreamResolver::detect_format_hint(
                "http://example.com/stream.mp3",
                Some("application/octet-stream")
            ),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_case_insensitive_content_type() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", Some("Audio/MPEG")),
            Some("mp3".to_string())
        );
    }

    // --- Content-type edge cases ---

    #[test]
    fn hint_content_type_with_charset() {
        assert_eq!(
            StreamResolver::detect_format_hint(
                "http://example.com/stream",
                Some("audio/mpeg; charset=utf-8")
            ),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_content_type_with_parameters() {
        assert_eq!(
            StreamResolver::detect_format_hint(
                "http://example.com/stream",
                Some("audio/aac;level=3")
            ),
            Some("aac".to_string())
        );
    }

    #[test]
    fn hint_empty_content_type() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.mp3", Some("")),
            Some("mp3".to_string()) // falls through to URL
        );
    }

    #[test]
    fn hint_content_type_x_mpegurl() {
        assert_eq!(
            StreamResolver::detect_format_hint(
                "http://example.com/stream",
                Some("audio/x-mpegurl")
            ),
            Some("m3u8".to_string())
        );
    }

    #[test]
    fn hint_content_type_apple_mpegurl() {
        assert_eq!(
            StreamResolver::detect_format_hint(
                "http://example.com/stream",
                Some("application/vnd.apple.mpegurl")
            ),
            Some("m3u8".to_string())
        );
    }

    // --- URL extension edge cases ---

    #[test]
    fn hint_from_url_adts() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.adts", None),
            Some("aac".to_string())
        );
    }

    #[test]
    fn hint_from_url_oga() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.oga", None),
            Some("ogg".to_string())
        );
    }

    #[test]
    fn hint_from_url_opus() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.opus", None),
            Some("opus".to_string())
        );
    }

    #[test]
    fn hint_from_url_mp4() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.mp4", None),
            Some("mp4".to_string())
        );
    }

    #[test]
    fn hint_unknown_extension() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.wav", None),
            None
        );
    }

    #[test]
    fn hint_no_extension_no_content_type() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream", None),
            None
        );
    }

    #[test]
    fn hint_double_extension_takes_last() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/file.tar.mp3", None),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_url_with_fragment() {
        // Fragment is NOT stripped by the '?' split, so .mp3#section → ext = "mp3#section" → no match
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.mp3#section", None),
            None
        );
    }

    #[test]
    fn hint_url_only_extension() {
        assert_eq!(
            StreamResolver::detect_format_hint(".mp3", None),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_url_with_port() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com:8000/stream.mp3", None),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_url_case_insensitive() {
        assert_eq!(
            StreamResolver::detect_format_hint("http://example.com/stream.MP3", None),
            Some("mp3".to_string())
        );
    }

    #[test]
    fn hint_url_aac_with_complex_query() {
        assert_eq!(
            StreamResolver::detect_format_hint(
                "http://example.com/stream.aac?token=abc&quality=high",
                None
            ),
            Some("aac".to_string())
        );
    }

    // --- Cancelling ---

    mod cancel {
        use super::*;
        use crate::stream::test_server::{Route, TestServer};
        use std::thread;
        use std::time::{Duration, Instant};

        /// A live stream that sends its headers and then nothing
        fn silent_station() -> Route {
            Route::new(Vec::new())
                .header("Content-Type", "audio/mpeg")
                .without_length()
                .stall()
        }

        /// Resolve `url` on a thread and cancel it once `waiting` holds.
        /// Returns the result and how long the resolve ran after the cancel.
        fn resolve_then_cancel(
            url: String,
            waiting: impl Fn() -> bool,
        ) -> (Result<ResolvedStream>, Duration) {
            let cancel = StreamCancel::new();
            let (tx, rx) = crossbeam_channel::bounded(1);
            let resolve_cancel = cancel.clone();
            thread::spawn(move || {
                let result = StreamResolver::resolve_cancellable(&url, &resolve_cancel);
                let _ = tx.send((result, Instant::now()));
            });
            let deadline = Instant::now() + Duration::from_secs(3);
            while !waiting() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(waiting(), "the resolve never got to the wait");
            // Let the waiting request's response arrive
            thread::sleep(Duration::from_millis(100));
            let cancelled_at = Instant::now();
            cancel.cancel();
            let (result, ended_at) = rx
                .recv_timeout(Duration::from_secs(3))
                .expect("the resolve must end once cancelled");
            (result, ended_at.saturating_duration_since(cancelled_at))
        }

        #[test]
        fn a_cancelled_resolve_makes_no_request() {
            let server = TestServer::start();
            server.route("/live.pls", Route::new("[playlist]\nFile1=/live\n"));
            let cancel = StreamCancel::new();
            cancel.cancel();
            let result = StreamResolver::resolve_cancellable(&server.url("/live.pls"), &cancel);
            assert!(matches!(result, Err(RadioError::Cancelled)));
            assert_eq!(server.hits("/live.pls"), 0);
        }

        #[test]
        fn cancelling_while_a_station_is_silent_ends_the_resolve_at_once() {
            let server = TestServer::start();
            server.route("/live", silent_station());
            let (result, took) =
                resolve_then_cancel(server.url("/live"), || server.hits("/live") > 0);
            assert!(matches!(result, Err(RadioError::Cancelled)));
            assert!(took < Duration::from_millis(200), "took {took:?}");
        }

        #[test]
        fn cancelling_while_waiting_for_the_first_hls_segment_ends_the_resolve_at_once() {
            let server = TestServer::start();
            server.route(
                "/live.m3u8",
                Route::new(
                    "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:1\n\
                     #EXTINF:2.0,\nseg1.aac\n",
                ),
            );
            server.route("/seg1.aac", silent_station());
            let (result, took) =
                resolve_then_cancel(server.url("/live.m3u8"), || server.hits("/seg1.aac") > 0);
            assert!(matches!(result, Err(RadioError::Cancelled)));
            assert!(took < Duration::from_millis(200), "took {took:?}");
        }

        #[test]
        fn the_resolved_stream_carries_the_cancel() {
            let server = TestServer::start();
            server.route(
                "/live",
                Route::new(vec![0x55; 4096])
                    .header("Content-Type", "audio/mpeg")
                    .without_length()
                    .stall(),
            );
            let cancel = StreamCancel::new();
            let resolved =
                StreamResolver::resolve_cancellable(&server.url("/live"), &cancel).unwrap();
            assert!(!resolved.cancel.is_cancelled());
            // Dropping a stream nobody plays (a stale resolve) stops it
            drop(resolved);
            assert!(cancel.is_cancelled());
        }
    }
}
