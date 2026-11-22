//! Radiotrope App Services
//!
//! Station providers, data persistence, and networking utilities.
//! Depends on the `radiotrope` engine crate.

pub mod config;
pub mod data;
pub mod hex_color;
pub mod network;
pub mod text;
pub mod visual;

pub use radiotrope_library::{error, providers};

#[cfg(feature = "embedded")]
pub mod wifi;
