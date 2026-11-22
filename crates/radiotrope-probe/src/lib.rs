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
pub use radiotrope::config::network::set_user_agent;
pub use report::{Report, Status};

/// The User-Agent the probe sends unless the program using it called
/// [`set_user_agent`] before its first check: the player's own, so a station
/// answers the probe as it answers listeners (some refuse agents they don't
/// know, which would read as a false DOWN)
pub const USER_AGENT: &str = radiotrope::config::network::USER_AGENT;
