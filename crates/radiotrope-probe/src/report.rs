//! What a check found, in the shape the JSON output takes

use serde::Serialize;

/// Bumped only when a field changes meaning; new fields don't bump it
pub const SCHEMA: u32 = 1;

/// The station's overall state
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Audio flows in real time and has sound in it
    Up,
    /// It plays, but something is wrong (see [`Report::problems`])
    Degraded,
    /// No audio (see [`Report::error`])
    Down,
}

impl Status {
    /// The process exit code, as monitoring tools read it (Nagios style).
    /// Several stations exit with the worst one.
    pub fn exit_code(self) -> i32 {
        match self {
            Status::Up => 0,
            Status::Degraded => 1,
            Status::Down => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Up => "UP",
            Status::Degraded => "DEGRADED",
            Status::Down => "DOWN",
        }
    }
}

/// The exit code when the check itself could not run (bad arguments, an
/// unreadable list of stations)
pub const EXIT_UNKNOWN: i32 = 3;

/// Why a station is down. The codes are fixed, so a scheduler can act on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Not an http or https address
    InvalidUrl,
    /// The host name doesn't exist
    Dns,
    /// The server refused or dropped the connection
    Connect,
    /// The secure connection failed (certificate, handshake)
    Tls,
    /// Nothing within the time allowed
    Timeout,
    /// The server answered with an HTTP error
    HttpStatus,
    /// A web page instead of a stream
    WebPage,
    /// A playlist with no stations in it
    EmptyPlaylist,
    /// An HLS stream we can't decrypt
    EncryptedHls,
    /// Audio in a format we can't decode
    UnsupportedCodec,
    /// Data we couldn't make audio of
    DecodeFailed,
    /// The stream opened but no audio came out
    NoAudio,
    /// Any other stream failure
    StreamFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    /// The HTTP status, for [`ErrorCode::HttpStatus`]
    pub http_status: Option<u16>,
    pub message: String,
}

/// What makes a playing station degraded
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemCode {
    /// No sound above the silence level for the whole listen
    Silent,
    /// Audio came slower than real time: a listener would have heard it stop
    Stalled,
    /// Less audio came than the time listened (it may not have stalled yet)
    Slow,
    /// Many frames couldn't be decoded
    DecodeErrors,
    /// The stream ended during the listen (a file, or a station going away)
    Ended,
}

