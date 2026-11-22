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
