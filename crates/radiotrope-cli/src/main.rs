//! Radiotrope CLI: the terminal internet radio player
//!
//! Plays favorites, searches the station directory, and records streams.

mod args;
mod favorites;
mod input;
mod library;
mod player;
mod recording;
mod search;
mod ui;
mod visual;

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;

use radiotrope::audio::AudioEngine;
use radiotrope_library::data::favorites::FavoritesManager;

use args::Cli;
use favorites::FavoritesWorker;
use input::Effect;
use library::{Settings, Station};
use player::{Phase, Player, VOLUME_STEP};
use search::SearchWorker;
use ui::{ColorMode, Palette, View};

/// Our crates' messages from info up; other libraries' only when they fail
const DEFAULT_LOG: &str = "error,radiotrope=info";
/// The engine's messages, one line each on stderr. `RUST_LOG` changes how
/// much (for example `RUST_LOG=radiotrope=debug`).
fn init_logging() {
    use std::io::Write;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(DEFAULT_LOG))
        .format(|out, record| writeln!(out, "{}", record.args()))
        .init();
}

/// Shown when stdin or stdout is not a terminal (a pipe, cron, a script)
const NEEDS_TERMINAL: &str =
    "radiotrope-cli needs a terminal. To check stations from a script, use radiotrope-probe.";

