//! The command line of `radiotrope-probe`. Kept in its own file with nothing
//! but clap and std in it: `cargo xtask` includes it to write the man page
//! and the shell completions, so the program itself carries no code for them.

use std::path::PathBuf;

use clap::Parser;

/// The Radiotrope station checker: on air or not, the format, and what is playing
#[derive(Parser, Debug)]
#[command(name = "radiotrope-probe", version, after_help = AFTER_HELP)]
pub struct Args {
    /// Station addresses: a stream, a PLS or M3U playlist, or HLS
    #[arg(value_name = "URL")]
    pub urls: Vec<String>,

    /// Read addresses from a file, one per line ("-" for standard input;
    /// blank lines and lines starting with # are skipped)
    #[arg(short, long, value_name = "FILE")]
    pub input: Option<PathBuf>,

    /// JSON: one object, or an array when checking several stations
    #[arg(long, conflicts_with_all = ["ndjson", "quiet"])]
    pub json: bool,

    /// One JSON object per line, printed as each check finishes
    #[arg(long, conflicts_with = "quiet")]
    pub ndjson: bool,

    /// One line per station
    #[arg(short, long)]
    pub quiet: bool,

    /// Seconds to listen once the audio starts
    #[arg(short, long, value_name = "SECS", default_value_t = 10,
          value_parser = clap::value_parser!(u64).range(1..=600))]
    pub listen: u64,

    /// Longest time for one station, in seconds, including the listen
    #[arg(short, long, value_name = "SECS", default_value_t = 30,
          value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub timeout: u64,

    /// Stations checked at the same time
    #[arg(short, long, value_name = "N", default_value_t = 4,
          value_parser = clap::value_parser!(u64).range(1..=64))]
    pub jobs: u64,

    /// Sound below this level counts as silence
    #[arg(long, value_name = "DBFS", default_value_t = -50.0, allow_negative_numbers = true)]
    pub silence_db: f64,

    /// The User-Agent to send, for example to name your monitor in the
    /// station's logs. By default the player's, so the station answers as it
    /// answers listeners (some refuse agents they don't know).
    #[arg(long, value_name = "TEXT")]
    pub user_agent: Option<String>,
}

/// Shown after the options in `--help`, in the same layout as the terminal
/// player's and the app's
const AFTER_HELP: &str = "\
Examples:
  radiotrope-probe https://example.com/live.mp3
                              Check one station
  radiotrope-probe --quiet --input stations.txt
                              Check a list from a file, one line per station
  radiotrope-probe --json --listen 20 https://example.com/playlist.m3u
                              Listen 20 seconds; the report as JSON

Exit status: 0 up, 1 degraded, 2 down (the worst station counts), 3 the check
can't run.

Environment:
  NO_COLOR=1                  No colors
  RUST_LOG=radiotrope=debug   More log output; send it to a file with 2>log.txt";
