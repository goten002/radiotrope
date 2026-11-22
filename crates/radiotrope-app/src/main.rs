// On Windows the release build is a GUI program, so no console window opens
// next to it. Debug builds keep the console for their logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod about_ui;
mod agents_ui;
mod app;
mod args;
#[cfg(feature = "desktop")]
mod autostart;
mod browse_ui;
mod caches;
mod context;
mod control;
mod favorites_list;
mod favorites_ui;
#[cfg(any(test, feature = "ui-snapshots"))]
mod headless;
mod import_ui;
mod instance;
mod log_file;
mod mcp;
mod player_ui;
mod quit_ui;
mod recording_ui;
mod remote;
mod remote_ui;
mod row_logos;
mod schedule_ui;
mod shortcuts;
mod stats_ui;
#[cfg(feature = "desktop")]
mod tray;
mod viz_ui;
mod window_settings;
// The tests open every screen of the snapshot tool
#[cfg(any(test, feature = "ui-snapshots"))]
#[cfg_attr(not(feature = "ui-snapshots"), allow(dead_code))]
mod ui_snapshots;
#[cfg(feature = "embedded")]
mod wifi_ui;
#[cfg(feature = "desktop")]
mod window_frame;
#[cfg(windows)]
mod windows_console;

slint::include_modules!();

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use crossbeam_channel::bounded;
use slint::Model;

use radiotrope::audio::{AudioAnalysis, HissLevel, LevelTarget, SharedStats};

use radiotrope_app::config::ui::{SHUTDOWN_GRACE, SHUTDOWN_SEND_TIMEOUT};
use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::types::Station;
use radiotrope_app::network::browse_logos::BrowseLogos;
use radiotrope_app::network::logo::LogoService;

use app::controller::AppController;
use app::listening::ListenSession;
use app::state::AppSnapshot;
use app::ui_sender::UiSender;
use args::Args;
use caches::{migrate_logo_ids, prune_caches_daily};
use context::{lock, AppContext};
use favorites_list::refresh_favorites;
use favorites_ui::{credit_listening, show_edited_favorite, EditedFavorite};
use recording_ui::{save_recording_settings, station_cover_png};
use window_settings::save_settings_at_exit;

// What the snapshot tool takes from the crate root
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use about_ui::{author_names, license_label};
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use agents_ui::show_agent_lines;
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use browse_ui::flag_image;
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use player_ui::UNNAMED_STATION;
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use stats_ui::{clear_stats_ui, show_health};
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use window_settings::show_eq_presets;

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

