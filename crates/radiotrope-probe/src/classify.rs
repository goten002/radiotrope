//! Turns the engine's errors into the report's fixed error codes

use std::error::Error;

use radiotrope::error::RadioError;

use crate::report::{ErrorCode, ErrorInfo};

/// The error code and message for an engine error
pub(crate) fn classify(err: &RadioError) -> ErrorInfo {
    let message = message_of(err);
    if let RadioError::Network(e) = err {
        return network(e, message);
    }
    if matches!(err, RadioError::Timeout(_) | RadioError::Cancelled) {
        return info(ErrorCode::Timeout, message);
    }
    from_text(message)
}

/// The message without the engine's "Stream error: " style prefix, which
/// tells a user nothing
fn message_of(err: &RadioError) -> String {
    match err {
        RadioError::Stream(m) | RadioError::Decode(m) | RadioError::Audio(m) => m.clone(),
        RadioError::Timeout(m) => m.clone(),
        RadioError::Cancelled => "Stopped before the stream answered".to_string(),
        other => other.to_string(),
    }
}

/// A reqwest error, told apart by what failed
fn network(e: &reqwest::Error, message: String) -> ErrorInfo {
    if let Some(status) = e.status() {
        return http(status.as_u16(), message);
    }
    if e.is_builder() {
        return info(ErrorCode::InvalidUrl, message);
    }
    if e.is_timeout() {
        return info(ErrorCode::Timeout, message);
    }
    let chain = chain_text(e);
    if is_dns(&chain) {
        return info(ErrorCode::Dns, with_cause(message, "host name not found"));
    }
    if is_tls(&chain) {
        return info(
            ErrorCode::Tls,
            with_cause(message, "secure connection failed"),
        );
    }
    if e.is_connect() {
        return info(ErrorCode::Connect, message);
    }
    from_text(message)
}

/// Engine errors that come as text
fn from_text(message: String) -> ErrorInfo {
    let lower = message.to_lowercase();
    if let Some(status) = http_status(&message) {
        return http(status, message);
    }
    let code = if lower.contains("web page") {
        ErrorCode::WebPage
    } else if lower.contains("no stream url found") || lower.contains("no variants") {
        ErrorCode::EmptyPlaylist
    } else if lower.contains("encrypted") {
        ErrorCode::EncryptedHls
    } else if lower.contains("unsupported codec") {
        ErrorCode::UnsupportedCodec
    } else if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("did not start within")
    {
        ErrorCode::Timeout
    } else if is_dns(&lower) {
        ErrorCode::Dns
    } else if lower.contains("could not connect") {
        ErrorCode::Connect
    } else if lower.contains("probe error")
        || lower.contains("no audio track")
        || lower.contains("invalid audio format")
    {
        ErrorCode::DecodeFailed
    } else if lower.contains("before any audio") {
        ErrorCode::NoAudio
    } else {
        ErrorCode::StreamFailed
    };
    info(code, message)
}

/// The status in a message like "HTTP 404 Not Found"
fn http_status(message: &str) -> Option<u16> {
    let at = message.find("HTTP ")?;
    let digits = message.get(at + 5..at + 8)?;
    let status: u16 = digits.parse().ok()?;
    (100..600).contains(&status).then_some(status)
}

/// Every error in the chain, lowercased: reqwest puts what really failed
/// (the DNS lookup, the certificate) in its sources
fn chain_text(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(" | ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text.to_lowercase()
}

fn is_dns(text: &str) -> bool {
    [
        "dns error",
        "failed to lookup address",
        "name or service not known",
        "no such host is known",
        "nodename nor servname",
        "temporary failure in name resolution",
    ]
    .iter()
    .any(|s| text.contains(s))
}

fn is_tls(text: &str) -> bool {
    ["certificate", "tls", "ssl", "handshake"]
        .iter()
        .any(|s| text.contains(s))
}

fn with_cause(message: String, cause: &str) -> String {
    format!("{message} ({cause})")
}

fn http(status: u16, message: String) -> ErrorInfo {
    ErrorInfo {
        code: ErrorCode::HttpStatus,
        http_status: Some(status),
        message,
    }
}

fn info(code: ErrorCode, message: String) -> ErrorInfo {
    ErrorInfo {
        code,
        http_status: None,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(err: RadioError) -> (ErrorCode, Option<u16>, String) {
        let info = classify(&err);
        (info.code, info.http_status, info.message)
    }

    #[test]
    fn http_errors_carry_their_status() {
        let (code, status, message) = code_of(RadioError::Stream("HTTP 404 Not Found".to_string()));
        assert_eq!(code, ErrorCode::HttpStatus);
        assert_eq!(status, Some(404));
        assert_eq!(message, "HTTP 404 Not Found");

        let (code, status, _) = code_of(RadioError::Stream(
            "No audio for 2 min: HTTP 503 Service Unavailable".to_string(),
        ));
        assert_eq!((code, status), (ErrorCode::HttpStatus, Some(503)));
    }

    #[test]
    fn engine_messages_map_to_codes() {
        let cases = [
            (
                "the server sent a web page instead of audio",
                ErrorCode::WebPage,
            ),
            (
                "No stream URL found in PLS playlist",
                ErrorCode::EmptyPlaylist,
            ),
            ("No variants in master playlist", ErrorCode::EmptyPlaylist),
            (
                "Encrypted HLS streams (AES-128) are not supported",
                ErrorCode::EncryptedHls,
            ),
            ("Unsupported codec: AC-3", ErrorCode::UnsupportedCodec),
            ("Timeout waiting for stream data", ErrorCode::Timeout),
            ("Connection timed out", ErrorCode::Timeout),
            ("Could not connect to radio.example", ErrorCode::Connect),
            ("Probe error: unsupported format", ErrorCode::DecodeFailed),
            ("The stream ended before any audio", ErrorCode::NoAudio),
            ("something new", ErrorCode::StreamFailed),
        ];
        for (message, expected) in cases {
            let (code, status, _) = code_of(RadioError::Stream(message.to_string()));
            assert_eq!(code, expected, "{message}");
            assert_eq!(status, None, "{message}");
        }
    }

    #[test]
    fn timeouts_and_stops_are_timeouts() {
        let (code, _, message) = code_of(RadioError::Timeout(
            "the station did not start within 12 s".to_string(),
        ));
        assert_eq!(code, ErrorCode::Timeout);
        assert_eq!(message, "the station did not start within 12 s");
        assert_eq!(code_of(RadioError::Cancelled).0, ErrorCode::Timeout);
    }

    #[test]
    fn the_engine_prefix_is_dropped() {
        let (_, _, message) = code_of(RadioError::Decode("Probe error: x".to_string()));
        assert_eq!(message, "Probe error: x");
    }

    #[test]
    fn only_real_statuses_are_read() {
        assert_eq!(http_status("HTTP 200 OK"), Some(200));
        assert_eq!(http_status("HTTP client error: x"), None);
        assert_eq!(http_status("HTTP 9"), None);
        assert_eq!(http_status("no status"), None);
    }

    #[test]
    fn dns_failures_are_recognised_on_linux_and_windows() {
        assert!(is_dns(
            "dns error: failed to lookup address information: name or service not known"
        ));
        assert!(is_dns("dns error: no such host is known. (os error 11001)"));
        assert!(!is_dns("connection refused"));
    }
}
