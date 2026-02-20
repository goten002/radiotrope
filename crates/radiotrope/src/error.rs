//! Error types for Radiotrope
//!
//! Centralized error handling using thiserror.

use thiserror::Error;

/// Main error type for Radiotrope engine
#[derive(Error, Debug)]
pub enum RadioError {
    #[error("{}", friendly_network_error(.0))]
    Network(#[from] reqwest::Error),

    #[error("Audio error: {0}")]
    Audio(String),

    #[error("Stream error: {0}")]
    Stream(String),

    #[error("Decode error: {0}")]
    Decode(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Timeout: {0}")]
    Timeout(String),
}

/// Result type alias for Radiotrope
pub type Result<T> = std::result::Result<T, RadioError>;

fn friendly_network_error(e: &reqwest::Error) -> String {
    if e.is_builder() {
        if let Some(url) = e.url() {
            return format!("Invalid URL: {url}");
        }
        return "Invalid URL".to_string();
    }
    if e.is_connect() {
        if let Some(url) = e.url() {
            return format!("Could not connect to {}", url.host_str().unwrap_or("server"));
        }
        return "Could not connect to server".to_string();
    }
    if e.is_timeout() {
        return "Connection timed out".to_string();
    }
    if e.is_decode() {
        return "Invalid response from server".to_string();
    }
    format!("Network error: {e}")
}
