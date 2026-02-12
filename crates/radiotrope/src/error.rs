//! Error types for Radiotrope
//!
//! Centralized error handling using thiserror.

use thiserror::Error;

/// Main error type for Radiotrope engine
#[derive(Error, Debug)]
pub enum RadioError {
    #[error("Network error: {0}")]
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
