//! The player's state as phones see it: `GET /v1/state` and each event of
//! `GET /v1/events`

use std::hash::{Hash, Hasher};

use serde::Serialize;

use radiotrope::audio::PlaybackState;
use radiotrope_app::data::schedule;
use radiotrope_app::data::types::url_to_id;

use crate::app::state::AppSnapshot;

/// The accent the window uses when the user picked none (Theme's
/// accent-default in ui/defaults.slint)
pub const DEFAULT_ACCENT: &str = "#f7931e";

/// The Accent Color dialog's swatches and their names (Defaults'
/// accent-swatches and accent-swatch-names in ui/defaults.slint)
pub const ACCENT_SWATCHES: [(&str, &str); 10] = [
    ("#f7931e", "Orange"),
    ("#ed5b00", "Dark orange"),
    ("#e62d42", "Red"),
    ("#d56199", "Pink"),
    ("#9141ac", "Purple"),
    ("#3584e4", "Blue"),
    ("#2190a4", "Teal"),
    ("#3a944a", "Green"),
    ("#c88800", "Gold"),
    ("#6f8396", "Slate"),
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct State {
    /// "stopped", "resolving", "playing" or "paused"
    pub playback: &'static str,
    pub station: Option<StationState>,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// 0 to 100
    pub volume: u8,
    pub muted: bool,
    /// How loud fades let the station play now, 0 to 1 (the volume bar's
    /// fill shrinks with it while the knob stays)
    pub fade: f32,
    pub stream: Option<StreamState>,
    /// The line under the station, as the window shows it
    pub status: String,
    /// The status is an error or warning (red in the window)
    pub status_is_error: bool,
    /// An alarm's station didn't start and the player beeps instead
    pub alarm_mode: bool,
    pub recording: Option<RecordingState>,
    pub sleep_timer: Option<SleepState>,
    /// The scheduled entry playing or recording now, e.g. "Play Jazz FM"
    pub scheduled_now: Option<String>,
    /// The next one, e.g. "Tomorrow 07:00: Play Jazz FM"
    pub next_scheduled: Option<String>,
    pub eq: EqState,
    /// "#rrggbb"
    pub accent: String,
    /// Changes whenever the favorites change: list them again
    pub favorites_rev: u64,
    /// Changes whenever the Scheduler entries change: list them again
    pub schedule_rev: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StationState {
    pub name: Option<String>,
    pub url: Option<String>,
    pub country: Option<String>,
    /// Where to get its logo from this player, e.g. "/v1/logos/1a2b..."
    pub logo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamState {
    pub codec: String,
    /// e.g. "ICY", "HLS"
    #[serde(rename = "type")]
    pub stream_type: String,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingState {
    /// The file's name, without its folder
    pub file: String,
    pub seconds: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SleepState {
    /// "HH:MM" local time
    pub until: String,
    pub seconds_left: i64,
    pub fade: bool,
    /// The length it was started with
    pub minutes: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EqState {
    pub enabled: bool,
    /// The preset picked, if the gains are still its own
    pub preset: Option<String>,
    /// The 10 bands' gains in dB
    pub gains: [f32; 10],
    pub preamp: f32,
}

impl State {
    pub fn from_snapshot(s: &AppSnapshot, favorites_rev: u64) -> Self {
        let now = chrono::Local::now();
        let playing = s.playback != PlaybackState::Stopped;
        let station = (s.station_name.is_some() || s.station_url.is_some()).then(|| StationState {
            name: s.station_name.clone(),
            url: s.station_url.clone(),
            country: s.station_country.clone(),
            logo: s
                .station_url
                .as_deref()
                .filter(|_| s.station_logo_url.is_some())
                .map(|url| format!("/v1/logos/{}", url_to_id(url))),
        });
        State {
            playback: playback_name(s),
            station,
            title: non_empty(&s.title),
            artist: non_empty(&s.artist),
            volume: (s.volume * 100.0).round().clamp(0.0, 100.0) as u8,
            muted: s.is_muted,
            fade: s.fade_gain,
            stream: (playing && !s.codec_name.is_empty()).then(|| StreamState {
                codec: s.codec_name.clone(),
                stream_type: s.stream_type.clone(),
                bitrate_kbps: s.bitrate,
                sample_rate: s.sample_rate,
                channels: s.channels,
            }),
            status: s.status_text.to_string(),
            status_is_error: s.is_error,
            alarm_mode: s.alarm_beep,
            recording: s.recording.as_ref().map(|r| RecordingState {
                file: r
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                seconds: r.duration.as_secs(),
                bytes: r.bytes,
            }),
            sleep_timer: s.sleep.as_ref().map(|t| SleepState {
                until: t.until.format("%H:%M").to_string(),
                seconds_left: (t.until - now).num_seconds().max(0),
                fade: t.fade,
                minutes: t.minutes,
            }),
            scheduled_now: s.scheduled.as_ref().map(|a| a.title.clone()),
            next_scheduled: s
                .next_run
                .as_ref()
                .map(|n| format!("{}: {}", schedule::when_label(&n.at, &now), n.title)),
            eq: EqState {
                enabled: s.eq_enabled,
                preset: s.eq_preset_name.clone(),
                gains: s.eq_gains,
                preamp: s.eq_preamp,
            },
            accent: s
                .accent_color
                .clone()
                .filter(|c| radiotrope_app::data::settings::parse_hex_rgb(c).is_some())
                .unwrap_or_else(|| DEFAULT_ACCENT.to_string()),
            favorites_rev,
            schedule_rev: schedule_rev(s),
        }
    }
}

/// A number that changes whenever the Scheduler entries do
fn schedule_rev(s: &AppSnapshot) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", s.schedule).hash(&mut hasher);
    hasher.finish()
}

fn playback_name(s: &AppSnapshot) -> &'static str {
    if s.is_resolving {
        return "resolving";
    }
    match s.playback {
        PlaybackState::Stopped => "stopped",
        PlaybackState::Playing => "playing",
        PlaybackState::Paused => "paused",
    }
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_swatches_are_the_windows() {
        let defaults = include_str!("../../ui/defaults.slint");
        let colors: Vec<&str> = ACCENT_SWATCHES.iter().map(|(c, _)| *c).collect();
        let names: Vec<String> = ACCENT_SWATCHES
            .iter()
            .map(|(_, n)| format!("\"{n}\""))
            .collect();
        let line = |start: &str| {
            let from = defaults.find(start).unwrap() + start.len();
            let to = from + defaults[from..].find("];").unwrap();
            defaults[from..to]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(line("accent-swatches: ["), colors.join(", ") + ",");
        assert_eq!(line("accent-swatch-names: ["), names.join(", ") + ",");
        assert!(defaults.contains(&format!("accent-default: {DEFAULT_ACCENT};")));
    }

    #[test]
    fn a_stopped_player_with_no_accent_set() {
        let state = State::from_snapshot(&AppSnapshot::default(), 3);
        assert_eq!(state.playback, "stopped");
        assert!(state.station.is_none());
        assert_eq!(state.accent, DEFAULT_ACCENT);
        assert_eq!(state.favorites_rev, 3);
    }

    #[test]
    fn a_playing_station_names_its_logo() {
        let s = AppSnapshot {
            playback: PlaybackState::Playing,
            station_name: Some("Jazz FM".into()),
            station_url: Some("http://jazz.test/stream".into()),
            station_logo_url: Some("http://jazz.test/logo.png".into()),
            codec_name: "MP3".into(),
            volume: 0.456,
            accent_color: Some("#3584e4".into()),
            ..Default::default()
        };
        let state = State::from_snapshot(&s, 0);
        let station = state.station.unwrap();
        assert_eq!(
            station.logo,
            Some(format!(
                "/v1/logos/{}",
                url_to_id("http://jazz.test/stream")
            ))
        );
        assert_eq!(state.volume, 46);
        assert_eq!(state.stream.unwrap().codec, "MP3");
        assert_eq!(state.accent, "#3584e4");
    }

    #[test]
    fn the_schedule_rev_follows_the_entries() {
        let a = AppSnapshot::default();
        let mut b = AppSnapshot::default();
        assert_eq!(schedule_rev(&a), schedule_rev(&b));
        use radiotrope_app::data::schedule::{Action, ClockTime, Days, End, Entry};
        b.schedule.push(Entry {
            id: 1,
            enabled: true,
            action: Action::Stop,
            station: None,
            start: ClockTime::parse("07:00").unwrap(),
            days: Days::from_flags([false; 7]),
            date: None,
            end: End::Never,
            volume: None,
            fade: false,
            fallback: true,
            armed_from: 0,
        });
        assert_ne!(schedule_rev(&a), schedule_rev(&b));
    }
}