#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    pub code: ProblemCode,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Timing {
    /// From the start until the stream answered (playlists followed)
    pub resolve: Option<u64>,
    /// From the start until the first audio was decoded
    pub first_audio: Option<u64>,
    /// The whole check
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamDetails {
    /// "direct" (Icecast, SHOUTcast or plain HTTP) or "hls"
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The stream the station's address led to, after any playlists
    pub resolved_url: String,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Station {
    /// From the server's `icy-name` header
    pub name: Option<String>,
    /// From the server's `icy-br` header
    pub advertised_kbps: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Audio {
    /// MP3, AAC, AAC+, Opus, Vorbis, FLAC…
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: Option<u32>,
    /// Network rate over the listen, including the server's start-up burst
    pub measured_kbps: Option<f64>,
    /// How long we listened
    pub listened_s: f64,
    /// How much audio came in that time
    pub decoded_s: f64,
    /// `decoded_s / listened_s`: above 1 when the server sends a burst first
    pub realtime_ratio: f64,
    pub frames: u64,
    pub decode_errors: u64,
    /// Times a player with a short buffer would have stopped to buffer
    pub stalls: u32,
    /// How long it would have been stopped in all
    pub stalled_s: f64,
    /// Average level over the listen, in dB below full scale
    pub rms_dbfs: Option<f64>,
    /// Loudest sample
    pub peak_dbfs: Option<f64>,
    /// Seconds below the silence level
    pub silent_s: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    /// "icy", "id3v2", "id3v1" or "hls_playlist"
    pub source: &'static str,
    /// Song changes seen during the listen
    pub changes: u32,
}

/// Everything one check found
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema: u32,
    pub url: String,
    /// When the check started, RFC 3339 in UTC
    pub checked_at: String,
    pub status: Status,
    /// On air: up, and still streaming at the end of the listen
    pub live: bool,
    /// One line about the result
    pub summary: String,
    pub timing_ms: Timing,
    pub stream: Option<StreamDetails>,
    pub station: Option<Station>,
    pub audio: Option<Audio>,
    pub metadata: Option<Metadata>,
    pub problems: Vec<Problem>,
    pub error: Option<ErrorInfo>,
}

impl Report {
    pub(crate) fn new(url: &str, checked_at: String) -> Self {
        Self {
            schema: SCHEMA,
            url: url.to_string(),
            checked_at,
            status: Status::Down,
            live: false,
            summary: String::new(),
            timing_ms: Timing::default(),
            stream: None,
            station: None,
            audio: None,
            metadata: None,
            problems: Vec::new(),
            error: None,
        }
    }

    /// The station's name, if the server sent one
    pub fn station_name(&self) -> Option<&str> {
        self.station.as_ref()?.name.as_deref()
    }

    /// Set the status, `live` and the summary from what was found
    pub(crate) fn conclude(&mut self) {
        self.status = if self.error.is_some() {
            Status::Down
        } else if self.problems.is_empty() {
            Status::Up
        } else {
            Status::Degraded
        };
        self.live = self.status == Status::Up;
        self.summary = match (&self.error, &self.audio) {
            (Some(error), _) => error.message.clone(),
            (None, Some(audio)) => self.format_line(audio),
            (None, None) => String::new(),
        };
    }

    /// "MP3 128 kbps, 44.1 kHz stereo, -18 dBFS"
    fn format_line(&self, audio: &Audio) -> String {
        let mut line = format_description(self, audio);
        if let Some(rms) = audio.rms_dbfs {
            line.push_str(&format!(", {rms:.0} dBFS"));
        }
        line
    }
}

/// "MP3 128 kbps, 44.1 kHz stereo"
pub(crate) fn format_description(report: &Report, audio: &Audio) -> String {
    let mut line = audio.codec.clone();
    if let Some(kbps) = report.station.as_ref().and_then(|s| s.advertised_kbps) {
        line.push_str(&format!(" {kbps} kbps"));
    }
    line.push_str(&format!(
        ", {} {}",
        khz(audio.sample_rate),
        channels(audio.channels)
    ));
    line
}

/// "44.1 kHz", "48 kHz"
pub(crate) fn khz(rate: u32) -> String {
    let khz = rate as f64 / 1000.0;
    if rate.is_multiple_of(1000) {
        format!("{khz:.0} kHz")
    } else {
        format!("{khz:.1} kHz")
    }
}

pub(crate) fn channels(count: u16) -> String {
    match count {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{n} channels"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_monitoring_convention() {
        assert_eq!(Status::Up.exit_code(), 0);
        assert_eq!(Status::Degraded.exit_code(), 1);
        assert_eq!(Status::Down.exit_code(), 2);
        assert_eq!(EXIT_UNKNOWN, 3);
        // The worst status wins when several stations are checked
        assert_eq!(
            [Status::Up, Status::Down, Status::Degraded].iter().max(),
            Some(&Status::Down)
        );
    }

    #[test]
    fn rates_and_channels_read_naturally() {
        assert_eq!(khz(44100), "44.1 kHz");
        assert_eq!(khz(48000), "48 kHz");
        assert_eq!(khz(22050), "22.1 kHz");
        assert_eq!(channels(1), "mono");
        assert_eq!(channels(2), "stereo");
        assert_eq!(channels(6), "6 channels");
    }

    #[test]
    fn codes_are_snake_case_in_json() {
        let json = serde_json::to_string(&ErrorCode::HttpStatus).unwrap();
        assert_eq!(json, "\"http_status\"");
        let json = serde_json::to_string(&ProblemCode::DecodeErrors).unwrap();
        assert_eq!(json, "\"decode_errors\"");
        let json = serde_json::to_string(&Status::Degraded).unwrap();
        assert_eq!(json, "\"degraded\"");
    }

    #[test]
    fn the_status_follows_errors_and_problems() {
        let mut report = Report::new("http://x", String::new());
        report.audio = Some(Audio {
            codec: "MP3".to_string(),
            sample_rate: 44100,
            channels: 2,
            rms_dbfs: Some(-18.2),
            ..Audio::default()
        });
        report.conclude();
        assert_eq!(report.status, Status::Up);
        assert!(report.live);
        assert_eq!(report.summary, "MP3, 44.1 kHz stereo, -18 dBFS");

        report.problems.push(Problem {
            code: ProblemCode::Silent,
            message: "silent".to_string(),
        });
        report.conclude();
        assert_eq!(report.status, Status::Degraded);
        assert!(!report.live);

        report.error = Some(ErrorInfo {
            code: ErrorCode::NoAudio,
            http_status: None,
            message: "No audio".to_string(),
        });
        report.conclude();
        assert_eq!(report.status, Status::Down);
        assert_eq!(report.summary, "No audio");
    }
}