fn main() {
    #[cfg(windows)]
    windows_console::set_up();
    log_file::log_panics();

    init_logging();
    let args = Args::parse();

    #[cfg(feature = "ui-snapshots")]
    if let Some(out_dir) = args.ui_snapshots {
        let options = ui_snapshots::Options {
            theme: args.theme.unwrap_or_default(),
            scale: args.scale,
            size: args.size.unwrap_or_default(),
            only: args.only,
        };
        if let Err(error) = ui_snapshots::run(&out_dir, &options) {
            eprintln!("UI snapshots: {error}");
            std::process::exit(1);
        }
        return;
    }

    // `--mcp` relays the agent to the running player and never opens a
    // window of its own
    if args.mcp && !args.standalone {
        std::process::exit(mcp::local::run_relay());
    }

    // `--quit` closes the running player, e.g. for an installer
    if args.quit {
        std::process::exit(match instance::ask_to_quit() {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("Can't close Radiotrope: {e}");
                1
            }
        });
    }

    // Started from the desktop, with no terminal to read it: the window's
    // messages go to the log file. Not with `--mcp`, whose client reads
    // what we print.
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    if !args.mcp {
        log_file::set_up();
    }

    // One player per user; a second launch brings the first one forward
    let instance = if args.standalone {
        None
    } else {
        match instance::acquire() {
            instance::Acquire::Primary(guard) => Some(Arc::new(guard)),
            instance::Acquire::Running => {
                if !instance::ask_to_show() {
                    eprintln!("Radiotrope is already running");
                }
                return;
            }
            instance::Acquire::Unavailable(e) => {
                eprintln!("Single instance check unavailable, agents can't connect: {e}");
                None
            }
        }
    };

    // Shared command channel + state
    let (cmd_tx, cmd_rx) = bounded(64);
    // What the UI thread sends never blocks it, even when the controller
    // is stuck
    let ui_tx = UiSender::new(cmd_tx.clone());
    let shared_state = Arc::new(Mutex::new(AppSnapshot::default()));

    // Channel for the engine's analysis Arc (one-shot handshake)
    let (analysis_tx, analysis_rx) = bounded::<Arc<Mutex<AudioAnalysis>>>(1);

    // Channel for the engine's SharedStats (one-shot handshake)
    let (stats_tx, stats_rx) = bounded::<SharedStats>(1);

    // Favorites manager + shared logo service (created early so MCP can use them)
    // A favorites file that can't be read starts an empty list (the file
    // itself is kept aside by the loader), and the logo cache is left alone
    let (favorites, favorites_loaded) = match FavoritesManager::load() {
        Ok(f) => (f, true),
        Err(e) => {
            eprintln!("Favorites: {e}");
            (FavoritesManager::new(), false)
        }
    };
    let favorites = Arc::new(Mutex::new(favorites));
    // Only its web client can fail (a cache folder that can't be made
    // leaves it without one); with no client nothing could be fetched
    let logo_service = match LogoService::new() {
        Ok(service) => Arc::new(service),
        Err(e) => start_up_failed(&format!("Radiotrope can't start its web client: {e}")),
    };
    // Listening time is saved a moment after it is added, on a thread of
    // its own: the UI doesn't wait on the disk once a minute
    let favorites_saver = favorites_ui::start_saver(&favorites);

    // `--mcp --standalone`: serve this agent on stdio, in this process
    if args.mcp {
        let mcp_tx = cmd_tx.clone();
        let mcp_state = shared_state.clone();
        let mcp_favs = favorites.clone();
        std::thread::Builder::new()
            .name("mcp-stdio".into())
            .spawn(move || {
                mcp::server::run(mcp_tx, mcp_state, mcp_favs);
            })
            .expect("Failed to spawn MCP thread");
    }

    // Load settings
    let mut settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
    // A preset that has since been renamed or retired: its saved band
    // gains still play, as a custom curve
    settings.eq_preset_name = settings
        .eq_preset_name
        .take()
        .filter(|name| radiotrope::audio::find_preset(name).is_some());
    {
        let mut state = lock(&shared_state);
        state.volume = settings.volume;
        state.is_muted = settings.muted;
        state.eq_gains = settings.eq_gains;
        state.eq_preamp = settings.eq_preamp;
        state.eq_preamp_moved = settings.eq_preamp_moved;
        state.eq_enabled = settings.eq_enabled;
        state.eq_preset_name = settings.eq_preset_name.clone();
        state.match_volume = settings.match_volume;
        state.level_target = LevelTarget::parse(&settings.match_level).unwrap_or_default();
        state.hiss = HissLevel::parse(&settings.hiss_reducer).unwrap_or_default();
        state.hum_filter = settings.hum_filter;
        state.rumble_filter = settings.rumble_filter;
        state.smooth_switching = settings.smooth_switching;
        state.accent_color = settings.accent_color.clone();
        state.recording_setup = app::state::RecordingSetup::from_settings(&settings);
    }

    // Create Slint UI
    let ui = App::new().unwrap_or_else(|e| start_up_failed(&window_error(&e)));
    // What the window's handlers share
    let ctx = AppContext {
        ui: ui.as_weak(),
        tx: ui_tx.clone(),
        state: shared_state.clone(),
        favorites: favorites.clone(),
        logos: logo_service.clone(),
    };
    shortcuts::install(&ui);
    ui.on_locate_favorite(|model, highlighted, playing, action, page| {
        shortcuts::favorite_highlight(
            model.iter().map(|station| station.url),
            highlighted.as_str(),
            playing.as_str(),
            action,
            page.max(0) as usize,
        )
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1)
    });

    // Wayland app_id / X11 WM_CLASS: desktops match it against the
    // `app.radiotrope.Radiotrope.desktop` file (and its StartupWMClass) to
    // find the icon. A no-op on Windows and macOS.
    let _ = slint::set_xdg_app_id(radiotrope_app::config::APP_ID);

    // Agents (`radiotrope --mcp`) and later launches reach us over a local
    // socket
    let agents = instance.as_ref().map(|instance| {
        let agents = Arc::new(mcp::agents::Agents::new(mcp::tools::RadioTools::new(
            cmd_tx.clone(),
            shared_state.clone(),
            favorites.clone(),
        )));
        let instance = Arc::clone(instance);
        let tools = agents.tools();
        let window = ui.as_weak();
        let show_window: mcp::local::ShowWindow = Arc::new(move || {
            let _ = window.upgrade_in_event_loop(|ui| {
                // A window that is open but covered is raised, which
                // asking for the focus doesn't do on Wayland
                #[cfg(feature = "desktop")]
                if window_frame::raise_shown(ui.window()) {
                    return;
                }
                bring_to_front(ui.window());
            });
        });
        // `radiotrope --quit`: exit as the menu's Quit does
        let quit: mcp::local::QuitApp = Arc::new(|| {
            let _ = slint::invoke_from_event_loop(quit_app);
        });
        std::thread::Builder::new()
            .name("mcp-local".into())
            .spawn(move || {
                mcp::local::serve(&instance, tools, show_window, quit);
            })
            .expect("Failed to spawn MCP thread");
        agents
    });
    // Network agents, when turned on, and the Agents dialog
    let _network_timer = agents_ui::setup(&ui, agents.clone(), &settings);
    let _agents_timer = agents_ui::watch(&ui, agents.as_ref().map(|a| a.tools().presence()));
    // Phones (Radiotrope Remote), when turned on, in the main player only
    let remote = instance.as_ref().map(|_| {
        Arc::new(remote::Remote::new(
            control::Control::new(cmd_tx.clone(), shared_state.clone(), favorites.clone()),
            Some(logo_service.clone()),
            remote_window_hooks(&ui, &favorites, &logo_service, &shared_state),
        ))
    });
    let _remote_timer = remote_ui::setup(&ui, remote, &settings);

    // Initial load of favorites into UI model
    migrate_logo_ids(&favorites, settings.last_station.as_ref(), &logo_service);
    refresh_favorites(&ui, &favorites, &logo_service);

    // The saved settings and the station of the last run
    window_settings::apply(&ui, &settings);
    player_ui::setup_header(&ui, &ctx, settings.last_station.as_ref());

    // Send initial volume/mute to controller so engine starts at the correct level
    {
        ui_tx.send(app::state::AppCommand::SetVolume(settings.volume));
        if settings.muted {
            ui_tx.send(app::state::AppCommand::Mute);
        }
    }

    // Send initial EQ state to controller (which will forward to engine once started)
    for command in app::state::AppCommand::restore_eq(&settings) {
        ui_tx.send(command);
    }
    for command in app::state::AppCommand::restore_sound(&settings) {
        ui_tx.send(command);
    }

    // What the window's controls do
    player_ui::setup(&ui, &ctx);
    favorites_ui::setup(&ui, &ctx);
    window_settings::setup(&ui, &ctx);
    // WiFi settings and Power Off / Restart (Raspberry Pi build only)
    #[cfg(feature = "embedded")]
    wifi_ui::setup(&ui);
    #[cfg(feature = "embedded")]
    wifi_ui::setup_power(&ui);

    about_ui::setup(&ui);
    setup_text_util(&ui);
    recording_ui::setup(&ui, &ctx, &settings);
    schedule_ui::setup(&ui, &settings, ui_tx.clone(), shared_state.clone());

    // Logos of the browser rows on screen, kept small on disk
    let browse_logos = Arc::new(BrowseLogos::open());
    // The caches are trimmed now (the logos the favorites lack are being
    // fetched already, and the restored station is in the shared state)
    // and then once a day
    // Favorite logos downloaded again by the daily check show at once
    let logos_refreshed = {
        let ui_weak = ui.as_weak();
        let favorites = favorites.clone();
        let logo_service = logo_service.clone();
        move || {
            let favorites = favorites.clone();
            let logo_service = logo_service.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                refresh_favorites(&ui, &favorites, &logo_service);
            });
        }
    };
    prune_caches_daily(
        favorites.clone(),
        favorites_loaded,
        shared_state.clone(),
        logo_service.clone(),
        browse_logos.clone(),
        logos_refreshed,
    );
    browse_ui::setup(&ui, &ctx, browse_logos);
    import_ui::setup(&ui, &ctx);

    // Spawn controller on its own thread
    let ctrl_state = shared_state.clone();
    let ctrl_tx = cmd_tx.clone();
    // A scheduled recording's cover art, as a recording started here gets
    let cover_logos = logo_service.clone();
    let cover_source: app::controller::CoverSource = Box::new(move |station| {
        station_cover_png(&cover_logos, &Station::new(&station.name, &station.url))
    });
    let schedule_path = radiotrope_app::data::data_path(radiotrope_app::data::schedule::FILE_NAME);
    let controller = std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || {
            let mut ctrl = AppController::new(cmd_rx, ctrl_tx, ctrl_state, analysis_tx, stats_tx)
                .with_cover_source(cover_source);
            match schedule_path {
                Ok(path) => ctrl = ctrl.with_schedule(path),
                Err(e) => eprintln!("Schedule unavailable: {e}"),
            }
            ctrl.run();
        })
        .expect("Failed to spawn controller thread");

    // The engine sends its analysis Arc and SharedStats once its audio
    // device is open, which can take a while (a Bluetooth device waking
    // up). The window doesn't wait for them: the timers below pick them up
    // when they come.

    let viz_timer = viz_ui::start_timer(&ui, analysis_rx);
    let engine_stats = stats_ui::setup(&ui, &shared_state, stats_rx, &viz_timer);

    // Listening session of the station playing now, for favorite stats
    let listen_session: std::rc::Rc<std::cell::RefCell<Option<ListenSession>>> = Default::default();
    let listen_favs = favorites.clone();
    let _favorites_timers = favorites_ui::start_timers(&ui, &favorites);
    let _poll_timer = player_ui::start_poll(
        &ui,
        &ctx,
        settings.last_station.as_ref(),
        &viz_timer,
        &engine_stats,
        &listen_session,
        &favorites_saver,
    );

    // Quit like a closed window on SIGTERM (systemd on stop or reboot),
    // SIGINT (Ctrl+C) and SIGHUP (the terminal closed), so the exit below
    // saves and finishes a recording
    #[cfg(unix)]
    quit_on_signals();

    // Closing the window ends the app, or only hides it when View > Close
    // to Tray is on and the tray shows our icon. Closing that would stop a
    // recording or skip a Scheduler entry asks first, as Quit does.
    quit_ui::setup(&ui, &shared_state);
    {
        let ui_weak = ui.as_weak();
        let state = shared_state.clone();
        ui.window().on_close_requested(move || {
            let Some(ui) = ui_weak.upgrade() else {
                quit_app();
                return slint::CloseRequestResponse::HideWindow;
            };
            #[cfg(feature = "desktop")]
            if tray::hides_on_close(&ui) {
                return slint::CloseRequestResponse::HideWindow;
            }
            quit_ui::request_quit(&ui, &state);
            if quit_ui::asking(&ui) {
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        });
    }

    // The tray icon, and whether the desktop shows one (GNOME only with
    // the AppIndicator extension)
    #[cfg(feature = "desktop")]
    let start_hidden = {
        let available = tray::host::available();
        ui.set_tray_available(available);
        let ui_weak = ui.as_weak();
        tray::host::watch(move |available| {
            let _ = ui_weak.upgrade_in_event_loop(move |ui| ui.set_tray_available(available));
        });
        tray::start(&ui);
        setup_start_at_login(&ui);
        let start_hidden = settings.start_in_tray && settings.tray_icon && available;
        if start_hidden {
            let ui_weak = ui.as_weak();
            let _ = slint::spawn_local(async move {
                use slint::winit_030::WinitWindowAccessor;
                let Some(ui) = ui_weak.upgrade() else {
                    return;
                };
                if ui.window().winit_window().await.is_ok() {
                    window_frame::drop_hidden_window(ui.window());
                }
            });
        }
        start_hidden
    };
    #[cfg(not(feature = "desktop"))]
    let start_hidden = false;

    // Run Slint event loop (blocks main thread) until Quit, or a close
    // that doesn't go to the tray; a hidden window keeps it going. An
    // error (the display went away, e.g. at logout) ends it like closing
    // the window does.
    if !start_hidden {
        if let Err(e) = ui.show() {
            start_up_failed(&window_error(&e));
        }
    }
    if let Err(e) = slint::run_event_loop_until_quit() {
        eprintln!("Event loop ended: {e}");
    }
    let _ = ui.hide();

    // Tell the controller first, so a recording starts finishing while the
    // rest is saved. A controller stuck on its audio device doesn't take
    // it, and isn't waited for.
    let shutdown_sent = ui_tx.shutdown(SHUTDOWN_SEND_TIMEOUT);
    let deadline = Instant::now() + SHUTDOWN_GRACE;

    // Credit the session still playing at exit, and save what is waiting
    // to be saved
    if let Some(session) = listen_session.borrow_mut().take() {
        credit_listening(&ui, &listen_favs, &favorites_saver, session);
    }
    favorites_ui::save_at_exit(&favorites, &favorites_saver);

    // Final save before shutdown
    save_settings_at_exit(&shared_state, &ui);

    // Give the controller time to finish a recording in progress, so the
    // end of the file is written before the process exits
    while shutdown_sent && !controller.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// View > System Tray > Start at Login: the tick shows what the autostart
/// file or Run key says, also after a change that failed
#[cfg(feature = "desktop")]
fn setup_start_at_login(ui: &App) {
    ui.set_start_at_login(autostart::is_enabled());
    let ui_weak = ui.as_weak();
    ui.on_start_at_login_changed(move |on| {
        if let Err(e) = autostart::set_enabled(on) {
            eprintln!("Start at Login: {e}");
        }
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_start_at_login(autostart::is_enabled());
        }
    });
}

