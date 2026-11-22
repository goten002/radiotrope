//! The output for people: a few labelled lines per station, or one

use crate::report::{channels, format_description, khz, ErrorCode, Report, Status};

/// Colours for the status word on a terminal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    Colour,
}

/// The labelled lines for one station, ending with a newline
pub fn full(report: &Report, style: Style) -> String {
    let mut out = String::new();
    let title = report.station_name().unwrap_or(&report.url);
    out.push_str(&format!("{}  {title}\n", status(report.status, style, 0)));
    let mut line = |label: &str, text: String| {
        out.push_str(&format!("  {label:<8} {text}\n"));
    };
    if report.station_name().is_some() {
        line("URL", report.url.clone());
    }
    if let Some(error) = &report.error {
        line("Error", error.message.clone());
    }
    if let Some(stream) = &report.stream {
        let kind = match stream.kind {
            "hls" => "HLS",
            _ => "HTTP",
        };
        let mut text = kind.to_string();
        if stream.resolved_url != report.url {
            text.push_str(&format!(": {}", stream.resolved_url));
        }
        if let Some(ct) = &stream.content_type {
            text.push_str(&format!(" ({ct})"));
        }
        line("Stream", text);
    }
    if let Some(audio) = &report.audio {
        let mut text = format_description(report, audio);
        if let Some(kbps) = measured_worth_showing(report, audio.measured_kbps) {
            text.push_str(&format!(" (measured {kbps:.0} kbps)"));
        }
        line("Format", text);
        if audio.decoded_s > 0.0 {
            let mut text = format!(
                "{:.1} s of audio in {:.1} s",
                audio.decoded_s, audio.listened_s
            );
            if let (Some(rms), Some(peak)) = (audio.rms_dbfs, audio.peak_dbfs) {
                text.push_str(&format!(", level {rms:.0} dBFS (peak {peak:.0})"));
            }
            line("Audio", text);
        }
    }
    if report.audio.is_some() {
        // Where the song info came from stays in the JSON
        let now = match &report.metadata {
            Some(m) => song(m.title.as_deref(), m.artist.as_deref()),
            None => "No song info".to_string(),
        };
        line("Now", now);
    }
    for problem in &report.problems {
        line("Problem", problem.message.clone());
    }
    let timing = match (report.timing_ms.resolve, report.timing_ms.first_audio) {
        // A direct stream starts as it is found: one time says it all
        (Some(found), Some(audio)) if audio.abs_diff(found) <= SAME_TIME_MS => {
            Some(format!("First audio in {}", secs(audio)))
        }
        (Some(found), Some(audio)) => Some(format!(
            "Stream found in {}, first audio in {}",
            secs(found),
            secs(audio)
        )),
        (Some(found), None) => Some(format!("Stream found in {}", secs(found))),
        (None, Some(audio)) => Some(format!("First audio in {}", secs(audio))),
        (None, None) => None,
    };
    if let Some(timing) = timing {
        line("Timing", timing);
    }
    out
}

/// Times closer than this are one time to a reader
const SAME_TIME_MS: u64 = 50;

/// The measured bitrate may differ this much from the advertised one before
/// it is worth a word
const KBPS_TOLERANCE: f64 = 0.15;

/// The measured bitrate, when the text should show it: when the server
/// advertises none, or one more than 15% off what really comes
fn measured_worth_showing(report: &Report, measured: Option<f64>) -> Option<f64> {
    let measured = measured?;
    match report.station.as_ref().and_then(|s| s.advertised_kbps) {
        Some(advertised) => {
            let advertised = f64::from(advertised);
            ((measured - advertised).abs() > advertised * KBPS_TOLERANCE).then_some(measured)
        }
        None => Some(measured),
    }
}

/// One line: "UP Rock FM: MP3 128 kbps, 44.1 kHz stereo, -18 dBFS, "Artist - Title""
pub fn quiet(report: &Report, style: Style) -> String {
    let title = report.station_name().unwrap_or(&report.url);
    let mut parts = Vec::new();
    match (&report.error, &report.audio) {
        (Some(error), _) => parts.push(error.message.clone()),
        (None, Some(audio)) => {
            let mut text = audio.codec.clone();
            if let Some(kbps) = report.station.as_ref().and_then(|s| s.advertised_kbps) {
                text.push_str(&format!(" {kbps} kbps"));
            }
            parts.push(text);
            parts.push(format!(
                "{} {}",
                khz(audio.sample_rate),
                channels(audio.channels)
            ));
            if let Some(rms) = audio.rms_dbfs {
                parts.push(format!("{rms:.0} dBFS"));
            }
            if let Some(m) = &report.metadata {
                parts.push(format!(
                    "\"{}\"",
                    song(m.title.as_deref(), m.artist.as_deref())
                ));
            }
            parts.extend(report.problems.iter().map(|p| p.message.clone()));
        }
        (None, None) => {}
    }
    format!(
        "{} {title}: {}\n",
        status(report.status, style, STATUS_WIDTH),
        parts.join(", ")
    )
}

