//! The output for people: a few labelled lines per station, or one

use crate::report::{channels, format_description, khz, Report, Status};

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
    out.push_str(&format!("{}  {title}\n", status(report.status, style)));
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
        if let Some(kbps) = audio.measured_kbps {
            text.push_str(&format!(" (received {kbps:.0} kbps)"));
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
        let now = match &report.metadata {
            Some(m) => format!(
                "{} ({})",
                song(m.title.as_deref(), m.artist.as_deref()),
                m.source
            ),
            None => "no song info".to_string(),
        };
        line("Now", now);
    }
    for problem in &report.problems {
        line("Problem", problem.message.clone());
    }
    let mut timing = Vec::new();
    if let Some(ms) = report.timing_ms.resolve {
        timing.push(format!("stream found in {}", secs(ms)));
    }
    if let Some(ms) = report.timing_ms.first_audio {
        timing.push(format!("first audio in {}", secs(ms)));
    }
    if !timing.is_empty() {
        line("Timing", timing.join(", "));
    }
    out
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
        status(report.status, style),
        parts.join(", ")
    )
}

fn status(status: Status, style: Style) -> String {
    let label = status.label();
    match style {
        Style::Plain => label.to_string(),
        Style::Colour => {
            let colour = match status {
                Status::Up => "32",
                Status::Degraded => "33",
                Status::Down => "31",
            };
            format!("\x1b[1;{colour}m{label}\x1b[0m")
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
             \x20 Format   MP3 128 kbps, 44.1 kHz stereo (received 131 kbps)\n\
             \x20 Audio    10.4 s of audio in 10.0 s, level -18 dBFS (peak -1)\n\
             \x20 Now      Daft Punk - One More Time (icy)\n\
             \x20 Timing   stream found in 0.41 s, first audio in 0.66 s\n"
        );
    }

    #[test]
    fn a_playing_station_in_one_line() {
        assert_eq!(
            quiet(&playing(), Style::Plain),
            "UP Rock FM: MP3 128 kbps, 44.1 kHz stereo, -18 dBFS, \"Daft Punk - One More Time\"\n"
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
            "DOWN http://radio.example/gone: HTTP 404 Not Found\n"
        );
    }

    #[test]
    fn colour_wraps_only_the_status_word() {
        let text = quiet(&playing(), Style::Colour);
        assert!(text.starts_with("\x1b[1;32mUP\x1b[0m Rock FM:"), "{text:?}");
    }
}
