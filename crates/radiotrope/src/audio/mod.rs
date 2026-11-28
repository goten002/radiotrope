//! Audio subsystem
//!
//! Handles audio playback, decoding, DSP processing, and visualization.
//!

pub mod aac;
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

pub use aac::{aac_decoder, fdk_version, AacDecoderKind};
pub use analyzer::AnalyzingSource;
pub use decoder::SymphoniaSource;
pub use dsp::equalizer::{
    find_preset, peak_boost_db, EqParams, EqPreset, EqSource, PresetGroup, SharedEqParams, PRESETS,
};
pub use dsp::sound::{
    CrackleLevel, FilterSwitches, HissLevel, HumReadout, LevelPhase, LevelReadout, LevelTarget,
    SharedSoundParams, SoundParams,
};
pub use engine::{AudioEngine, EngineConfig, EngineOutput};
pub use recording::{
    recover_partial_recordings, Listen, ListenError, ListenStream, Recorder, RecordingFormat,
    RecordingOptions, RecordingStatus, RecordingTags, RecordingTap, Recovered, TapPoint,
};
pub use stats::{new_shared_stats, DecoderStats, SharedStats, StreamStats};
pub use types::{
    AudioAnalysis, AudioCommand, AudioEvent, CodecInfo, EngineEvent, PlaybackState, StreamId,
};