fn main() -> ExitCode {
    init_logging();
    let cli = Cli::parse();
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("{NEEDS_TERMINAL}");
        return ExitCode::FAILURE;
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            restore_terminal();
            eprintln!("radiotrope-cli: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let favorites_path = library::favorites_path();
    let loaded = favorites_path
        .as_deref()
        .ok_or_else(|| "Favorites directory unavailable".to_owned())
        .and_then(|path| FavoritesManager::observe(path).map_err(|error| error.to_string()));
    let (manager, mut favorites_error) = match loaded {
        Ok(manager) => (manager, None),
        Err(error) => (FavoritesManager::new(), Some(error)),
    };
    let favorites = favorites::stations(&manager);
    let settings = read_settings();

    let station = cli.station();
    let start = match station.as_deref() {
        None => None,
        Some(arg) if arg.contains("://") => Some(
            favorites
                .iter()
                .find(|station| station.url == arg)
                .cloned()
                .unwrap_or_else(|| Station::new("", arg)),
        ),
        Some(arg) => match library::find_favorite(&favorites, arg) {
            Some(station) => Some(station.clone()),
            None => {
                for line in no_match(arg, favorites.len(), favorites_error.as_deref()) {
                    eprintln!("{line}");
                }
                std::process::exit(2);
            }
        },
    };

    let quitting = quit_on_signals();

    // Audio libraries (ALSA, PulseAudio) print to stderr as the engine
    // starts and while it runs, which would scribble over the screen. Only
    // a stderr that is the terminal is silenced: one sent to a file
    // (`2>log.txt`, with `RUST_LOG`) keeps the log.
    #[cfg(unix)]
    if io::stderr().is_terminal() {
        SAVED_STDERR.store(quiet_stderr()?, Ordering::SeqCst);
    }
    // Release builds abort on panic, so drops don't run: restore the
    // terminal (and stderr) before the panic message is printed
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let engine = AudioEngine::new().map_err(|e| format!("Can't open the audio output: {e}"))?;
    let analysis = engine.analysis();
    let mut player = Player::new(engine, &settings);

    let accent = settings.accent.unwrap_or(ui::DEFAULT_ACCENT);
    let palette = Palette::new(ColorMode::detect(), accent);
    let mut view = View::new(favorites, palette, accent);
    if let Some(station) = start {
        if let Some(i) = view.index_of(&station.url) {
            view.select(i);
        }
        player.play(station);
    }

    let search_worker = SearchWorker::spawn();
    let favorites_worker =
        favorites_path.and_then(|path| match FavoritesWorker::spawn(path, manager) {
            Ok(worker) => Some(worker),
            Err(error) => {
                favorites_error = Some(error.to_string());
                None
            }
        });
    if favorites_worker.is_some() {
        if let Some(error) = favorites_error {
            view.apply_favorites(favorites::Reply::Followed(Err(error)));
        }
    } else if let Some(error) = favorites_error {
        view.favorites_unreadable(error);
    }

    // The guard puts the terminal back however this ends: a quit, a signal,
    // an error, or a panic
    let terminal_guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut edit_waiting: Option<favorites::Change> = None;

    // A failed draw or read (the terminal closed under us) ends the loop
    // like a quit, so the recording is still saved below
    let looped = (|| -> io::Result<()> {
        while !quitting.load(Ordering::SeqCst) {
            player.poll();
            if let Ok(worker) = &search_worker {
                view.search.poll(worker);
            }
            if let Some(worker) = &favorites_worker {
                view.poll_favorites(worker);
            }
            if let Some(change) = edit_waiting.filter(|_| !view.saving_favorites) {
                edit_waiting = None;
                if view.favorites_error.is_none() {
                    player.notify(change.notice(), false);
                }
            }
            if player.phase == Phase::Playing {
                let mut a = analysis.lock().unwrap_or_else(|e| e.into_inner());
                view.spectrum.update(Some(&mut a));
            } else {
                view.spectrum.update(None);
            }

            terminal.draw(|f| ui::draw(f, &mut view, &player))?;

            match wait_for_event(ui::frame_interval(&view, &player))? {
                Wait::Event => {}
                Wait::Idle => continue,
                // The terminal window closed: SIGHUP is on its way, or
                // already here
                Wait::Gone => break,
            }
            let key = match event::read() {
                // Windows also reports key releases
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => key,
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let effect = input::route(key, &mut view, player.station.as_ref());
            if let Effect::Edit(edit) = &effect {
                edit_waiting = Some(favorites::Change::of(edit));
            }
            if !apply_effect(
                effect,
                &mut view,
                &mut player,
                search_worker.as_ref().ok(),
                favorites_worker.as_ref(),
            ) {
                break;
            }
        }
        Ok(())
    })();
    if let Some(worker) = favorites_worker {
        worker.finish();
    }

    // Save any recording and close the engine while the screen is still
    // ours (audio libraries may print on the way out)
    let saved = player.shutdown();
    drop(terminal);
    drop(terminal_guard);
    // The screen is gone: the paths stay in the scrollback, to copy
    for path in saved {
        println!("Saved recording: {}", path.display());
    }
    Ok(looped?)
}

/// What to print when `arg` names no favorite
fn no_match(arg: &str, count: usize, favorites_error: Option<&str>) -> Vec<String> {
    if arg.parse::<usize>().is_ok() && favorites_error.is_none() && count > 0 {
        return vec![match count {
            1 => "There is only 1 favorite.".to_string(),
            n => format!("There are only {n} favorites."),
        }];
    }
    let mut lines = vec![format!("No favorite matches \"{arg}\".")];
    // `stream.example.com/live` or `host:8000`: an address without its scheme
    let looks_like_address = !arg.contains(' ')
        && arg.contains('.')
        && (arg.contains('/')
            || arg
                .rsplit_once(':')
                .is_some_and(|(_, port)| port.parse::<u16>().is_ok()));
    if looks_like_address {
        lines.push("If this is an address, start it with http:// or https://".into());
    } else if let Some(error) = favorites_error {
        lines.push(format!("Can't read your favorites: {error}"));
    } else if count == 0 {
        lines.push(
            "There are no favorites yet: search for a station with /, or give a stream address."
                .into(),
        );
    } else {
        lines.push("Run radiotrope-cli without a station to see your favorites.".into());
    }
    lines
}

/// A flag set by SIGTERM, SIGHUP (the terminal window closed) or SIGINT,
/// which the main loop checks so it leaves the normal way: the recording
/// is saved and the terminal restored. A second signal ends at once.
#[cfg(unix)]
fn quit_on_signals() -> Arc<AtomicBool> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

    let quitting = Arc::new(AtomicBool::new(false));
    for sig in [SIGTERM, SIGINT, SIGHUP] {
        // Keep a signal ignored by whoever started us (`nohup`) ignored
        // Safety: with no new action, sigaction only reads the current one
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let read = unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) } == 0;
        if read && old.sa_sigaction == libc::SIG_IGN {
            continue;
        }
        // Checked before it is set: only the second signal ends us
        let _ = signal_hook::flag::register_conditional_shutdown(sig, 1, quitting.clone());
        if let Err(e) = signal_hook::flag::register(sig, quitting.clone()) {
            log::warn!("Can't watch for signal {sig}: {e}");
        }
    }
    quitting
}

