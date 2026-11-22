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

use std::io;
#[cfg(unix)]
use std::sync::atomic::Ordering;

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    let cli = Cli::parse();

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

    let start = match cli.station.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(arg) if arg.contains("://") => Some(
            favorites
                .iter()
                .find(|station| station.url == arg)
                .cloned()
                .unwrap_or_else(|| Station::new("", arg)),
        ),
        Some(arg) => {
            match library::find_favorite(&favorites, arg) {
                Some(station) => Some(station.clone()),
                None => {
                    eprintln!("No favorite matches \"{arg}\".");
                    if let Some(error) = &favorites_error {
                        eprintln!("Favorites: {error}");
                    } else if favorites.is_empty() {
                        eprintln!("There are no favorites yet: open station search, or give a stream URL.");
                    } else {
                        eprintln!("Run radiotrope-cli without a station to see your favorites.");
                    }
                    std::process::exit(2);
                }
            }
        }
    };

    let engine = match AudioEngine::new() {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Audio error: {e}");
            std::process::exit(1);
        }
    };
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
    } else {
        view.favorites_error = favorites_error;
    }

    // The guard puts the terminal back however this ends: a quit, an
    // error returned by `?`, or a panic
    let terminal_guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    loop {
        player.poll();
        if let Ok(worker) = &search_worker {
            view.search.poll(worker);
        }
        if let Some(worker) = &favorites_worker {
            view.poll_favorites(worker);
        }
        if player.phase == Phase::Playing {
            let mut a = analysis.lock().unwrap_or_else(|e| e.into_inner());
            view.spectrum.update(Some(&mut a));
        } else {
            view.spectrum.update(None);
        }

        terminal.draw(|f| ui::draw(f, &mut view, &player))?;

        if event::poll(ui::frame_interval(&view, &player))? {
            if let Event::Key(key) = event::read()? {
                // Windows also reports key releases
                if key.kind == KeyEventKind::Press
                    && !apply_effect(
                        input::route(key, &mut view),
                        &mut view,
                        &mut player,
                        search_worker.as_ref().ok(),
                        favorites_worker.as_ref(),
                    )
                {
                    break;
                }
            }
        }
    }
    if let Some(worker) = favorites_worker {
        worker.finish();
    }

    // Save any recording and close the engine while the screen is still
    // ours (audio libraries may print on the way out)
    player.shutdown();
    drop(terminal_guard);
    Ok(())
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

/// Raw mode, the alternate screen and (on Unix) a silenced stderr while it
/// lives; dropping it, or a panic, restores all three
struct TerminalGuard;

/// The stderr saved by [`quiet_stderr`], or -1, for the panic hook
#[cfg(unix)]
static SAVED_STDERR: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        // Suppress stderr during TUI — ALSA/PulseAudio and other libs write
        // diagnostic messages to stderr which corrupt the ratatui display.
        // Windows audio (WASAPI) doesn't, so there stderr is left alone.
        #[cfg(unix)]
        SAVED_STDERR.store(quiet_stderr()?, Ordering::SeqCst);

        // Release builds abort on panic, so drops don't run: restore the
        // terminal before the panic message is printed
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            default_hook(info);
        }));

        let guard = TerminalGuard;
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
/// call more than once.
fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
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
