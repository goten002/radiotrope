//! Error types for Radiotrope
//!
//! Centralized error handling using thiserror.

use thiserror::Error;

/// Main error type for Radiotrope engine
///
/// The `Display` text is what a user may read, with no "… error:" prefix.
/// `Debug` keeps the variant for the logs.
#[derive(Error, Debug)]
pub enum RadioError {
    #[error("{}", friendly_network_error(.0))]
    Network(#[from] reqwest::Error),

    #[error("{0}")]
    Audio(String),

    #[error("{0}")]
    Stream(String),

    #[error("{0}")]
    Decode(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Timeout(String),

    /// The stream was stopped (see `StreamCancel`) before it was ready
    #[error("Cancelled")]
    Cancelled,
}

/// Result type alias for Radiotrope
pub type Result<T> = std::result::Result<T, RadioError>;

fn friendly_network_error(e: &reqwest::Error) -> String {
    if e.is_builder() {
        if let Some(url) = e.url() {
            return format!("Can't open {url}: not a valid web address");
        }
        return "Can't open this address: not a valid web address".to_string();
    }
    if e.is_connect() {
        if is_lookup_failure(&error_chain(e)) {
            return "No internet connection".to_string();
        }
        if let Some(url) = e.url() {
            if let Some(host) = url.host_str() {
                return format!("Can't connect to {host}");
            }
        }
        return "Can't connect to the server".to_string();
    }
    if e.is_timeout() {
        return "Connection timed out".to_string();
    }
    if e.is_decode() {
        return "Can't read the server's answer".to_string();
    }
    format!("Can't reach the server ({})", reqwest_reason(e))
}

/// Ways each system words a failed name lookup, in lower case. A failed
/// lookup almost always means there is no network, not a bad station.
const LOOKUP_FAILED: [&str; 5] = [
    "dns error",
    "failed to lookup address",
    "name or service not known",
    "temporary failure in name resolution",
    "no such host is known",
];

/// Whether `chain`, an error and its causes in lower case (see
/// [`error_chain`]), is a failed name lookup
pub fn is_lookup_failure(chain: &str) -> bool {
    LOOKUP_FAILED.iter().any(|m| chain.contains(m))
}

/// An error and all its causes, one per line, in lower case
pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut chain = String::new();
    let mut source = Some(e);
    while let Some(s) = source {
        chain.push_str(&s.to_string().to_lowercase());
        chain.push('\n');
        source = s.source();
    }
    chain
}

/// The innermost cause of a request error, which names what really failed
/// ("connection reset by peer") without reqwest's "error sending request
/// for url (…)" wrapper
fn reqwest_reason(e: &reqwest::Error) -> String {
    let mut reason = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        reason = s.to_string();
        source = s.source();
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::RadioError;

    #[test]
    fn display_has_no_prefix() {
        assert_eq!(
            RadioError::Stream("HTTP 404 Not Found".into()).to_string(),
            "HTTP 404 Not Found"
        );
        assert_eq!(
            RadioError::Timeout("Timeout waiting for stream data".into()).to_string(),
            "Timeout waiting for stream data"
        );
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        assert_eq!(RadioError::Io(io).to_string(), "no such file");
    }

    #[test]
    fn debug_keeps_the_variant() {
        let e = RadioError::Decode("Probe error: x".into());
        assert_eq!(format!("{e:?}"), "Decode(\"Probe error: x\")");
    }

    #[test]
    fn a_failed_name_lookup_reads_as_no_internet() {
        let linux = "error sending request\nclient error (connect)\ndns error\nfailed to lookup address information: temporary failure in name resolution\n";
        let windows = "error sending request\ndns error\nno such host is known. (os error 11001)\n";
        let macos = "error sending request\ndns error\nfailed to lookup address information: nodename nor servname provided, or not known\n";
        assert!(super::is_lookup_failure(linux));
        assert!(super::is_lookup_failure(windows));
        assert!(super::is_lookup_failure(macos));
        let refused =
            "error sending request\ntcp connect error\nconnection refused (os error 111)\n";
        assert!(!super::is_lookup_failure(refused));
    }

    #[test]
    fn a_host_that_does_not_exist_reads_as_no_internet() {
        // `.invalid` never resolves (RFC 6761), with or without a network
        let e = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get("http://radiotrope-test.invalid/")
            .send()
            .unwrap_err();
        assert_eq!(RadioError::Network(e).to_string(), "No internet connection");
    }

    #[test]
    fn refused_connection_names_the_host() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .unwrap_err();
        assert_eq!(
            RadioError::Network(e).to_string(),
            "Can't connect to 127.0.0.1"
        );
    }
}
