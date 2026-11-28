//! Digital signal processing modules

pub mod cleanup;
pub mod crackle;
pub mod equalizer;
pub(crate) mod filter;
pub mod hiss;
pub mod hum;
pub mod level;
pub mod limiter;
pub(crate) mod output_stage;
pub mod resample;
pub mod sound;

pub use cleanup::CleanupSource;
pub use crackle::CrackleSource;
pub use equalizer::{
    find_preset, peak_boost_db, EqParams, EqPreset, EqSource, PresetGroup, SharedEqParams, PRESETS,
};
pub use hiss::HissSource;
pub use hum::HumSource;
pub use level::LevelSource;
pub use limiter::LimiterSource;
pub use resample::ResampleSource;
pub use sound::{
    CrackleLevel, FilterSwitches, HissLevel, HumReadout, LevelPhase, LevelReadout, LevelTarget,
    SharedSoundParams, SoundParams,
};
