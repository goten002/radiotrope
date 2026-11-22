//! The command line of `radiotrope-cli`. Kept in its own file with nothing
//! but clap and std in it: `cargo xtask` includes it to write the man page
//! and the shell completions, so the program itself carries no code for them.

use clap::Parser;

/// Shown after the options in `--help`. The keys are the main ones from the
/// player's key table (a test checks they still match); `?` in the player
/// lists them all.
pub const AFTER_HELP: &str = "\
Keys:
  /          Search the station directory
  Enter      Play the highlighted station
  Space      Play or stop
  n  p       Next or previous favorite
  +  -       Volume up or down
  m          Mute or unmute
  r          Start or stop recording
  f          Add the playing station to favorites, or remove the highlighted one
  ?  F1      Show all the keys
  q          Quit

Examples:
  radiotrope-cli                    Open on your favorites
  radiotrope-cli 3                  Play favorite number 3
  radiotrope-cli jazz radio         Play the first favorite with \"jazz radio\" in its name
  radiotrope-cli https://example.com/live.mp3
                                    Play a stream address

Environment:
  NO_COLOR=1                  No colors
  RUST_LOG=radiotrope=debug   More log output; send it to a file with 2>log.txt

Favorites, the volume, the equalizer and the recording settings come from the
Radiotrope app. Volume changes made here are not saved.";

#[derive(Parser)]
#[command(
    name = "radiotrope-cli",
    about = "The Radiotrope terminal player: internet radio, station search and your Radiotrope favorites",
    after_help = AFTER_HELP,
    version
)]
pub struct Cli {
    /// A favorite's number, part of its name, or a stream address
    /// (http:// or https://) to play at start. Without it, the player opens
    /// on your favorites.
    #[arg(value_name = "STATION")]
    pub station: Vec<String>,
}

impl Cli {
    /// The station asked for, its words joined with spaces
    pub fn station(&self) -> Option<String> {
        let joined = self.station.join(" ");
        let trimmed = joined.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}
