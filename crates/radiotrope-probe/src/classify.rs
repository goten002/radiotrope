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
        return info(ErrorCode::Connect, with_port(e, message));
    }
    from_text(message)
}

/// The engine's words for a station it couldn't connect to: "Can't
/// connect to <host>" now, "Could not connect to <host>" before
const CONNECT_FAILED: [&str; 2] = ["can't connect", "could not connect"];

/// Engine errors that come as text. The engine's other network texts ("Can't
/// reach the server (…)", "Can't read the server's answer", "Can't open …:
/// not a valid web address") match nothing here and stay stream_failed, as
/// their old wording did; a cause inside the brackets (a timeout, a failed
/// name lookup) still counts.
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
    } else if CONNECT_FAILED.iter().any(|m| lower.contains(m)) {
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

/// "Can't connect to 127.0.0.1" names the host only; add the port when it
/// isn't the scheme's own (80, 443), as a station on 8000 is a different
/// server from the web site on 80
fn with_port(e: &reqwest::Error, message: String) -> String {
    let Some(url) = e.url() else {
        return message;
    };
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) if message.ends_with(host) => format!("{message}:{port}"),
        _ => message,
    }
}

fn with_cause(message: String, cause: &str) -> String {
    format!("{message} ({cause})")
}

fn http(status: u16, message: String) -> ErrorInfo {
    ErrorInfo {
        code: ErrorCode::HttpStatus,
        http_status: Some(status),
        message: capitalised(message),
    }
}

fn info(code: ErrorCode, message: String) -> ErrorInfo {
    ErrorInfo {
        code,
        http_status: None,
        message: capitalised(message),
    }
}

/// Every message starts with a capital, as a sentence does; some engine
/// texts ("the server sent a web page…") don't
fn capitalised(message: String) -> String {
    let mut chars = message.chars();
    match chars.next() {
        Some(first) if first.is_lowercase() => first.to_uppercase().chain(chars).collect(),
        _ => message,
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
            ("Can't connect to radio.example", ErrorCode::Connect),
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

    /// A server on a free local port that answers one request with
    /// `reply`, or holds it unanswered for `hold`, then closes
    fn one_shot_server(reply: &'static [u8], hold: std::time::Duration) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                std::thread::sleep(hold);
                let _ = socket.write_all(reply);
            }
        });
        port
    }

    /// A client that gives up after 300 ms, for answers that don't come
    fn client() -> reqwest::blocking::Client {
        client_waiting(std::time::Duration::from_millis(300))
    }

    /// A client that waits as long as the engine's do for a connection
    /// (`STREAM_CONNECT_TIMEOUT_SECS`). Windows tries a refused connect
    /// again for about 2 s before it says so: a shorter wait ends as a
    /// timeout there, which no client of the engine's has.
    fn patient_client() -> reqwest::blocking::Client {
        client_waiting(std::time::Duration::from_secs(
            radiotrope::config::timeouts::STREAM_CONNECT_TIMEOUT_SECS,
        ))
    }

    fn client_waiting(timeout: std::time::Duration) -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(timeout)
            .build()
            .unwrap()
    }

    /// The engine's own text for `e`, as its Display gives it, read back as
    /// text: what a reader that gave up reports ("No audio for …: <text>")
    fn as_text(e: reqwest::Error) -> (ErrorCode, String) {
        let text = RadioError::Network(e).to_string();
        let gave_up = RadioError::Stream(format!("No audio for 1 min: {text}"));
        let (code, _, _) = code_of(gave_up);
        (code, text)
    }

    #[test]
    fn the_engine_network_texts_keep_their_codes() {
        // Refused: nothing listens on a port just freed
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let refused = || {
            patient_client()
                .get(format!("http://127.0.0.1:{port}/"))
                .send()
                .unwrap_err()
        };
        let (code, text) = as_text(refused());
        assert!(text.starts_with("Can't connect to "), "{text}");
        assert_eq!(code, ErrorCode::Connect, "{text}");
        // And as the error itself, on Windows too
        let (code, _, message) = code_of(RadioError::Network(refused()));
        assert_eq!(code, ErrorCode::Connect);
        assert_eq!(message, format!("Can't connect to 127.0.0.1:{port}"));

        // No answer in time
        let port = one_shot_server(b"", std::time::Duration::from_secs(2));
        let slow = client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .unwrap_err();
        let (code, text) = as_text(slow);
        assert!(text.contains("timed out"), "{text}");
        assert_eq!(code, ErrorCode::Timeout, "{text}");

        // An answer that can't be read
        let port = one_shot_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
            std::time::Duration::ZERO,
        );
        let unreadable = client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .unwrap()
            .json::<serde_json::Value>()
            .unwrap_err();
        let (code, text) = as_text(unreadable);
        assert!(text.starts_with("Can't read the server"), "{text}");
        assert_eq!(code, ErrorCode::StreamFailed, "{text}");

        // The connection drops before an answer
        let port = one_shot_server(b"", std::time::Duration::ZERO);
        let dropped = client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .unwrap_err();
        let (code, text) = as_text(dropped);
        assert!(text.starts_with("Can't reach the server ("), "{text}");
        assert_eq!(code, ErrorCode::StreamFailed, "{text}");

        // Not an address at all
        let invalid = client().get("http://").send().unwrap_err();
        let (code, text) = as_text(invalid);
        assert!(text.starts_with("Can't open "), "{text}");
        assert_eq!(code, ErrorCode::StreamFailed, "{text}");
    }

    #[test]
    fn the_engine_texts_are_its_display() {
        // Display no longer adds "Stream error: ", so text and message agree
        for err in [
            RadioError::Stream("No variants in master playlist".to_string()),
            RadioError::Decode("Unsupported codec: AC-3".to_string()),
            RadioError::Timeout("Timeout waiting for stream data".to_string()),
        ] {
            let text = err.to_string();
            let (code, _, message) = code_of(err);
            assert_eq!(message, text);
            assert_eq!(from_text(text.clone()).code, code, "{text}");
        }
    }

    #[test]
    fn timeouts_and_stops_are_timeouts() {
        let (code, _, message) = code_of(RadioError::Timeout(
            "the station did not start within 12 s".to_string(),
        ));
        assert_eq!(code, ErrorCode::Timeout);
        assert_eq!(message, "The station did not start within 12 s");
        assert_eq!(code_of(RadioError::Cancelled).0, ErrorCode::Timeout);
    }

    #[test]
    fn every_message_starts_with_a_capital() {
        let (code, _, message) = code_of(RadioError::Stream(
            "the server sent a web page instead of audio".to_string(),
        ));
        assert_eq!(code, ErrorCode::WebPage);
        assert_eq!(message, "The server sent a web page instead of audio");
        assert_eq!(capitalised("éclair".to_string()), "Éclair");
        assert_eq!(capitalised(String::new()), "");
        assert_eq!(capitalised("HTTP 404".to_string()), "HTTP 404");
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
