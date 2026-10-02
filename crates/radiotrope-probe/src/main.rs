//! radiotrope-probe: check internet radio stations from the command line
//! or a scheduler
//!
//! Exit codes: 0 every station up, 1 one degraded, 2 one down, 3 the check
//! could not run (bad arguments, unreadable list).

use std::collections::BTreeMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::Parser;
use radiotrope_probe::report::EXIT_UNKNOWN;
use radiotrope_probe::text::{self, Style};
use radiotrope_probe::{check, Options, Report, Status};

/// Check internet radio stations: on air or not, their format, and what is playing
#[derive(Parser, Debug)]
#[command(name = "radiotrope-probe", version, after_help = EXIT_HELP)]
struct Args {
    /// Station addresses: a stream, a PLS or M3U playlist, or HLS
    urls: Vec<String>,

    /// Read addresses from a file, one per line ("-" for standard input;
    /// blank lines and lines starting with # are skipped)
    #[arg(short, long, value_name = "FILE")]
    input: Option<PathBuf>,

    /// JSON: one object, or an array when checking several stations
    #[arg(long, conflicts_with_all = ["ndjson", "quiet"])]
    json: bool,

    /// One JSON object per line, printed as each check finishes
    #[arg(long, conflicts_with = "quiet")]
    ndjson: bool,

    /// One line per station
    #[arg(short, long)]
    quiet: bool,

    /// Seconds to listen once the audio starts
    #[arg(short, long, value_name = "SECS", default_value_t = 10,
          value_parser = clap::value_parser!(u64).range(1..=600))]
    listen: u64,

    /// Most seconds a station may take, start to finish
    #[arg(short, long, value_name = "SECS", default_value_t = 30,
          value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,

    /// Stations checked at the same time
    #[arg(short, long, value_name = "N", default_value_t = 4,
          value_parser = clap::value_parser!(u64).range(1..=64))]
    jobs: u64,

    /// Sound below this level counts as silence
    #[arg(long, value_name = "DBFS", default_value_t = -50.0, allow_negative_numbers = true)]
    silence_db: f64,
}

const EXIT_HELP: &str = "Exit status: 0 up, 1 degraded, 2 down (the worst station counts), \
3 the check could not run.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    Text,
    Quiet,
    Json,
    Ndjson,
}

fn main() {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(e) => {
            let code = match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                // clap would exit 2, which a scheduler reads as "down"
                _ => EXIT_UNKNOWN,
            };
            let _ = e.print();
            std::process::exit(code);
        }
    };
    std::process::exit(run(args));
}

fn run(args: Args) -> i32 {
    let mut urls = args.urls.clone();
    if let Some(path) = &args.input {
        match read_list(path) {
            Ok(list) => urls.extend(list),
            Err(e) => {
                eprintln!("radiotrope-probe: can't read {}: {e}", path.display());
                return EXIT_UNKNOWN;
            }
        }
    }
    if urls.is_empty() {
        eprintln!("radiotrope-probe: no station to check (give a URL or --input FILE; see --help)");
        return EXIT_UNKNOWN;
    }

    let options = Options {
        listen: Duration::from_secs(args.listen),
        timeout: Duration::from_secs(args.timeout),
        silence_db: args.silence_db,
    };
    let output = if args.json {
        Output::Json
    } else if args.ndjson {
        Output::Ndjson
    } else if args.quiet {
        Output::Quiet
    } else {
        Output::Text
    };
    let style = if use_colour(output) {
        Style::Colour
    } else {
        Style::Plain
    };

    let several = urls.len() > 1;
    let reports = check_all(urls, &options, args.jobs as usize);
    let mut worst = Status::Up;
    let mut printer = Printer::new(output, style, several);
    for (index, report) in reports {
        worst = worst.max(report.status);
        printer.add(index, report);
    }
    printer.finish();
    worst.exit_code()
}

/// Addresses in a list file: one per line, blank lines and # comments skipped
fn read_list(path: &PathBuf) -> io::Result<Vec<String>> {
    let lines: Vec<String> = if path.as_os_str() == "-" {
        io::stdin().lock().lines().collect::<io::Result<_>>()?
    } else {
        let text = std::fs::read(path)?;
        String::from_utf8_lossy(&text)
            .lines()
            .map(str::to_string)
            .collect()
    };
    Ok(lines
        .iter()
        // A list saved by a Windows editor may start with a BOM
        .map(|l| l.trim_start_matches('\u{feff}').trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

/// Check every address, `jobs` at a time. Reports come as each finishes,
/// with their place in `urls`.
fn check_all(
    urls: Vec<String>,
    options: &Options,
    jobs: usize,
) -> impl Iterator<Item = (usize, Report)> {
    let urls = Arc::new(urls);
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = crossbeam_channel::unbounded();
    for _ in 0..jobs.min(urls.len()) {
        let (urls, next, tx, options) = (urls.clone(), next.clone(), tx.clone(), options.clone());
        thread::spawn(move || loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(url) = urls.get(index) else {
                break;
            };
            if tx.send((index, check(url, &options))).is_err() {
                break;
            }
        });
    }
    rx.into_iter()
}

/// Prints reports: text in the order given, NDJSON as they come, JSON at the end
struct Printer {
    output: Output,
    style: Style,
    several: bool,
    waiting: BTreeMap<usize, Report>,
    next: usize,
    all: Vec<(usize, Report)>,
}

impl Printer {
    fn new(output: Output, style: Style, several: bool) -> Self {
        Self {
            output,
            style,
            several,
            waiting: BTreeMap::new(),
            next: 0,
            all: Vec::new(),
        }
    }

    fn add(&mut self, index: usize, report: Report) {
        match self.output {
            Output::Ndjson => print(&format!("{}\n", json(&report, false))),
            Output::Json => self.all.push((index, report)),
            Output::Text | Output::Quiet => {
                self.waiting.insert(index, report);
                while let Some(report) = self.waiting.remove(&self.next) {
                    let text = match self.output {
                        Output::Quiet => text::quiet(&report, self.style),
                        // A blank line between stations
                        _ if self.next > 0 => format!("\n{}", text::full(&report, self.style)),
                        _ => text::full(&report, self.style),
                    };
                    print(&text);
                    self.next += 1;
                }
            }
        }
    }

    fn finish(mut self) {
        if self.output != Output::Json {
            return;
        }
        let text = if self.several {
            // In the order given, not the order they finished
            self.all.sort_by_key(|(index, _)| *index);
            let reports: Vec<&Report> = self.all.iter().map(|(_, r)| r).collect();
            serde_json::to_string_pretty(&reports).unwrap_or_default()
        } else {
            self.all
                .first()
                .map(|(_, r)| json(r, true))
                .unwrap_or_default()
        };
        print(&format!("{text}\n"));
    }
}

fn json(report: &Report, pretty: bool) -> String {
    let result = if pretty {
        serde_json::to_string_pretty(report)
    } else {
        serde_json::to_string(report)
    };
    result.unwrap_or_default()
}

/// Print, ignoring a closed pipe (`radiotrope-probe ... | head`)
fn print(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Colour the status word on a terminal, unless NO_COLOR is set. Not on
/// Windows: its older consoles show the codes as text.
fn use_colour(output: Output) -> bool {
    matches!(output, Output::Text | Output::Quiet)
        && !cfg!(windows)
        && io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}
