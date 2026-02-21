//! Error types for Radiotrope app services
//!
//! Application-level errors that wrap engine errors and add app-specific variants.

use radiotrope::error::RadioError;
use thiserror::Error;

/// Application error type
#[derive(Error, Debug)]
pub enum AppError {
    #[error(transparent)]
    Engine(#[from] RadioError),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Image error: {0}")]
    Image(String),
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Engine(RadioError::Network(e))
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Engine(RadioError::Io(e))
    }
}

/// Result type alias for Radiotrope app services
pub type Result<T> = std::result::Result<T, AppError>;
