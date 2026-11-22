//! radiotrope-probe: check internet radio stations from the command line
//! or a scheduler
//!
//! Exit codes: 0 every station up, 1 one degraded, 2 one down, 3 the check
//! can't run (bad arguments, unreadable list).

mod args;

use std::collections::BTreeMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::Parser;
use radiotrope_probe::report::EXIT_UNKNOWN;
use radiotrope_probe::text::{self, Style, Totals};
use radiotrope_probe::{check, Options, Report, Status};

use args::Args;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    Text,
    Quiet,
    Json,
    Ndjson,
}

/// Warnings and errors only: the report says what happened
const DEFAULT_LOG: &str = "error,radiotrope=warn";
/// The engine's messages, one line each on stderr. `RUST_LOG` changes how
/// much (for example `RUST_LOG=radiotrope=debug`).
fn init_logging() {
    use std::io::Write;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(DEFAULT_LOG))
        .format(|out, record| writeln!(out, "{}", record.args()))
        .init();
}

fn main() {
    init_logging();
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
    if args.listen >= args.timeout {
        eprintln!(
            "radiotrope-probe: --timeout must be longer than --listen (the timeout covers the whole check)"
        );
        return EXIT_UNKNOWN;
    }
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

    // A list makes an array even with one line in it, so a script reading
    // the output doesn't depend on how long the list is today
    let array = args.input.is_some() || urls.len() > 1;
    let several = urls.len() > 1;
    let count = urls.len();
    let stop = Arc::new(AtomicBool::new(false));
    let reports = check_all(urls, &options, args.jobs as usize, stop.clone());
    let mut worst = Status::Up;
    let mut totals = Totals::default();
    let mut progress = Progress::new(several && show_progress(), count);
    let mut printer = Printer::new(output, style, array);
    progress.show(0);
    for (index, report) in reports {
        worst = worst.max(report.status);
        totals.add(&report);
        progress.clear();
        if !printer.add(index, report) {
            // The reader went away (`| head -1`): no one wants the rest
            stop.store(true, Ordering::Relaxed);
            return worst.exit_code();
        }
        progress.show(totals.checked());
    }
    progress.clear();
    if !printer.finish() {
        return worst.exit_code();
    }
    if several {
        match output {
            Output::Text => {
                print(&format!("\n{}", totals.line()));
            }
            // On stderr, so the lines on stdout stay one per station
            Output::Quiet => eprint!("{}", totals.line()),
            Output::Json | Output::Ndjson => {}
        }
    }
    worst.exit_code()
}

/// "Checked 12 of 200" on one line of stderr while a long list runs
struct Progress {
    on: bool,
    total: usize,
    shown: bool,
}

impl Progress {
    fn new(on: bool, total: usize) -> Self {
        Self {
            on,
            total,
            shown: false,
        }
    }

    fn show(&mut self, done: usize) {
        if self.on && done < self.total {
            let mut err = io::stderr().lock();
            let _ = write!(err, "\r\x1b[KChecked {done} of {}", self.total);
            let _ = err.flush();
            self.shown = true;
        }
    }

    /// Take the line away before a report or the totals take its place
    fn clear(&mut self) {
        if self.shown {
            let mut err = io::stderr().lock();
            let _ = write!(err, "\r\x1b[K");
            let _ = err.flush();
            self.shown = false;
        }
    }
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
/// with their place in `urls`. Setting `stop` starts no more checks.
fn check_all(
    urls: Vec<String>,
    options: &Options,
    jobs: usize,
    stop: Arc<AtomicBool>,
) -> impl Iterator<Item = (usize, Report)> {
    let urls = Arc::new(urls);
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = crossbeam_channel::unbounded();
    for _ in 0..jobs.min(urls.len()) {
        let (urls, next, tx, options, stop) = (
            urls.clone(),
            next.clone(),
            tx.clone(),
            options.clone(),
            stop.clone(),
        );
        thread::spawn(move || loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
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
    /// JSON as an array, even of one
    array: bool,
    waiting: BTreeMap<usize, Report>,
    next: usize,
    all: Vec<(usize, Report)>,
}

impl Printer {
    fn new(output: Output, style: Style, array: bool) -> Self {
        Self {
            output,
            style,
            array,
            waiting: BTreeMap::new(),
            next: 0,
            all: Vec::new(),
        }
    }

    /// False once the output can't be written any more
    fn add(&mut self, index: usize, report: Report) -> bool {
        match self.output {
            Output::Ndjson => print(&format!("{}\n", json(&report, false))),
            Output::Json => {
                self.all.push((index, report));
                true
            }
            Output::Text | Output::Quiet => {
                self.waiting.insert(index, report);
                while let Some(report) = self.waiting.remove(&self.next) {
                    let text = match self.output {
                        Output::Quiet => text::quiet(&report, self.style),
                        // A blank line between stations
                        _ if self.next > 0 => format!("\n{}", text::full(&report, self.style)),
                        _ => text::full(&report, self.style),
                    };
                    if !print(&text) {
                        return false;
                    }
                    self.next += 1;
                }
                true
            }
        }
    }

    /// False if the output can't be written
    fn finish(mut self) -> bool {
        if self.output != Output::Json {
            return true;
        }
        let text = if self.array {
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
        print(&format!("{text}\n"))
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

/// Print; false when the output is closed (`radiotrope-probe ... | head`),
/// which is not an error to report
fn print(text: &str) -> bool {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes()).is_ok() && out.flush().is_ok()
}

/// Colour the status word on a terminal, unless NO_COLOR is set
fn use_colour(output: Output) -> bool {
    matches!(output, Output::Text | Output::Quiet)
        && terminal_codes()
        && io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// The progress line, on a terminal that can redraw it
fn show_progress() -> bool {
    terminal_codes() && io::stderr().is_terminal()
}

/// Whether the console understands colour and line codes. On Windows only
/// Windows Terminal is sure to (it sets WT_SESSION); older consoles show
/// the codes as text.
fn terminal_codes() -> bool {
    !cfg!(windows) || std::env::var_os("WT_SESSION").is_some()
}
