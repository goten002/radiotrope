//! `GET /v1/stats`: the window's Statistics for the phone. Asked for
//! about once a second while the phone shows them, so they stay out of
//! the state the event stream sends on every change. What the filters do
//! (the SOUND group) is in that state already, under `filters`.

use hyper::{Response, StatusCode};
use serde::Serialize;

use radiotrope::audio::health::HealthState;
use radiotrope::audio::{PlaybackState, StreamStats};
use radiotrope::stream::StreamType;

use super::server::{json, Body, Shared};
use crate::app::state::AppSnapshot;

#[derive(Serialize, Debug, PartialEq)]
pub struct Stats {
    /// "stopped", "connecting", "ok", "stalled" or "failed"; the rest is
    /// `None` while stopped
    pub health: &'static str,
    pub playtime_secs: Option<u64>,
    pub codec: Option<String>,
    /// "HLS" or "ICY"
    pub stream_type: Option<&'static str>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub received_bytes: Option<u64>,
    /// HLS segments downloaded; `None` for other streams
    pub segments: Option<u64>,
    pub throughput_kbps: Option<u32>,
    pub buffer_bytes: Option<u64>,
    pub buffer_capacity_bytes: Option<u64>,
    pub frames: Option<u64>,
    pub decode_errors: Option<u64>,
    pub underruns: Option<u64>,
}

impl Stats {
    const STOPPED: Stats = Stats {
        health: "stopped",
        playtime_secs: None,
        codec: None,
        stream_type: None,
        bitrate_kbps: None,
        sample_rate: None,
        channels: None,
        received_bytes: None,
        segments: None,
        throughput_kbps: None,
        buffer_bytes: None,
        buffer_capacity_bytes: None,
        frames: None,
        decode_errors: None,
        underruns: None,
    };

    /// As the window's Statistics show them: blank with nothing playing
    /// or connecting
    pub fn of(snapshot: &AppSnapshot) -> Self {
        let live = snapshot.playback != PlaybackState::Stopped || snapshot.is_resolving;
        let Some(shared) = snapshot.stats.as_ref().filter(|_| live) else {
            return Self::STOPPED;
        };
        let s = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Self::from_stream(&s)
    }

    fn from_stream(s: &StreamStats) -> Self {
        let codec = s.codec_info.as_ref();
        Self {
            health: match s.health_state {
                HealthState::WaitingForAudio => "connecting",
                HealthState::Healthy => "ok",
                HealthState::Stalled => "stalled",
                HealthState::Failed(_) => "failed",
            },
            playtime_secs: s.play_started_at.map(|t| t.elapsed().as_secs()),
            codec: codec.map(|c| c.codec_name.clone()),
            stream_type: s.stream_type.map(|t| match t {
                StreamType::Hls => "HLS",
                StreamType::Direct => "ICY",
            }),
            bitrate_kbps: codec.and_then(|c| c.bitrate),
            sample_rate: codec.map(|c| c.sample_rate).filter(|&r| r > 0),
            channels: codec.map(|c| c.channels).filter(|&c| c > 0),
            received_bytes: Some(s.bytes_received),
            segments: Some(s.segments_downloaded).filter(|&n| n > 0),
            throughput_kbps: Some(s.throughput_kbps.round() as u32),
            buffer_bytes: Some(s.buffer_level_bytes as u64),
            buffer_capacity_bytes: Some(s.buffer_capacity_bytes as u64),
            frames: Some(s.frames_played),
            decode_errors: Some(s.decode_errors),
            underruns: Some(s.underrun_count as u64),
        }
    }
}

pub(super) fn stats(shared: &Shared) -> Response<Body> {
    json(StatusCode::OK, &Stats::of(&shared.control.snapshot()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use radiotrope::audio::CodecInfo;

    #[test]
    fn a_playing_stream() {
        let s = StreamStats {
            codec_info: Some(CodecInfo {
                codec_name: "AAC".into(),
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: None,
                bitrate: Some(128),
            }),
            stream_type: Some(StreamType::Hls),
            health_state: HealthState::Healthy,
            bytes_received: 2048,
            segments_downloaded: 3,
            throughput_kbps: 130.6,
            buffer_level_bytes: 512,
            buffer_capacity_bytes: 4096,
            frames_played: 99,
            ..Default::default()
        };
        let stats = Stats::from_stream(&s);
        assert_eq!(stats.health, "ok");
        assert_eq!(stats.codec.as_deref(), Some("AAC"));
        assert_eq!(stats.stream_type, Some("HLS"));
        assert_eq!(stats.bitrate_kbps, Some(128));
        assert_eq!(stats.channels, Some(2));
        assert_eq!(stats.segments, Some(3));
        assert_eq!(stats.throughput_kbps, Some(131));
        assert_eq!(stats.buffer_capacity_bytes, Some(4096));
        assert_eq!(stats.playtime_secs, None);
    }

    #[test]
    fn nothing_playing() {
        let snapshot = AppSnapshot::default();
        assert_eq!(Stats::of(&snapshot), Stats::STOPPED);
    }
}