/// Windows: Ctrl+C reaches the player as a key, and there are no other
/// signals to watch
#[cfg(not(unix))]
fn quit_on_signals() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// What [`wait_for_event`] found
#[derive(Debug, PartialEq, Eq)]
enum Wait {
    /// An event can be read without blocking
    Event,
    /// Nothing came in time, or a signal cut the wait short
    Idle,
    /// The terminal hung up
    Gone,
}

/// Wait up to `timeout` for a key (or another terminal event)
#[cfg(unix)]
fn wait_for_event(timeout: Duration) -> io::Result<Wait> {
    // crossterm reads a terminal that has hung up in an endless loop (each
    // read returns 0 bytes), and the loop would never see the quit flag. So
    // poll(2) looks first, and crossterm only reads a terminal still there.
    if stdin_wait(Duration::ZERO)? == Some(Wait::Gone) {
        return Ok(Wait::Gone);
    }
    // Events crossterm already holds, or bytes waiting now
    if poll_events(Duration::ZERO)? {
        return Ok(Wait::Event);
    }
    match stdin_wait(timeout)? {
        Some(Wait::Gone) => Ok(Wait::Gone),
        Some(_) if poll_events(Duration::ZERO)? => Ok(Wait::Event),
        _ => Ok(Wait::Idle),
    }
}

#[cfg(not(unix))]
fn wait_for_event(timeout: Duration) -> io::Result<Wait> {
    Ok(if poll_events(timeout)? {
        Wait::Event
    } else {
        Wait::Idle
    })
}

/// crossterm's poll, with a signal counted as no event
fn poll_events(timeout: Duration) -> io::Result<bool> {
    match event::poll(timeout) {
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(false),
        other => other,
    }
}

/// poll(2) on stdin (the terminal, checked at start): `Some(Event)` when
/// there is input, `Some(Gone)` when it hung up, `None` on timeout or signal
#[cfg(unix)]
fn stdin_wait(timeout: Duration) -> io::Result<Option<Wait>> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    // Safety: one valid pollfd, for the length given
    match unsafe { libc::poll(&mut fd, 1, millis) } {
        -1 => match io::Error::last_os_error() {
            e if e.kind() == io::ErrorKind::Interrupted => Ok(None),
            e => Err(e),
        },
        0 => Ok(None),
        _ if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 => {
            Ok(Some(Wait::Gone))
        }
        _ => Ok(Some(Wait::Event)),
    }
}

/// The app's settings, read fresh (the recording folder may have changed)
fn read_settings() -> Settings {
    library::settings_path()
        .as_deref()
        .map(library::read_settings)
        .unwrap_or_default()
}

fn apply_effect(
    effect: Effect,
    view: &mut View,
    player: &mut Player,
    search_worker: Option<&SearchWorker>,
    favorites_worker: Option<&FavoritesWorker>,
) -> bool {
    match effect {
        Effect::None => {}
        Effect::Quit => return false,
        Effect::CloseSearch => {
            if let Some(worker) = search_worker {
                worker.clear_pending();
            }
        }
        Effect::Search(request) => {
            let result = search_worker
                .ok_or_else(|| "Search worker unavailable".to_owned())
                .and_then(|worker| worker.request(request.clone()));
            if let Err(error) = result {
                view.search.apply(search::Reply {
                    request,
                    result: Err(error),
                });
            }
        }
        Effect::Edit(edit) => {
            let result = favorites_worker
                .ok_or_else(|| "Favorites worker unavailable".to_owned())
                .and_then(|worker| worker.edit(edit));
            if let Err(error) = result {
                view.apply_favorites(favorites::Reply::Edited(Err(error)));
            }
        }
        Effect::Play => {
            if let Some(station) = view.highlighted() {
                player.play(station.clone());
            }
        }
        Effect::TogglePlayback => {
            if player.is_active() {
                player.stop();
            } else if let Some(station) = player.station.as_ref().or_else(|| view.highlighted()) {
                player.play(station.clone());
            }
        }
        Effect::StepFavorite(delta) => step_station(view, player, delta),
        Effect::Volume(direction) => player.change_volume(direction as f32 * VOLUME_STEP),
        Effect::Mute => player.toggle_mute(),
        Effect::Record => player.toggle_recording(&read_settings()),
    }
    true
}