/// The widest status word, DEGRADED: one-line reports pad to it so the
/// station names line up
const STATUS_WIDTH: usize = 8;

/// Counts for the closing line of a run with several stations
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Totals {
    pub up: usize,
    pub degraded: usize,
    pub down: usize,
    /// Of the down ones: addresses that aren't web addresses, a typo in the
    /// list rather than a station that is off the air
    pub invalid: usize,
}

impl Totals {
    pub fn add(&mut self, report: &Report) {
        match report.status {
            Status::Up => self.up += 1,
            Status::Degraded => self.degraded += 1,
            Status::Down => self.down += 1,
        }
        if report.error.as_ref().map(|e| e.code) == Some(ErrorCode::InvalidUrl) {
            self.invalid += 1;
        }
    }

    pub fn checked(&self) -> usize {
        self.up + self.degraded + self.down
    }

    /// "12 checked: 10 up, 1 degraded, 1 down (1 not a web address)", with a
    /// newline; counts of none are left out
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if self.up > 0 {
            parts.push(format!("{} up", self.up));
        }
        if self.degraded > 0 {
            parts.push(format!("{} degraded", self.degraded));
        }
        if self.down > 0 {
            let mut down = format!("{} down", self.down);
            if self.invalid > 0 {
                down.push_str(&format!(" ({} not a web address)", self.invalid));
            }
            parts.push(down);
        }
        format!("{} checked: {}\n", self.checked(), parts.join(", "))
    }
}

/// The status word, padded to `width` columns (the colour codes don't count)
fn status(status: Status, style: Style, width: usize) -> String {
    let label = status.label();
    let pad = " ".repeat(width.saturating_sub(label.len()));
    match style {
        Style::Plain => format!("{label}{pad}"),
        Style::Colour => {
            let colour = match status {
                Status::Up => "32",
                Status::Degraded => "33",
                Status::Down => "31",
            };
            format!("\x1b[1;{colour}m{label}\x1b[0m{pad}")
        }
    }
}

fn song(title: Option<&str>, artist: Option<&str>) -> String {
    match (artist, title) {
        (Some(artist), Some(title)) => format!("{artist} - {title}"),
        (Some(one), None) | (None, Some(one)) => one.to_string(),
        (None, None) => String::new(),
    }
}

