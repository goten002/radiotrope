//! The command line of `radiotrope-cli`. Kept in its own file with nothing
//! but clap and std in it: `cargo xtask` includes it to write the man page
//! and the shell completions, so the program itself carries no code for them.

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "radiotrope-cli",
    about = "Play internet radio, search the station directory, and manage shared Radiotrope favorites",
    version
)]
pub struct Cli {
    /// A favorite's number, part of its name, or a stream URL to play at
    /// start. Without it, the player opens on your favorites.
    pub station: Option<String>,
}
