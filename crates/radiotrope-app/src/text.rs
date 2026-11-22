//! Text helpers for the UI

/// Digit with the widest advance in the bundled Inter font (SemiBold and
/// Regular); "1" is the narrowest
const WIDEST_DIGIT: char = '4';

/// `text` with every ASCII digit replaced by the widest one, for sizing a
/// label whose digits change (a timer, a file size) so it doesn't shift
/// width on every tick. Inter's default digits are proportional and Slint
/// can't turn on its tabular figures.
pub fn steady_digits(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_digit() { WIDEST_DIGIT } else { c })
        .collect()
}

/// A value that isn't known yet, or doesn't apply
pub const MISSING: &str = "\u{2014}";

/// Joins the parts of a status line
pub const SEPARATOR: &str = " \u{b7} ";

/// One word for each playback state, the same in the window, the tray,
/// the Statistics dialog and the phone
pub mod status {
    /// A station picked and not heard yet
    pub const CONNECTING: &str = "Connecting\u{2026}";
    /// A recording waiting for its station to come back
    pub const RECONNECTING: &str = "Reconnecting\u{2026}";
    pub const PLAYING: &str = "Playing";
    pub const PAUSED: &str = "Paused";
    pub const STOPPED: &str = "Stopped";
    /// Nothing played yet
    pub const READY: &str = "Ready";
    /// The station stopped sending for a moment
    pub const WAITING_FOR_DATA: &str = "Waiting for data\u{2026}";
}

/// A running time: "0:12", "4:05", "1:02:03"
pub fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A length of time to the minute: "45 min", "2 h", "8 h 34 min", "<1 min"
/// under a minute, or empty for none at all
pub fn minutes(secs: u64) -> String {
    let mins = secs / 60;
    match mins {
        _ if secs == 0 => String::new(),
        0 => "<1 min".into(),
        1..=59 => format!("{mins} min"),
        _ if mins.is_multiple_of(60) => format!("{} h", mins / 60),
        _ => format!("{} h {} min", mins / 60, mins % 60),
    }
}

/// A sample rate in kHz, to one decimal at most: "44.1 kHz", "48 kHz"
pub fn sample_rate(hz: u32) -> String {
    let khz = (hz as f64 / 100.0).round() / 10.0;
    if khz.fract() == 0.0 {
        format!("{khz:.0} kHz")
    } else {
        format!("{khz:.1} kHz")
    }
}

/// A gain to `decimals` places with a real minus sign: "\u{2212}6.5 dB",
/// "+3.0 dB", and "0 dB" with no sign when it rounds to nothing
pub fn signed_db(db: f32, decimals: usize) -> String {
    let shown = format!("{:.*}", decimals, db.abs());
    if shown.trim_start_matches(['0', '.']).is_empty() {
        "0 dB".to_string()
    } else if db < 0.0 {
        format!("\u{2212}{shown} dB")
    } else {
        format!("+{shown} dB")
    }
}

/// Why a file or folder couldn't be used, in plain words: "permission
/// denied", without the system's "(os error 13)"
pub fn io_reason(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = match text.rfind(" (os error ") {
        Some(at) => &text[..at],
        None => &text,
    };
    lower_first(text)
}

/// The prefixes the engine and older code put before an error, which tell
/// a user nothing
const ERROR_PREFIXES: [&str; 6] = [
    "Error: ",
    "Stream error: ",
    "Audio error: ",
    "Decode error: ",
    "I/O error: ",
    "Timeout: ",
];

