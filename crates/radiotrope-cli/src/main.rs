//! Radiotrope CLI: the terminal internet radio player
//!
//! Plays your favorites from the Radiotrope app, or a stream URL, with the
//! station's details, a spectrum and recording. It reads the app's
//! favorites and settings and never writes them.

mod library;
mod player;
mod recording;
mod ui;
mod visual;

use std::io;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;

use radiotrope::audio::AudioEngine;

use library::{Settings, Station};
use player::{Phase, Player, VOLUME_STEP};
use ui::{ColorMode, Palette, View};

/// How often the favorites file is checked for changes the app saved
const FAVORITES_CHECK: Duration = Duration::from_secs(2);

/// Rows `PgUp` and `PgDn` move
const PAGE: isize = 10;

#[derive(Parser)]
#[command(
    name = "radiotrope-cli",
    about = "Terminal internet radio player: your Radiotrope favorites, or any stream URL",
    version
)]
struct Cli {
    /// A favorite's number, part of its name, or a stream URL to play at
    /// start. Without it, the player opens on your favorites.
    station: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let favorites_path = library::favorites_path();
    let favorites = favorites_path
        .as_deref()
        .map(library::read_favorites)
        .unwrap_or_default();
    let settings = read_settings();

    let start = match cli.station.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(arg) if arg.contains("://") => Some(Station {
            name: String::new(),
            url: arg.to_string(),
            country: None,
        }),
        Some(arg) => match library::find_favorite(&favorites, arg) {
            Some(station) => Some(station.clone()),
            None => {
                eprintln!("No favorite matches \"{arg}\".");
                if favorites.is_empty() {
                    eprintln!("There are no favorites yet: add some in the Radiotrope app, or give a stream URL.");
                } else {
                    eprintln!("Run radiotrope-cli without a station to see your favorites.");
                }
                std::process::exit(2);
            }
        },
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

    // The guard puts the terminal back however this ends: a quit, an
    // error returned by `?`, or a panic
    let terminal_guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut favorites_seen = favorites_path.as_deref().and_then(library::modified);
    let mut favorites_checked = Instant::now();
    loop {
        player.poll();
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
                if key.kind == KeyEventKind::Press && !handle_key(key, &mut view, &mut player) {
                    break;
                }
            }
        }

        if favorites_checked.elapsed() >= FAVORITES_CHECK {
            favorites_checked = Instant::now();
            if let Some(path) = favorites_path.as_deref() {
                let stamp = library::modified(path);
                if stamp != favorites_seen {
                    favorites_seen = stamp;
                    view.replace_favorites(library::read_favorites(path));
                }
            }
        }
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

/// Act on a key; false to quit
fn handle_key(key: KeyEvent, view: &mut View, player: &mut Player) -> bool {
    match key.code {
        // Raw mode turns Ctrl+C into a key press
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
        KeyCode::Char('q') | KeyCode::Esc => return false,
        KeyCode::Up | KeyCode::Char('k') => view.move_cursor(-1),
        KeyCode::Down | KeyCode::Char('j') => view.move_cursor(1),
        KeyCode::PageUp => view.move_cursor(-PAGE),
        KeyCode::PageDown => view.move_cursor(PAGE),
        KeyCode::Home => view.move_cursor(isize::MIN / 2),
        KeyCode::End => view.move_cursor(isize::MAX / 2),
        KeyCode::Enter => {
            if let Some(i) = view.cursor() {
                player.play(view.favorites[i].clone());
            }
        }
        KeyCode::Char(' ') => {
            if player.is_active() {
                player.stop();
            } else if let Some(station) = player.station.clone() {
                player.play(station);
            } else if let Some(i) = view.cursor() {
                player.play(view.favorites[i].clone());
            }
        }
        KeyCode::Char('n') => step_station(view, player, 1),
        KeyCode::Char('p') => step_station(view, player, -1),
        KeyCode::Char('+') | KeyCode::Char('=') => player.change_volume(VOLUME_STEP),
        KeyCode::Char('-') | KeyCode::Char('_') => player.change_volume(-VOLUME_STEP),
        KeyCode::Char('m') => player.toggle_mute(),
        KeyCode::Char('r') => player.toggle_recording(&read_settings()),
        _ => {}
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
    view.select(next);
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
