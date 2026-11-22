//! The command line of `radiotrope`. Kept in its own file with nothing but
//! clap and std in it: `cargo xtask` includes it to write the man page and
//! the shell completions, so the program itself carries no code for them.

use clap::Parser;

/// Radiotrope, an internet radio player for the desktop
#[derive(Parser)]
#[command(name = "radiotrope", version)]
pub struct Args {
    /// Connect an AI agent over MCP on stdio. Talks to the running player,
    /// starting it first if needed, so all agents share one player.
    #[arg(long)]
    pub mcp: bool,

    /// With --mcp: run a separate player for this agent alone instead
    #[arg(long, requires = "mcp")]
    pub standalone: bool,

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