/// What the window says when a station can't play, from the engine's error
/// text (which agents and the logs get as it is): "Can't …" in plain words
pub fn playback_error(raw: &str) -> String {
    let mut text = raw.trim();
    while let Some(rest) = ERROR_PREFIXES.iter().find_map(|p| text.strip_prefix(p)) {
        text = rest.trim();
    }
    let lower = text.to_lowercase();
    if text.is_empty() {
        return "Can't play this station".into();
    }
    if lower.starts_with("can't") {
        return text.to_string();
    }
    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("did not start within")
    {
        return "Station not responding. Try again later.".into();
    }
    if lower.contains("web page") {
        return "Can't play this address: it's a web page, not a station".into();
    }
    if let Some(codec) = text
        .find("Unsupported codec: ")
        .map(|at| &text[at + "Unsupported codec: ".len()..])
    {
        return format!("Can't play this station: {codec} isn't supported");
    }
    format!("Can't play this station: {}", lower_first(text))
}

/// `text` with its first letter in lower case, unless the first word is an
/// acronym ("HTTP 404")
fn lower_first(text: &str) -> String {
    let first_word = text.split_whitespace().next().unwrap_or("");
    let acronym = first_word.chars().filter(|c| c.is_alphabetic()).count() > 1
        && first_word
            .chars()
            .filter(|c| c.is_alphabetic())
            .all(|c| c.is_uppercase());
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if !acronym => c.to_lowercase().chain(chars).collect(),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_has_no_leading_zero() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(12), "0:12");
        assert_eq!(clock(245), "4:05");
        assert_eq!(clock(3723), "1:02:03");
        assert_eq!(clock(36_000), "10:00:00");
    }

    #[test]
    fn minutes_use_min_and_h() {
        assert_eq!(minutes(0), "");
        assert_eq!(minutes(30), "<1 min");
        assert_eq!(minutes(45 * 60), "45 min");
        assert_eq!(minutes(2 * 3600), "2 h");
        assert_eq!(minutes(8 * 3600 + 34 * 60 + 59), "8 h 34 min");
    }

    #[test]
    fn sample_rate_in_khz() {
        assert_eq!(sample_rate(44_100), "44.1 kHz");
        assert_eq!(sample_rate(48_000), "48 kHz");
        assert_eq!(sample_rate(22_050), "22.1 kHz");
        assert_eq!(sample_rate(8_000), "8 kHz");
    }

    #[test]
    fn signed_db_uses_a_real_minus() {
        assert_eq!(signed_db(-6.5, 1), "\u{2212}6.5 dB");
        assert_eq!(signed_db(3.0, 1), "+3.0 dB");
        assert_eq!(signed_db(0.0, 1), "0 dB");
        assert_eq!(signed_db(-0.04, 1), "0 dB");
        assert_eq!(signed_db(-3.0, 0), "\u{2212}3 dB");
    }

    #[test]
    fn io_reason_is_plain() {
        let e = std::io::Error::other("Permission denied (os error 13)");
        assert_eq!(io_reason(&e), "permission denied");
    }

    #[test]
    fn playback_errors_lose_their_prefixes() {
        assert_eq!(
            playback_error("Error: Can't connect to radio.example"),
            "Can't connect to radio.example"
        );
        assert_eq!(
            playback_error("Stream error: No audio for 2 min: HTTP 404 Not Found"),
            "Can't play this station: no audio for 2 min: HTTP 404 Not Found"
        );
        assert_eq!(
            playback_error("Stream error: HTTP 404 Not Found"),
            "Can't play this station: HTTP 404 Not Found"
        );
        assert_eq!(
            playback_error("Connection timed out"),
            "Station not responding. Try again later."
        );
        assert_eq!(
            playback_error("Decode error: Unsupported codec: AC-3"),
            "Can't play this station: AC-3 isn't supported"
        );
        assert_eq!(
            playback_error("the server sent a web page instead of audio"),
            "Can't play this address: it's a web page, not a station"
        );
        assert_eq!(playback_error(""), "Can't play this station");
    }

    #[test]
    fn steady_digits_replaces_only_digits() {
        assert_eq!(steady_digits("Playing 01:17"), "Playing 44:44");
        assert_eq!(steady_digits("REC 0:01 · 71.1 KB"), "REC 4:44 · 44.4 KB");
        assert_eq!(steady_digits("Stopped"), "Stopped");
        assert_eq!(steady_digits(""), "");
    }
}
