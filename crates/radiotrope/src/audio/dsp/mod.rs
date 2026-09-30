//! Digital signal processing modules

pub mod equalizer;

pub use equalizer::{
    find_preset, peak_boost_db, EqParams, EqPreset, EqSource, PresetGroup, SharedEqParams, PRESETS,
};
