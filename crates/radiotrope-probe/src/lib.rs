//! Radiotrope Probe: checks an internet radio station
//!
//! Connects to a station the way the player does (playlists, HLS, ICY),
//! decodes a few seconds of its audio without a sound card, and reports
//! whether it is on air, its format, its sound level and what is playing.
//!
//! ```no_run
//! let report = radiotrope_probe::check("http://radio.example/live", &Default::default());
//! println!("{}", serde_json::to_string(&report).unwrap());
//! ```

mod check;
mod classify;
mod level;
pub mod report;
pub mod text;
mod time;

pub use check::{check, Options};
pub use report::{Report, Status};
