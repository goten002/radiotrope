//! Audio subsystem
//!
//! Handles audio playback, decoding, DSP processing, and visualization.
//!

pub mod analyzer;
pub mod decoder;
pub mod engine;
pub mod health;
pub mod stats;
pub mod types;

pub use analyzer::AnalyzingSource;
pub use decoder::SymphoniaSource;
pub use engine::AudioEngine;
pub use stats::{new_shared_stats, DecoderStats, EventBus, SharedStats, StreamEvent, StreamStats};
pub use types::{AudioAnalysis, AudioCommand, AudioEvent, CodecInfo, PlaybackState};
