//! Audio subsystem
//!
//! Handles audio playback, decoding, DSP processing, and visualization.
//!

pub mod analyzer;
pub mod decoder;
pub mod dsp;
pub mod engine;
pub mod health;
mod output;
mod pcm;
pub mod recording;
pub mod stats;
pub mod types;

pub use analyzer::AnalyzingSource;
pub use decoder::SymphoniaSource;
pub use dsp::equalizer::{find_preset, EqParams, EqPreset, EqSource, SharedEqParams, PRESETS};
pub use engine::{AudioEngine, EngineConfig, EngineOutput};
pub use recording::{
    Recorder, RecordingFormat, RecordingOptions, RecordingStatus, RecordingTags, RecordingTap,
    TapPoint,
};
pub use stats::{new_shared_stats, DecoderStats, EventBus, SharedStats, StreamEvent, StreamStats};
pub use types::{AudioAnalysis, AudioCommand, AudioEvent, CodecInfo, PlaybackState};