/// Play the favorite after (or before) the one playing, round the list
fn step_station(view: &mut View, player: &mut Player, delta: isize) {
    let count = view.favorites.len() as isize;
    if count == 0 {
        return;
    }
    let from = player
        .station
        .as_ref()
        .and_then(|s| view.index_of(&s.url))
        .or_else(|| view.cursor())
        .map(|i| i as isize);
    let next = match from {
        Some(i) => (i + delta).rem_euclid(count),
        None => 0,
    } as usize;
    if !view.search.open {
        view.select(next);
    }
    player.play(view.favorites[next].clone());
}

/// Raw mode and the alternate screen while it lives; dropping it restores
/// both, and stderr if it was silenced
struct TerminalGuard;

/// Whether raw mode and the alternate screen may be on, for the panic hook
static SCREEN_TAKEN: AtomicBool = AtomicBool::new(false);

/// The stderr saved by [`quiet_stderr`], or -1, for the panic hook
#[cfg(unix)]
static SAVED_STDERR: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let guard = TerminalGuard;
        SCREEN_TAKEN.store(true, Ordering::SeqCst);
        terminal::enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Leave raw mode and the alternate screen and give stderr back. Safe to
/// call more than once, and before the screen was taken.
fn restore_terminal() {
    if SCREEN_TAKEN.swap(false, Ordering::SeqCst) {
        let _ = terminal::disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
    #[cfg(unix)]
    restore_stderr(SAVED_STDERR.swap(-1, Ordering::SeqCst));
}

/// Send stderr to /dev/null, returning a copy of the old stderr (or -1)
#[cfg(unix)]
fn quiet_stderr() -> io::Result<libc::c_int> {
    use std::os::unix::io::AsRawFd;

    let saved_stderr = unsafe { libc::dup(2) };
    let devnull = std::fs::File::open("/dev/null")?;
    unsafe { libc::dup2(devnull.as_raw_fd(), 2) };
    Ok(saved_stderr)
}

/// Put back the stderr [`quiet_stderr`] saved
#[cfg(unix)]
fn restore_stderr(saved_stderr: libc::c_int) {
    if saved_stderr >= 0 {
        unsafe {
            libc::dup2(saved_stderr, 2);
            libc::close(saved_stderr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_match_says_what_to_do() {
        assert_eq!(no_match("7", 1, None), ["There is only 1 favorite."]);
        assert_eq!(no_match("7", 3, None), ["There are only 3 favorites."]);
        for address in ["stream.example.com/live", "radio.test:8000"] {
            assert_eq!(
                no_match(address, 3, None)[1],
                "If this is an address, start it with http:// or https://"
            );
        }
        assert!(no_match("jazz", 0, None)[1].starts_with("There are no favorites yet"));
        assert!(no_match("jazz", 2, Some("Invalid favorites: x"))[1]
            .starts_with("Can't read your favorites"));
        assert_eq!(
            no_match("jazz radio", 2, None),
            [
                "No favorite matches \"jazz radio\".",
                "Run radiotrope-cli without a station to see your favorites."
            ]
        );
    }

    #[test]
    fn several_words_name_one_station() {
        let cli = Cli::parse_from(["radiotrope-cli", "radio", "paradise"]);
        assert_eq!(cli.station().as_deref(), Some("radio paradise"));
        assert_eq!(Cli::parse_from(["radiotrope-cli"]).station(), None);
    }
}