/// What to tell a user whose window didn't open (no display, no usable
/// graphics driver: a remote session, a VM)
fn window_error(error: &dyn std::fmt::Display) -> String {
    format!(
        "Radiotrope can't open its window: {error}. \
         Try `SLINT_BACKEND=winit-software radiotrope`."
    )
}

/// Say why the player can't start, in words rather than a panic, and end
/// it. On Windows, whose release build has no terminal, in a message box
/// too.
fn start_up_failed(message: &str) -> ! {
    eprintln!("{message}");
    #[cfg(all(windows, feature = "desktop"))]
    {
        let _ = rfd::MessageDialog::new()
            .set_title("Radiotrope")
            .set_description(message)
            .set_level(rfd::MessageLevel::Error)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
    std::process::exit(1);
}

/// Quit the event loop on SIGTERM, SIGINT or SIGHUP. The signals are
/// taken on a thread of their own (a signal handler may not touch the
/// event loop). A second one while the first is being handled ends the
/// process at once, so a stuck exit can still be interrupted. A signal
/// ignored by whoever started us (`nohup`, a script's background job)
/// stays ignored.
#[cfg(unix)]
fn quit_on_signals() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use std::sync::atomic::AtomicBool;

    let ignored = |sig: libc::c_int| {
        // Safety: with no new action, sigaction only reads the current one
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let read = unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) } == 0;
        read && old.sa_sigaction == libc::SIG_IGN
    };
    let signals: Vec<_> = [SIGTERM, SIGINT, SIGHUP]
        .into_iter()
        .filter(|&sig| !ignored(sig))
        .collect();
    let quitting = Arc::new(AtomicBool::new(false));
    for &sig in &signals {
        // Checked before it is set: only the second signal ends us
        let _ = signal_hook::flag::register_conditional_shutdown(sig, 1, quitting.clone());
        let _ = signal_hook::flag::register(sig, quitting.clone());
    }
    match signal_hook::iterator::Signals::new(signals) {
        Ok(mut signals) => {
            let _ = std::thread::Builder::new()
                .name("signals".into())
                .spawn(move || {
                    for _ in signals.forever() {
                        let _ = slint::quit_event_loop();
                    }
                });
        }
        Err(e) => eprintln!("Signal handling unavailable: {e}"),
    }
}

