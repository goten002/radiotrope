//! The command line of `radiotrope-probe`. Kept in its own file with nothing
//! but clap and std in it: `cargo xtask` includes it to write the man page
//! and the shell completions, so the program itself carries no code for them.

use std::path::PathBuf;

use clap::Parser;

/// Check internet radio stations: on air or not, their format, and what is playing
#[derive(Parser, Debug)]
#[command(name = "radiotrope-probe", version, after_help = EXIT_HELP)]
pub struct Args {
    /// Station addresses: a stream, a PLS or M3U playlist, or HLS
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

    /// Most seconds a station may take, start to finish
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
}

const EXIT_HELP: &str = "Exit status: 0 up, 1 degraded, 2 down (the worst station counts), \
3 the check can't run.";