fn secs(ms: u64) -> String {
    format!("{:.2} s", ms as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{
        Audio, ErrorCode, ErrorInfo, Metadata, Problem, ProblemCode, Station, StreamDetails,
    };

    fn playing() -> Report {
        let mut report = Report::new("http://radio.example/rock.pls", String::new());
        report.timing_ms.resolve = Some(412);
        report.timing_ms.first_audio = Some(655);
        report.stream = Some(StreamDetails {
            kind: "direct",
            resolved_url: "http://stream.example:8000/rock".to_string(),
            content_type: Some("audio/mpeg".to_string()),
        });
        report.station = Some(Station {
            name: Some("Rock FM".to_string()),
            advertised_kbps: Some(128),
        });
        report.audio = Some(Audio {
            codec: "MP3".to_string(),
            sample_rate: 44100,
            channels: 2,
            measured_kbps: Some(131.4),
            listened_s: 10.0,
            decoded_s: 10.4,
            rms_dbfs: Some(-18.2),
            peak_dbfs: Some(-0.9),
            ..Audio::default()
        });
        report.metadata = Some(Metadata {
            title: Some("One More Time".to_string()),
            artist: Some("Daft Punk".to_string()),
            source: "icy",
            changes: 0,
        });
        report.conclude();
        report
    }

    #[test]
    fn a_playing_station_in_full() {
        assert_eq!(
            full(&playing(), Style::Plain),
            "UP  Rock FM\n\
             \x20 URL      http://radio.example/rock.pls\n\
             \x20 Stream   HTTP: http://stream.example:8000/rock (audio/mpeg)\n\
             \x20 Format   MP3 128 kbps, 44.1 kHz stereo\n\
             \x20 Audio    10.4 s of audio in 10.0 s, level -18 dBFS (peak -1)\n\
             \x20 Now      Daft Punk - One More Time\n\
             \x20 Timing   Stream found in 0.41 s, first audio in 0.66 s\n"
        );
    }

    #[test]
    fn a_playing_station_in_one_line() {
        assert_eq!(
            quiet(&playing(), Style::Plain),
            "UP       Rock FM: MP3 128 kbps, 44.1 kHz stereo, -18 dBFS, \"Daft Punk - One More Time\"\n"
        );
    }

    #[test]
    fn problems_are_listed() {
        let mut report = playing();
        report.problems.push(Problem {
            code: ProblemCode::Silent,
            message: "No sound above -50 dBFS for the whole 10 s".to_string(),
        });
        report.conclude();
        let text = full(&report, Style::Plain);
        assert!(text.starts_with("DEGRADED  Rock FM\n"), "{text}");
        assert!(
            text.contains("  Problem  No sound above -50 dBFS for the whole 10 s\n"),
            "{text}"
        );
        assert!(quiet(&report, Style::Plain)
            .ends_with(", No sound above -50 dBFS for the whole 10 s\n"));
    }

    #[test]
    fn a_down_station_shows_why() {
        let mut report = Report::new("http://radio.example/gone", String::new());
        report.error = Some(ErrorInfo {
            code: ErrorCode::HttpStatus,
            http_status: Some(404),
            message: "HTTP 404 Not Found".to_string(),
        });
        report.conclude();
        assert_eq!(
            full(&report, Style::Plain),
            "DOWN  http://radio.example/gone\n  Error    HTTP 404 Not Found\n"
        );
        assert_eq!(
            quiet(&report, Style::Plain),
            "DOWN     http://radio.example/gone: HTTP 404 Not Found\n"
        );
    }

    #[test]
    fn colour_wraps_only_the_status_word() {
        let text = quiet(&playing(), Style::Colour);
        assert!(
            text.starts_with("\x1b[1;32mUP\x1b[0m       Rock FM:"),
            "{text:?}"
        );
        let text = full(&playing(), Style::Colour);
        assert!(
            text.starts_with("\x1b[1;32mUP\x1b[0m  Rock FM\n"),
            "{text:?}"
        );
    }

    #[test]
    fn the_measured_rate_shows_only_when_it_is_far_off() {
        let format_line = |report: &Report| {
            full(report, Style::Plain)
                .lines()
                .find(|l| l.contains("Format"))
                .unwrap()
                .to_string()
        };
        let mut report = playing();
        // 131 against 128 advertised is within 15%
        assert!(!format_line(&report).contains("measured"));
        report.audio.as_mut().unwrap().measured_kbps = Some(64.2);
        assert!(
            format_line(&report).ends_with("stereo (measured 64 kbps)"),
            "{}",
            format_line(&report)
        );
        report.station.as_mut().unwrap().advertised_kbps = None;
        assert!(format_line(&report).ends_with("MP3, 44.1 kHz stereo (measured 64 kbps)"));
    }

    #[test]
    fn close_times_are_one_time() {
        let mut report = playing();
        report.timing_ms.resolve = Some(70);
        report.timing_ms.first_audio = Some(90);
        let text = full(&report, Style::Plain);
        assert!(
            text.ends_with("  Timing   First audio in 0.09 s\n"),
            "{text}"
        );
    }

    #[test]
    fn no_song_info_is_said_plainly() {
        let mut report = playing();
        report.metadata = None;
        assert!(full(&report, Style::Plain).contains("  Now      No song info\n"));
    }

    #[test]
    fn totals_count_each_status_and_typos_apart() {
        let mut totals = Totals::default();
        totals.add(&playing());
        totals.add(&playing());
        let mut gone = Report::new("ftp://x/", String::new());
        gone.error = Some(ErrorInfo {
            code: ErrorCode::InvalidUrl,
            http_status: None,
            message: "x".to_string(),
        });
        gone.conclude();
        totals.add(&gone);
        assert_eq!(
            totals.line(),
            "3 checked: 2 up, 1 down (1 not a web address)\n"
        );

        let mut totals = Totals::default();
        totals.add(&playing());
        assert_eq!(totals.line(), "1 checked: 1 up\n");
    }
}