/// End the app: the event loop stops, and the exit saves and finishes a
/// recording
fn quit_app() {
    #[cfg(feature = "desktop")]
    tray::quit();
    #[cfg(not(feature = "desktop"))]
    let _ = slint::quit_event_loop();
}

/// Show the window and ask for focus (a second launch of radiotrope, an
/// agent, the tray icon)
fn bring_to_front(window: &slint::Window) {
    let _ = window.show();
    #[cfg(feature = "desktop")]
    window_frame::bring_to_front(window);
}

/// Text helpers the UI calls back into
fn setup_text_util(ui: &App) {
    ui.global::<TextUtil>()
        .on_steady_digits(|text| radiotrope_app::text::steady_digits(&text).into());
}

/// What the window does when a phone changes the accent or the recording
/// settings, or edits a favorite; all run on the window's thread
fn remote_window_hooks(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
    shared_state: &Arc<Mutex<AppSnapshot>>,
) -> remote::WindowHooks {
    let accent_ui = ui.as_weak();
    let edit_ui = ui.as_weak();
    let favs = favorites.clone();
    let logo_svc = logo_service.clone();
    let state = shared_state.clone();
    let recording_ui = ui.as_weak();
    let recording_state = shared_state.clone();
    remote::WindowHooks {
        accent: Some(Box::new(move |hex: String| {
            let Some((r, g, b)) = radiotrope_app::data::settings::parse_hex_rgb(&hex) else {
                return;
            };
            let _ = accent_ui.upgrade_in_event_loop(move |ui| {
                ui.set_accent_color(slint::Color::from_rgb_u8(r, g, b));
            });
        })),
        favorite_edited: Some(Box::new(move |old, new| {
            let favs = favs.clone();
            let logo_svc = logo_svc.clone();
            let state = state.clone();
            let _ = edit_ui.upgrade_in_event_loop(move |ui| {
                let s = &new.station;
                let id = old.id();
                let edited = EditedFavorite {
                    old: Some(&old),
                    id: &id,
                    name: &s.name,
                    url: &s.url,
                    logo_url: s.logo_url.as_deref().unwrap_or(""),
                    country: s.country.as_deref().unwrap_or(""),
                };
                show_edited_favorite(&ui, &favs, &logo_svc, &state, edited);
            });
        })),
        recording_settings: Some(Box::new(move |change| {
            let state = recording_state.clone();
            let _ = recording_ui.upgrade_in_event_loop(move |ui| {
                save_recording_settings(&state, |s| change.apply(s));
                if let Some(format) = change.format {
                    ui.set_recording_format(format.id().into());
                }
                if let Some(bitrate) = change.bitrate {
                    ui.set_recording_bitrate(bitrate.unwrap_or(0) as i32);
                }
                if let Some(on) = change.with_eq {
                    ui.set_record_with_eq(on);
                }
            });
        })),
    }
}
