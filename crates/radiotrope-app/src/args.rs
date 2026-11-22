//! The command line of `radiotrope`. Kept in its own file with nothing but
//! clap and std in it: `cargo xtask` includes it to write the man page and
//! the shell completions, so the program itself carries no code for them.

use clap::Parser;

/// Radiotrope, an internet radio player
#[derive(Parser)]
#[command(name = "radiotrope", version, after_help = AFTER_HELP)]
pub struct Args {
    /// Connect an AI agent over MCP on stdio. Talks to the running player,
    /// starting it first if needed, so all agents share one player.
    #[arg(long)]
    pub mcp: bool,

    /// With --mcp: run a separate player for this agent alone
    #[arg(long, requires = "mcp")]
    pub standalone: bool,

    /// Close the running player and wait for it to exit (installers use
    /// this before an update). Succeeds when none is running.
    #[arg(long, conflicts_with = "mcp")]
    pub quit: bool,

    #[cfg(feature = "ui-snapshots")]
    #[arg(long, hide = true, conflicts_with = "mcp", value_name = "OUT_DIR")]
    pub ui_snapshots: Option<std::path::PathBuf>,

    #[cfg(feature = "ui-snapshots")]
    #[arg(long, hide = true, requires = "ui_snapshots", value_enum)]
    pub theme: Option<crate::ui_snapshots::Theme>,

    #[cfg(feature = "ui-snapshots")]
    #[arg(long, hide = true, requires = "ui_snapshots", value_parser = crate::ui_snapshots::parse_scale)]
    pub scale: Option<f32>,

    /// Window size: WIDTHxHEIGHT in logical pixels, or `min`
    #[cfg(feature = "ui-snapshots")]
    #[arg(long, hide = true, requires = "ui_snapshots", value_parser = crate::ui_snapshots::parse_size)]
    pub size: Option<crate::ui_snapshots::Size>,

    /// Only the shots whose names start with this (comma-separated list)
    #[cfg(feature = "ui-snapshots")]
    #[arg(long, hide = true, requires = "ui_snapshots", value_name = "PREFIX")]
    pub only: Option<String>,
}

/// Shown after the options in `--help`, in the same layout as the terminal
/// player's and the station checker's
const AFTER_HELP: &str = "\
Run without options to open the player. A second start brings the open
window forward.

Examples:
  radiotrope                  Open the player
  radiotrope --mcp            Let an AI agent use the open player
  radiotrope --quit           Close the open player

Environment:
  RUST_LOG=radiotrope=debug   More log output; send it to a file with 2>log.txt";
