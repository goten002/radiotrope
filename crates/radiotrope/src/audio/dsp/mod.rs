//! Digital signal processing modules

pub mod equalizer;

pub use equalizer::{find_preset, EqParams, EqPreset, EqSource, SharedEqParams, PRESETS};
