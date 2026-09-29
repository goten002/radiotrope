//! Radiotrope App Services
//!
//! Station providers, data persistence, and networking utilities.
//! Depends on the `radiotrope` engine crate.

pub mod config;
pub mod data;
pub mod error;
pub mod network;
pub mod providers;
pub mod text;
pub mod visual;

#[cfg(feature = "embedded")]
pub mod wifi;
