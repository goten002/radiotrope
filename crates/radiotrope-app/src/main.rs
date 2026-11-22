// On Windows the release build is a GUI program, so no console window opens
// next to it. Debug builds keep the console for their logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod about_ui;
mod agents_ui;
mod app;
mod args;
mod caches;
mod context;
mod control;
#[cfg(any(test, feature = "ui-snapshots"))]
mod headless;
mod instance;
mod mcp;
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use crossbeam_channel::bounded;
use slint::{Model, ModelRc, SharedPixelBuffer, VecModel};

use radiotrope::audio::{AudioAnalysis, HissLevel, LevelTarget, PlaybackState, SharedStats};

use radiotrope_app::config::ui::{
    FAVORITES_SAVE_DELAY, LISTEN_CREDIT_SECS, MIN_LISTEN_SECS, PLAY_TAKE_TIMEOUT, SEARCH_PAGE_SIZE,
    SETTINGS_SAVE_DELAY, SHUTDOWN_GRACE, SHUTDOWN_SEND_TIMEOUT,
};
use radiotrope_app::data::favorites::{FavoritesManager, ImportMode, ImportOutcome, PlayMetadata};
use radiotrope_app::data::types::{FavoriteSort, Station};
use radiotrope_app::error::ServiceProblem;
use radiotrope_app::network::browse_logos::BrowseLogos;
use radiotrope_app::network::logo::LogoService;
use radiotrope_app::providers::types::{CategoryType, SearchResults, StationFilter};
use radiotrope_app::providers::{RadiotropeProvider, StationProvider};
use radiotrope_app::text::{self, status, SEPARATOR};
use radiotrope_app::visual::{self, logo_palette};

use about_ui::setup_about;
use agents_ui::{setup_agents, watch_agents};
use app::controller::AppController;
use app::delayed_save::DelayedSave;
use app::listening::ListenSession;
use app::picked_station::PendingPick;
use app::shown_station::ShownStation;
use app::state::AppSnapshot;
use app::ui_sender::UiSender;
use args::Args;
use caches::{migrate_logo_ids, prune_caches_daily};
use context::lock;
use recording_ui::{
    save_recording_settings, setup_recording, show_listeners, show_recording_state,
    station_cover_png,
};
use stats_ui::format_playtime;
use viz_ui::viz_wanted;
use window_settings::{
    save_settings, show_filters, window_settings, write_settings, FilterSettings,
};

// What the snapshot tool takes from the crate root
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use about_ui::{author_names, license_label};
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use agents_ui::show_agent_lines;
#[cfg(any(test, feature = "ui-snapshots"))]
pub(crate) use stats_ui::{clear_stats_ui, format_health};
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
    let logo_service = Arc::new(LogoService::new().expect("Failed to create logo service"));
    // Listening time is saved a moment after it is added, on a thread of
    // its own: the UI doesn't wait on the disk once a minute
    let favorites_saver = {
        let favorites = favorites.clone();
        DelayedSave::start("favorites-save", FAVORITES_SAVE_DELAY, move || {
            let mut favs = lock(&favorites);
            if let Err(e) = favs.save() {
                eprintln!("Failed to save favorites: {e}");
            }
        })
    };

    // Generation counter for browse logo fetches (to cancel stale requests)
    let browse_logo_gen = Arc::new(AtomicU64::new(0));

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
    let ui = App::new().unwrap();
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
            let _ = window.upgrade_in_event_loop(|ui| bring_to_front(ui.window()));
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
    let _network_timer = setup_agents(&ui, agents.clone(), &settings);
    let _agents_timer = watch_agents(&ui, agents.as_ref().map(|a| a.tools().presence()));
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

    window_settings::apply(&ui, &settings);
    // Re-sample the visualizer colours whenever the station logo or the
    // theme changes (near-white suits the dark theme, near-black the light)
    {
        let ui_weak = ui.as_weak();
        ui.on_logo_changed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                apply_logo_palette(&ui);
            }
        });
    }

    // Restore last station to UI (so user can hit Play to resume)
    if let Some(ref station) = settings.last_station {
        ui.set_station_name(station.name.as_str().into());
        ui.set_station_url(station.url.as_str().into());
        let shown = show_station(&station.url);
        if let Some(ref logo_url) = station.logo_url {
            ui.set_station_logo_url(logo_url.as_str().into());
        }
        // Check if it's favorited
        {
            let favs = lock(&favorites);
            ui.set_is_station_favorited(favs.is_favorite(&station.url));
        }
        // Try to load cached logo
        if let Some(ref logo_url) = station.logo_url {
            if !logo_url.is_empty() {
                let tmp = Station::new(&station.name, &station.url).with_logo(logo_url);
                if let Some((rgba, w, h)) = logo_service.get_cached_rgba(&tmp) {
                    set_header_logo(&ui, rgba, w, h);
                } else {
                    // Not cached (e.g. removed by an older cleanup): fetch it
                    let logo_svc = logo_service.clone();
                    let ui_weak = ui.as_weak();
                    let station_url = station.url.clone();
                    std::thread::Builder::new()
                        .name("last-logo-fetch".into())
                        .spawn(move || {
                            if let Some((rgba, w, h)) = logo_svc.get_rgba(&tmp) {
                                let _ = slint::invoke_from_event_loop(move || {
                                    let Some(ui) = ui_weak.upgrade() else { return };
                                    if is_current_station(shown, &station_url) {
                                        set_header_logo(&ui, rgba, w, h);
                                    }
                                });
                            }
                        })
                        .ok();
                }
            }
        }
        if let Some(ref country) = station.country {
            ui.set_station_country(country.as_str().into());
        }
        // Update shared state so controller knows about the last station
        {
            let mut s = lock(&shared_state);
            s.station_name = Some(station.name.clone());
            s.station_url = Some(station.url.clone());
            s.station_logo_url = station.logo_url.clone();
            s.station_country = station.country.clone();
        }
    }

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

    // Wire Slint callbacks → ui_tx
    let play_tx = ui_tx.clone();
    let play_url_weak = ui.as_weak();
    let play_url_favs = favorites.clone();
    let play_url_logo_svc = logo_service.clone();
    let play_url_state = shared_state.clone();
    ui.on_play_url(move |url| {
        if let Some(ui) = play_url_weak.upgrade() {
            // Replaying the station already in the player (the Play button)
            // keeps its name, logo and country, which a station that isn't a
            // favorite has nowhere else
            let current = ui.get_station_url() == url;
            let keep = |value: slint::SharedString| {
                Some(value.to_string()).filter(|v| current && !v.is_empty())
            };
            play_station_with_metadata(
                &ui,
                &play_tx,
                &play_url_state,
                &play_url_favs,
                &play_url_logo_svc,
                PlayMetadata {
                    url: url.to_string(),
                    name: keep(ui.get_station_name()),
                    logo_url: keep(ui.get_station_logo_url()),
                    country: keep(ui.get_station_country()),
                    provider_id: None,
                },
            );
        }
    });

    // Open Network Stream, with the station details typed in the dialog
    {
        let play_tx = ui_tx.clone();
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let state = shared_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_play_stream(move |url, name, logo_url, country| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let url = url.trim();
            if url.is_empty() {
                return;
            }
            let opt = |value: &str| Some(value.trim().to_string()).filter(|v| !v.is_empty());
            play_station_with_metadata(
                &ui,
                &play_tx,
                &state,
                &favs,
                &logo_svc,
                PlayMetadata {
                    url: url.to_string(),
                    name: opt(&name),
                    logo_url: opt(&logo_url),
                    country: opt(&country),
                    provider_id: None,
                },
            );
        });
    }

    let stop_tx = ui_tx.clone();
    ui.on_stop_clicked(move || {
        stop_tx.send(app::state::AppCommand::Stop);
    });

    let vol_tx = ui_tx.clone();
    ui.on_volume_changed(move |vol| {
        vol_tx.send(app::state::AppCommand::SetVolume(vol));
    });

    let mute_tx = ui_tx.clone();
    let mute_state = shared_state.clone();
    ui.on_mute_clicked(move || {
        let is_muted = lock(&mute_state).is_muted;
        if is_muted {
            mute_tx.send(app::state::AppCommand::Unmute);
        } else {
            mute_tx.send(app::state::AppCommand::Mute);
        }
    });

    // toggle-favorite callback
    {
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let state = shared_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_favorite(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let url = ui.get_station_url().to_string();
            if url.is_empty() {
                return;
            }
            // A stream without a name shows as "Radiotrope"; it is saved
            // under its address instead
            let name = lock(&state)
                .station_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| radiotrope_app::data::types::name_from_url(&url));
            let logo_url = ui.get_station_logo_url().to_string();
            let country = ui.get_station_country().to_string();

            let mut f = lock(&favs);
            if f.is_favorite(&url) {
                let _ = f.remove_by_url(&url);
            } else {
                use radiotrope_app::data::types::Favorite;
                use std::collections::HashSet;
                let mut fav = Favorite::new(&name, &url);
                if !logo_url.is_empty() {
                    fav = fav.with_logo(&logo_url);
                }
                let country_opt = if country.is_empty() {
                    None
                } else {
                    Some(country)
                };
                fav = fav.with_metadata(country_opt, None, HashSet::new());
                let _ = f.add(fav);
            }
            let _ = f.save();
            let is_fav = f.is_favorite(&url);
            drop(f);

            ui.set_is_station_favorited(is_fav);
            refresh_favorites(&ui, &favs, &logo_svc);
        });
    }

    // play-favorite callback
    {
        let play_tx = ui_tx.clone();
        let logo_svc = logo_service.clone();
        let favs = favorites.clone();
        let play_state = shared_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_play_favorite(move |station| {
            if let Some(ui) = ui_weak.upgrade() {
                let logo_url = station.logo_url.to_string();
                let country = station.country.to_string();
                play_station_with_metadata(
                    &ui,
                    &play_tx,
                    &play_state,
                    &favs,
                    &logo_svc,
                    PlayMetadata {
                        url: station.url.to_string(),
                        name: Some(station.name.to_string()),
                        logo_url: if logo_url.is_empty() {
                            None
                        } else {
                            Some(logo_url)
                        },
                        country: if country.is_empty() {
                            None
                        } else {
                            Some(country)
                        },
                        provider_id: None,
                    },
                );
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let state = shared_state.clone();
        ui.on_step_favorite(move |forward| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let playing_url = {
                let state = lock(&state);
                state
                    .station_url
                    .clone()
                    .filter(|_| state.playback == PlaybackState::Playing && !state.alarm_beep)
            };
            let favorites = ui.get_favorites_list();
            let index = shortcuts::favorite_step(
                favorites.iter().map(|station| station.url),
                playing_url.as_deref(),
                forward,
            );
            if let Some(station) = index.and_then(|index| favorites.row_data(index)) {
                ui.invoke_play_favorite(station);
            }
        });
    }

    // reset-favorite-stats callback
    {
        let favs = favorites.clone();
        let ui_weak = ui.as_weak();
        ui.on_reset_favorite_stats(move |id| {
            let mut f = lock(&favs);
            // What another player saved since is taken in first, and then
            // the poll redraws the whole list
            let took_in = f.reload_if_changed();
            if f.reset_stats(&id).is_err() {
                return;
            }
            // Otherwise the row is updated in place below
            if !took_in {
                note_favorites_shown(&f);
            }
            let _ = f.save();
            if let Some(map) = lock(&SESSION_LISTEN).as_mut() {
                map.remove(id.as_str());
            }
            if let Some(ui) = ui_weak.upgrade() {
                update_favorite_stats(&ui, &f);
            }
        });
    }

    // edit-favorite callback
    {
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let ui_weak = ui.as_weak();
        let edit_shared_state = shared_state.clone();
        ui.on_edit_favorite(move |station| {
            let id = station.id.to_string();
            let name = station.name.to_string();
            let url = station.url.to_string();
            let logo_url = station.logo_url.to_string();
            let country = station.country.to_string();

            let mut update = radiotrope_app::data::types::FavoriteUpdate::new()
                .name(name.clone())
                .url(url.clone());
            update.logo_url = Some(if logo_url.is_empty() {
                None
            } else {
                Some(logo_url.clone())
            });
            update.country = Some(if country.is_empty() {
                None
            } else {
                Some(country.clone())
            });

            // A failed update (another favorite has the new URL) changes
            // nothing; the dialog stays open and shows why
            let mut f = lock(&favs);
            let old_fav = f.get(&id).cloned();
            if f.update(&id, update).is_err() {
                return if f.get_id_for_url(&url) != id && f.is_favorite(&url) {
                    "Another favorite already has this stream URL".into()
                } else {
                    // The only other failure: the favorite is gone
                    "Can't save: this favorite was removed. Close and try again.".into()
                };
            }
            let _ = f.save();
            drop(f);

            if let Some(ui) = ui_weak.upgrade() {
                let edited = EditedFavorite {
                    old: old_fav.as_ref(),
                    id: &id,
                    name: &name,
                    url: &url,
                    logo_url: &logo_url,
                    country: &country,
                };
                show_edited_favorite(&ui, &favs, &logo_svc, &edit_shared_state, edited);
            }
            slint::SharedString::new()
        });
    }

    // delete-favorite callback
    {
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let ui_weak = ui.as_weak();
        ui.on_delete_favorite(move |station| {
            let id = station.id.to_string();
            let mut f = lock(&favs);
            let _ = f.remove(&id);
            let _ = f.save();
            drop(f);
            invalidate_logo_image(&id);

            if let Some(ui) = ui_weak.upgrade() {
                let current_url = ui.get_station_url().to_string();
                if station.url == current_url {
                    ui.set_is_station_favorited(false);
                }
                refresh_favorites(&ui, &favs, &logo_svc);
            }
        });
    }

    // move-favorite callback: Move to Top / Move to Bottom from a row's menu
    {
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let ui_weak = ui.as_weak();
        ui.on_move_favorite(move |station, to_top| {
            {
                let mut f = lock(&favs);
                if f.move_to_edge(&station.id, to_top).is_err() {
                    return;
                }
                let _ = f.save();
            }
            if let Some(ui) = ui_weak.upgrade() {
                refresh_favorites(&ui, &favs, &logo_svc);
            }
        });
    }

    // reorder-favorites callback
    {
        let favs = favorites.clone();
        let ui_weak = ui.as_weak();
        ui.on_reorder_favorites(move |from, to| {
            let from = from as usize;
            let to = to as usize;

            // Update data model
            {
                let mut f = lock(&favs);
                let mut sorted: Vec<String> = f
                    .sorted(FavoriteSort::Manual)
                    .iter()
                    .map(|fav| fav.id())
                    .collect();

                // The order on screen, which the drag's rows refer to, is
                // read before taking in what another player saved since
                let took_in = f.reload_if_changed();
                if from < sorted.len() && to < sorted.len() {
                    let id = sorted.remove(from);
                    sorted.insert(to, id);
                    let id_refs: Vec<&str> = sorted.iter().map(|s| s.as_str()).collect();
                    let _ = f.reorder(&id_refs);
                }
                // The rows are moved in place below; a rebuild would break
                // the next drag. After taking something in, the poll
                // redraws the list instead.
                if !took_in {
                    note_favorites_shown(&f);
                }
            }

            // Shuffle existing UI model data (no disk I/O or image decoding)
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_favorites_list();
                let logos_model = ui.get_favorite_logos();
                let mut items: Vec<FavoriteStation> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .collect();
                let mut logos: Vec<slint::Image> = (0..logos_model.row_count())
                    .filter_map(|i| logos_model.row_data(i))
                    .collect();

                if from < items.len() && to < items.len() {
                    let item = items.remove(from);
                    items.insert(to, item);
                    if from < logos.len() && to < logos.len() {
                        let logo = logos.remove(from);
                        logos.insert(to, logo);
                    }
                }

                ui.set_favorites_list(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                ui.set_favorite_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
            }

            // Save to disk in background
            let favs_bg = favs.clone();
            std::thread::Builder::new()
                .name("fav-save".into())
                .spawn(move || {
                    let mut f = lock(&favs_bg);
                    let _ = f.save();
                })
                .ok();
        });
    }

    window_settings::setup(&ui, &ui_tx, &shared_state);
    // WiFi settings and Power Off / Restart (Raspberry Pi build only)
    #[cfg(feature = "embedded")]
    wifi_ui::setup_wifi(&ui);
    #[cfg(feature = "embedded")]
    wifi_ui::setup_power(&ui);

    setup_about(&ui);
    setup_text_util(&ui);
    setup_recording(
        &ui,
        &settings,
        ui_tx.clone(),
        shared_state.clone(),
        logo_service.clone(),
    );
    schedule_ui::setup(&ui, &settings, ui_tx.clone(), shared_state.clone());

    // What the station browser shows, and how far "Load More" has paged
    let browse_state = Arc::new(Mutex::new((BrowseQuery::default(), 0usize)));

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
    let row_logos = row_logos::RowLogos::start(
        ui.as_weak(),
        browse_logos,
        logo_service.clone(),
        browse_logo_gen.clone(),
    );
    {
        let ui_weak = ui.as_weak();
        let row_logos = row_logos.clone();
        ui.on_browse_rows_changed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                row_logos.refresh(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let row_logos = row_logos.clone();
        ui.on_browse_closed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                row_logos.release(&ui);
                row_logos::return_freed_memory();
            }
        });
    }

    // search-stations callback: a name query within the filters (nothing
    // given shows the most played stations)
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_search_stations(move |query, country, genre| {
            let query = BrowseQuery {
                name: query.trim().to_string(),
                country: country.to_string(),
                genre: genre.to_string(),
            };
            start_browse(&ui_weak, &state, &gen, &row_logos, &favs, query);
        });
    }

    // load-more-stations callback
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_load_more_stations(move || {
            // The offset moves on only once the page is in the list, so a
            // list put aside meanwhile keeps the offset of the rows it holds
            let (query, offset) = {
                let s = lock(&state);
                (s.0.clone(), s.1 + SEARCH_PAGE_SIZE)
            };
            let my_gen = gen.load(Ordering::Relaxed);
            let ui_weak = ui_weak.clone();
            let gen = gen.clone();
            let row_logos = row_logos.clone();
            let favs = favs.clone();
            let state = state.clone();
            std::thread::Builder::new()
                .name("load-more".into())
                .spawn(move || {
                    let results = fetch_browse_page(&query, offset);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        // A new search or the other mode's list replaced this
                        // one meanwhile (and cleared its loading mark)
                        if gen.load(Ordering::Relaxed) != my_gen {
                            return;
                        }
                        ui.set_search_loading_more(false);
                        match results {
                            Ok(results) => {
                                lock(&state).1 = offset;
                                show_browse_results(&ui, results, true, &favs, &row_logos)
                            }
                            // The offset stayed, so the retry asks for this page again
                            Err(e) => show_browse_error(&ui, &e),
                        }
                    });
                })
                .ok();
        });
    }

    // Try the failed browser request again: the first page, or the next one
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_retry_browse(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_search_loading() || ui.get_search_loading_more() {
                return;
            }
            ui.set_search_error(Default::default());
            if ui.get_search_results().row_count() == 0 {
                let query = lock(&state).0.clone();
                start_browse(&ui_weak, &state, &gen, &row_logos, &favs, query);
            } else {
                ui.set_search_loading_more(true);
                ui.invoke_load_more_stations();
            }
        });
    }

    // Star on a search result: add the station to favorites, or remove it
    {
        let ui_weak = ui.as_weak();
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let row_logos = row_logos.clone();
        ui.on_toggle_browse_favorite(move |item| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut f = lock(&favs);
            let existing = f
                .find_match(&item.url, Some(item.provider_id.as_str()))
                .map(|fav| fav.url().to_string());
            match existing {
                Some(url) => {
                    let _ = f.remove_by_url(&url);
                }
                None => {
                    use radiotrope_app::data::types::Favorite;
                    let mut fav = Favorite::new(item.name.as_str(), item.url.as_str())
                        .with_metadata(
                            Some(item.country.to_string()).filter(|c| !c.is_empty()),
                            None,
                            Default::default(),
                        )
                        .with_provider(
                            radiotrope_app::providers::radiotrope::PROVIDER_ID,
                            Some(item.provider_id.to_string()).filter(|id| !id.is_empty()),
                        )
                        .with_audio_info(
                            Some(item.codec.to_string()).filter(|c| !c.is_empty()),
                            u32::try_from(item.bitrate).ok().filter(|b| *b > 0),
                        );
                    if !item.logo_url.is_empty() {
                        fav = fav.with_logo(item.logo_url.as_str());
                        // The browser already has the logo: no new download
                        if let Some(png) = row_logos.logos().cached_png(&item.logo_url) {
                            let _ = logo_svc.cache().put_logo(&fav, &png);
                        }
                    }
                    let _ = f.add(fav);
                }
            }
            let _ = f.save();
            let current = ui.get_station_url();
            ui.set_is_station_favorited(!current.is_empty() && f.is_favorite(&current));
            drop(f);
            refresh_favorites(&ui, &favs, &logo_svc);
        });
    }

    // Favorites to and from a file
    {
        // The list read from the file picked, until Add Missing or Replace
        let pending: std::rc::Rc<std::cell::RefCell<Vec<radiotrope_app::data::types::Favorite>>> =
            Default::default();
        {
            let ui_weak = ui.as_weak();
            let favs = favorites.clone();
            ui.on_export_favorites(move || export_favorites(ui_weak.clone(), favs.clone()));
        }
        {
            let ui_weak = ui.as_weak();
            let favs = favorites.clone();
            let pending = pending.clone();
            ui.on_import_favorites(move || {
                import_favorites(ui_weak.clone(), favs.clone(), pending.clone())
            });
        }
        {
            let ui_weak = ui.as_weak();
            let favs = favorites.clone();
            let logo_svc = logo_service.clone();
            ui.on_import_favorites_chosen(move |replace| {
                let Some(ui) = ui_weak.upgrade() else { return };
                let list = std::mem::take(&mut *pending.borrow_mut());
                let mode = if replace {
                    ImportMode::Replace
                } else {
                    ImportMode::Add
                };
                let mut f = lock(&favs);
                let done = f
                    .import_list(mode, &list, control::MAX_FAVORITES, false)
                    .and_then(|outcome| f.save().map(|_| outcome));
                drop(f);
                match done {
                    Ok(outcome) => {
                        let text = import_done_text(mode, &outcome, list.len());
                        ui.set_import_message(text.into());
                        ui.set_import_message_detail(Default::default());
                        ui.set_import_message_error(false);
                    }
                    Err(e) => {
                        ui.set_import_message(e.to_string().into());
                        ui.set_import_message_detail(Default::default());
                        ui.set_import_message_error(true);
                    }
                }
                refresh_favorites(&ui, &favs, &logo_svc);
            });
        }
    }

    // The countries or genres the station browser's filter picker lists
    {
        let ui_weak = ui.as_weak();
        ui.on_load_filter_entries(move |kind| {
            let ui_weak = ui_weak.clone();
            let kind = kind.to_string();
            std::thread::Builder::new()
                .name("load-filters".into())
                .spawn(move || {
                    let results = browse_directory().and_then(|p| p.browse_categories());
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        // The picker moved on to another kind meanwhile
                        if ui.get_filter_kind() != kind.as_str() {
                            return;
                        }
                        match results {
                            Ok(categories) => {
                                let entries = filter_entries(&kind, categories);
                                ALL_FILTER_ENTRIES.with(|all| *all.borrow_mut() = entries);
                                show_filter_entries(&ui, &ui.get_filter_entries_text());
                                ui.set_filter_entries_error(Default::default());
                            }
                            Err(e) => {
                                // The same words as the search window
                                let problem = ServiceProblem::of(&e);
                                ui.set_filter_entries_error(problem.message().into());
                            }
                        }
                        ui.set_filter_entries_loading(false);
                    });
                })
                .ok();
        });
    }

    // Narrow the filter picker's list by name
    {
        let ui_weak = ui.as_weak();
        ui.on_filter_filter_entries(move |text| {
            if let Some(ui) = ui_weak.upgrade() {
                show_filter_entries(&ui, &text);
            }
        });
    }

    // play-station callback
    {
        let play_tx = ui_tx.clone();
        let logo_svc = logo_service.clone();
        let favs = favorites.clone();
        let play_state = shared_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_play_station(move |station| {
            if let Some(ui) = ui_weak.upgrade() {
                let logo_url = station.logo_url.to_string();
                let country = station.country.to_string();
                play_station_with_metadata(
                    &ui,
                    &play_tx,
                    &play_state,
                    &favs,
                    &logo_svc,
                    PlayMetadata {
                        url: station.url.to_string(),
                        name: Some(station.name.to_string()),
                        logo_url: if logo_url.is_empty() {
                            None
                        } else {
                            Some(logo_url)
                        },
                        country: if country.is_empty() {
                            None
                        } else {
                            Some(country)
                        },
                        provider_id: Some(station.provider_id.to_string())
                            .filter(|id| !id.is_empty()),
                    },
                );
                report_browse_play(&station);
            }
        });
    }

    // TODO: set up system tray when mcp_mode is true

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

    // Poll shared state → Slint properties (runs on UI thread via Timer)
    let ui_weak = ui.as_weak();
    let poll_state = shared_state.clone();
    let poll_favs = favorites.clone();
    let poll_logo_svc = logo_service.clone();
    let poll_tx = ui_tx.clone();
    let poll_viz_timer = viz_timer.clone();
    let poll_stats = engine_stats.clone();
    // Logo of the station restored at startup, which may not be a favorite
    let restored_logo: Option<(String, String)> = settings
        .last_station
        .as_ref()
        .and_then(|s| Some((s.url.clone(), s.logo_url.clone()?)))
        .filter(|(_, logo)| !logo.is_empty());
    // Recording notice on screen: its sequence number and when it appeared
    let poll_notice = std::cell::RefCell::new((0u64, Instant::now()));
    // Listening session of the station playing now, for favorite stats
    let listen_session: std::rc::Rc<std::cell::RefCell<Option<ListenSession>>> = Default::default();
    let poll_listen = listen_session.clone();
    let listen_favs = favorites.clone();
    let poll_saver = favorites_saver.clone();
    // The window's settings, saved a moment after they change rather than
    // only at exit, which a logoff or a crash never reaches
    let poll_settings =
        std::cell::RefCell::new(app::settings_watch::SettingsWatch::new(SETTINGS_SAVE_DELAY));
    // Keep "12 min ago" and similar texts current
    let stats_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let favs = favorites.clone();
        stats_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(60),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                if let Ok(f) = favs.try_lock() {
                    update_favorite_stats(&ui, &f);
                }
            },
        );
    }
    // Take in favorites another player saved (one an agent started with
    // `--mcp --standalone`); the poll below then redraws them
    let follow_timer = slint::Timer::default();
    {
        let favs = favorites.clone();
        follow_timer.start(
            slint::TimerMode::Repeated,
            radiotrope_app::config::ui::FAVORITES_FOLLOW,
            move || {
                if let Ok(mut f) = favs.try_lock() {
                    f.reload_if_changed();
                }
            },
        );
    }
    let _timer = slint::Timer::default();
    _timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };

            // Commands that found the controller's queue full go out now
            poll_tx.flush();

            // Check for external favorites changes (e.g. from MCP); the
            // UI's own changes are drawn as they are made
            if let Ok(f) = poll_favs.try_lock() {
                let gen = f.generation();
                drop(f);
                if gen != FAVORITES_SHOWN.get() {
                    refresh_favorites(&ui, &poll_favs, &poll_logo_svc);
                }
            }

            // try_lock: skip this tick if controller holds the lock
            let Ok(s) = poll_state.try_lock() else { return };
            let picked = PENDING_PICK.with(|p| {
                p.borrow_mut()
                    .waiting(s.play_seq, Instant::now(), PLAY_TAKE_TIMEOUT)
                    .cloned()
            });
            // Copy all data under lock, then drop before touching UI
            let mut station_name: slint::SharedString =
                s.station_name.as_deref().unwrap_or(UNNAMED_STATION).into();
            let mut codec_info: slint::SharedString = format_codec_line(&s).into();
            let mut status_text: slint::SharedString = s.status_text.as_ref().into();
            let mut is_error = s.is_error;
            let mut is_loading = s.is_resolving || s.status_text == status::CONNECTING;
            let mut is_playing = s.playback == PlaybackState::Playing;
            let alarm_beep = s.alarm_beep;
            let mut now_playing: slint::SharedString = if !s.title.is_empty() {
                if !s.artist.is_empty() {
                    format!("{} - {}", s.artist, s.title).into()
                } else {
                    s.title.as_str().into()
                }
            } else {
                Default::default()
            };
            let volume = s.volume;
            let is_muted = s.is_muted;
            let mut station_url: Option<slint::SharedString> =
                s.station_url.as_deref().map(Into::into);
            // Given with the Play, by the UI or an agent
            let station_logo = s.station_logo_url.clone();
            let station_country = s.station_country.clone();
            let eq_gains = s.eq_gains;
            let eq_preamp = s.eq_preamp;
            let eq_enabled = s.eq_enabled;
            let eq_preset: slint::SharedString = s.eq_preset_name.as_deref().unwrap_or("").into();
            let filters = FilterSettings::of(&s);
            let recording = s.recording.clone();
            let recording_notice = s.recording_notice.clone();
            let listeners = s
                .listen
                .as_ref()
                .map(|l| l.listener_names())
                .unwrap_or_default();
            let schedule_view = schedule_ui::View::of(&s);
            let settings_now = window_settings(&s, &ui);
            drop(s);
            if let Some(settings) = poll_settings
                .borrow_mut()
                .note(settings_now, Instant::now())
            {
                write_settings(&settings);
            }

            // A station picked here that the controller hasn't taken up
            // yet shows as starting: the state still holds the old one,
            // playing
            if let Some(picked) = picked {
                station_name = picked.name.as_deref().unwrap_or(UNNAMED_STATION).into();
                codec_info = AWAITING_STREAM.into();
                status_text = STARTING_STATUS.into();
                is_error = false;
                is_loading = true;
                is_playing = false;
                now_playing = Default::default();
                station_url = Some(picked.url.as_str().into());
            }

            schedule_ui::show_state(&ui, schedule_view);
            show_listeners(&ui, &listeners);
            show_recording_state(
                &ui,
                recording.as_ref(),
                recording_notice.as_ref(),
                is_playing,
                &mut poll_notice.borrow_mut(),
            );

            // Credit listening time to the favorite being played
            // An alarm's beep isn't time on its station
            let playing_url = station_url.as_deref().filter(|_| is_playing && !alarm_beep);
            track_listening(
                &ui,
                &poll_favs,
                &poll_saver,
                &mut poll_listen.borrow_mut(),
                playing_url,
            );

            // Set UI properties without holding any lock
            ui.set_station_name(station_name);
            ui.set_codec_info(codec_info);
            ui.set_status_text(status_text);
            ui.set_is_error(is_error);
            ui.set_is_playing(is_playing);
            // The play time beside "Playing", kept current here: the
            // statistics dialog, which shows it too, is fed only while open
            if is_playing {
                if let Some(stats) = poll_stats() {
                    if let Ok(s) = stats.try_lock() {
                        let started = s.play_started_at;
                        drop(s);
                        ui.set_stat_playtime(format_playtime(started).into());
                    }
                }
            }
            // Playing started, or the window came back from being minimised
            if viz_wanted(&ui) && !poll_viz_timer.running() {
                poll_viz_timer.restart();
            }
            ui.set_now_playing_title(now_playing);
            if !ui.get_volume_dragging() {
                ui.set_volume(volume);
            }
            ui.set_is_muted(is_muted);
            if let Some(url) = station_url {
                let url_changed = SHOWN_STATION.with(|s| s.borrow().changed(url.as_str()));
                ui.set_station_url(url.clone());
                // Update favorite star based on current station (while a
                // save holds the favorites, the star stays as it was)
                if let Ok(f) = poll_favs.try_lock() {
                    ui.set_is_station_favorited(f.is_favorite(url.as_str()));
                }

                // When station URL changes (e.g. MCP play), update logo
                // and country
                if url_changed {
                    let shown = show_station(url.as_str());

                    // A favorite's details first, then the restored
                    // station's own logo, then what came with the Play
                    let (fav_logo, fav_country) = poll_favs
                        .lock()
                        .ok()
                        .and_then(|f| {
                            f.get_by_url(url.as_str()).map(|fav| {
                                (fav.station.logo_url.clone(), fav.station.country.clone())
                            })
                        })
                        .unwrap_or_default();
                    let logo_url = fav_logo
                        .or_else(|| {
                            restored_logo
                                .as_ref()
                                .filter(|(u, _)| u == url.as_str())
                                .map(|(_, logo)| logo.clone())
                        })
                        .or(station_logo)
                        .filter(|logo| !logo.is_empty());
                    let country = fav_country.or(station_country).unwrap_or_default();
                    ui.set_station_logo_url(logo_url.as_deref().unwrap_or("").into());
                    ui.set_station_country(country.as_str().into());

                    // Try cached logo first (works even without logo_url on favorite)
                    let tmp_station = Station::new(ui.get_station_name().as_str(), url.as_str());
                    if let Some((rgba, w, h)) = poll_logo_svc.get_cached_rgba(&tmp_station) {
                        set_header_logo(&ui, rgba, w, h);
                    } else {
                        // Clear stale logo only when no cached replacement is available
                        ui.set_current_logo(Default::default());
                        // Not cached: fetch it
                        if let Some(logo) = logo_url {
                            let logo_svc = poll_logo_svc.clone();
                            let ui_weak2 = ui.as_weak();
                            let station_name = ui.get_station_name().to_string();
                            let station_url = url.to_string();
                            std::thread::Builder::new()
                                .name("poll-logo-fetch".into())
                                .spawn(move || {
                                    let tmp =
                                        Station::new(&station_name, &station_url).with_logo(&logo);
                                    if let Some((rgba, w, h)) = logo_svc.get_rgba(&tmp) {
                                        let _ = slint::invoke_from_event_loop(move || {
                                            let Some(ui) = ui_weak2.upgrade() else { return };
                                            if is_current_station(shown, &station_url) {
                                                set_header_logo(&ui, rgba, w, h);
                                            }
                                        });
                                    }
                                })
                                .ok();
                        }
                    }
                }
            }
            ui.set_is_loading(is_loading);

            // Sync EQ state (for MCP-driven changes)
            ui.set_eq_enabled(eq_enabled);
            ui.set_eq_preset_name(eq_preset);
            ui.set_eq_preamp(eq_preamp);
            ui.set_eq_band0(eq_gains[0]);
            ui.set_eq_band1(eq_gains[1]);
            ui.set_eq_band2(eq_gains[2]);
            ui.set_eq_band3(eq_gains[3]);
            ui.set_eq_band4(eq_gains[4]);
            ui.set_eq_band5(eq_gains[5]);
            ui.set_eq_band6(eq_gains[6]);
            ui.set_eq_band7(eq_gains[7]);
            ui.set_eq_band8(eq_gains[8]);
            ui.set_eq_band9(eq_gains[9]);

            show_filters(&ui, &filters);
        },
    );

    // Quit like a closed window on SIGTERM (systemd on stop or reboot),
    // SIGINT (Ctrl+C) and SIGHUP (the terminal closed), so the exit below
    // saves and finishes a recording
    #[cfg(unix)]
    quit_on_signals();

    // Closing the window ends the app, or only hides it when View > Close
    // to Tray is on and the tray shows our icon
    {
        let ui_weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            #[cfg(feature = "desktop")]
            if ui_weak
                .upgrade()
                .is_some_and(|ui| tray::hides_on_close(&ui))
            {
                return slint::CloseRequestResponse::HideWindow;
            }
            let _ = &ui_weak;
            quit_app();
            slint::CloseRequestResponse::HideWindow
        });
    }
    #[cfg(feature = "desktop")]
    ui.on_quit_app(quit_app);

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
    let shown = if start_hidden { Ok(()) } else { ui.show() };
    if let Err(e) = shown.and_then(|()| slint::run_event_loop_until_quit()) {
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
    favorites_saver.flush();

    // Final save before shutdown
    save_settings(&shared_state, &ui);

    // Give the controller time to finish a recording in progress, so the
    // end of the file is written before the process exits
    while shutdown_sent && !controller.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
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

/// The header logo's visualizer colours on each theme (dark, light),
/// worked out once per logo: a theme switch only picks
struct HeaderLogo {
    image: slint::Image,
    colors: [Vec<slint::Color>; 2],
}

thread_local! {
    static HEADER_LOGO: std::cell::RefCell<Option<HeaderLogo>> =
        const { std::cell::RefCell::new(None) };
}

/// A logo's visualizer colours on each theme (dark, light). `rgba` is the
/// logo's pixels in RGBA order.
fn logo_colors(rgba: &[u8]) -> [Vec<slint::Color>; 2] {
    [true, false].map(|dark| {
        logo_palette(rgba, dark)
            .into_iter()
            .map(|[r, g, b]| slint::Color::from_rgb_u8(r, g, b))
            .collect()
    })
}

/// A logo as the UI shows it: on its colour fog when it needs one (see
/// [`visual::with_backdrop`])
fn logo_image(rgba: Vec<u8>, width: u32, height: u32) -> slint::Image {
    let (rgba, width, height) = visual::with_backdrop(rgba, width, height);
    let pixels = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, width, height);
    slint::Image::from_rgba8(pixels)
}

/// Show a station logo in the header. The visualizer takes its colours
/// from the logo itself, not from the fog it is shown on.
fn set_header_logo(ui: &App, rgba: Vec<u8>, width: u32, height: u32) {
    let colors = logo_colors(&rgba);
    let image = logo_image(rgba, width, height);
    HEADER_LOGO.with(|header| {
        *header.borrow_mut() = Some(HeaderLogo {
            image: image.clone(),
            colors,
        });
    });
    ui.set_current_logo(image);
}

/// Colour the visualizer with the main colours of the station logo
fn apply_logo_palette(ui: &App) {
    let dark = ui.get_dark_mode();
    let image = ui.get_current_logo();
    HEADER_LOGO.with(|header| {
        let mut header = header.borrow_mut();
        // A logo not shown through set_header_logo (none at all, say)
        if header.as_ref().is_none_or(|h| h.image != image) {
            *header = Some(HeaderLogo {
                colors: image
                    .to_rgba8()
                    .map(|buf| logo_colors(buf.as_bytes()))
                    .unwrap_or_default(),
                image,
            });
        }
        let Some(logo) = header.as_ref() else { return };
        let colors = logo.colors[!dark as usize].clone();
        ui.global::<VizStyle>()
            .set_logo_colors(ModelRc::from(std::rc::Rc::new(VecModel::from(colors))));
    });
}

/// Text helpers the UI calls back into
fn setup_text_util(ui: &App) {
    ui.global::<TextUtil>()
        .on_steady_digits(|text| radiotrope_app::text::steady_digits(&text).into());
}

thread_local! {
    /// The station the player shows, numbered (see [`ShownStation`])
    static SHOWN_STATION: std::cell::RefCell<ShownStation> = Default::default();
    /// A station picked here that the controller hasn't taken up yet
    static PENDING_PICK: std::cell::RefCell<PendingPick> = Default::default();
}

/// The player now shows the station at `url`. Returns its number, for the
/// logo fetched for it.
fn show_station(url: &str) -> u64 {
    SHOWN_STATION.with(|s| s.borrow_mut().show(url))
}

/// Whether a logo fetched for `url` while station number `shown` was in
/// the player may still be shown: no station was picked since (from the
/// UI or an agent). The poll overwrites the logo of a station it finds
/// changed, so one landing just before it doesn't stay.
fn is_current_station(shown: u64, url: &str) -> bool {
    SHOWN_STATION.with(|s| s.borrow().is_current(shown, url))
}

/// Shared helper for all play actions. Enriches metadata from favorites if available,
/// sets all UI properties consistently, sends the Play command, and spawns logo fetch.
fn play_station_with_metadata(
    ui: &App,
    cmd_tx: &UiSender,
    shared_state: &Arc<Mutex<AppSnapshot>>,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
    req: PlayMetadata,
) {
    // A favorite's URL, name and logo take priority over the caller's
    let (
        PlayMetadata {
            url,
            name,
            logo_url,
            country,
            ..
        },
        is_favorite,
    ) = {
        let favs = lock(favorites);
        let play = favs.resolve_play(req);
        let is_favorite = favs.is_favorite(&play.url);
        (play, is_favorite)
    };

    // A second click on the same station right after the first is ignored:
    // a touchpad can turn one tap into two, which restarted the stream.
    // The first click acts at once, and another station always switches.
    {
        let now = Instant::now();
        let mut last = lock(&LAST_PLAY_CLICK);
        let guard =
            Duration::from_millis(ui.global::<Defaults>().get_repeat_click_guard().max(0) as u64);
        let repeat = last
            .as_ref()
            .is_some_and(|(last_url, at)| *last_url == url && now.duration_since(*at) < guard);
        if repeat {
            return;
        }
        *last = Some((url.clone(), now));
    }
    let shown = show_station(&url);

    // Set UI metadata properties
    ui.set_station_logo_url(logo_url.as_deref().unwrap_or("").into());
    ui.set_station_country(country.as_deref().unwrap_or("").into());

    // Try cached logo first to avoid placeholder flash. The cache is keyed
    // by the stream URL, so a logo cached earlier shows even when the
    // station has no logo URL now (as the poll timer does on a station
    // change); otherwise replaying the same station would lose it.
    let cached = logo_service.get_cached_rgba(&Station::new(name.as_deref().unwrap_or(""), &url));
    let cache_hit = if let Some((rgba, w, h)) = cached {
        set_header_logo(ui, rgba, w, h);
        true
    } else {
        false
    };
    if !cache_hit {
        ui.set_current_logo(Default::default());
    }

    // The controller takes up the Play only after stopping the old
    // station; the poll shows this one as starting until then. Its number
    // is read before the Play goes out, which may be taken at once.
    let play_seq = lock(shared_state).play_seq;
    PENDING_PICK.with(|p| {
        p.borrow_mut()
            .pick(play_seq, &url, name.clone(), Instant::now())
    });

    // Send play command
    cmd_tx.send(app::state::AppCommand::Play {
        url: url.clone(),
        name: name.clone(),
        logo_url: logo_url.clone(),
        country: country.clone(),
        taken: None,
    });
    // Show it as starting right away, in the header, the favorites and the
    // Play button (it turns into Stop), not only at the next state poll
    ui.set_station_url(url.as_str().into());
    ui.set_station_name(name.as_deref().unwrap_or(UNNAMED_STATION).into());
    ui.set_codec_info(AWAITING_STREAM.into());
    ui.set_status_text(STARTING_STATUS.into());
    ui.set_is_error(false);
    ui.set_is_playing(false);
    ui.set_now_playing_title(Default::default());
    ui.set_is_station_favorited(is_favorite);
    ui.set_is_loading(true);

    // Save settings (persists last_station, volume, eq, etc.)
    {
        let mut s = lock(shared_state);
        s.station_url = Some(url.clone());
        s.station_name = name.clone();
        s.station_logo_url = logo_url.clone();
        s.station_country = country;
        drop(s);
        save_settings(shared_state, ui);
    }

    // Fetch logo on background thread (only if not already cached)
    if !cache_hit {
        if let Some(ref logo) = logo_url {
            if !logo.is_empty() {
                let ui_weak = ui.as_weak();
                let logo_svc = logo_service.clone();
                let play_name = name.unwrap_or_default();
                let play_url = url;
                let play_logo = logo.clone();
                std::thread::Builder::new()
                    .name("logo-fetch".into())
                    .spawn(move || {
                        let tmp_station = Station::new(&play_name, &play_url).with_logo(&play_logo);
                        if let Some((rgba, width, height)) = logo_svc.get_rgba(&tmp_station) {
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = ui_weak.upgrade() else { return };
                                // Another station may have been picked while
                                // this one's logo was downloading
                                if !is_current_station(shown, &play_url) {
                                    return;
                                }
                                set_header_logo(&ui, rgba, width, height);
                            });
                        }
                    })
                    .ok();
            }
        }
    }
}

/// What the station browser is listing: stations whose name matches,
/// within the filters. Each part may be empty; all empty lists the most
/// played stations.
#[derive(Clone, Default)]
struct BrowseQuery {
    name: String,
    /// ISO 3166-1 code
    country: String,
    /// The directory's genre slug
    genre: String,
}

impl BrowseQuery {
    fn filter(&self) -> StationFilter {
        let given = |v: &str| Some(v.to_string()).filter(|v| !v.is_empty());
        StationFilter {
            name: given(&self.name),
            country: given(&self.country),
            genre: given(&self.genre),
            ..Default::default()
        }
    }
}

/// Fetch one page of browser results from the Radiotrope directory
fn fetch_browse_page(
    query: &BrowseQuery,
    offset: usize,
) -> radiotrope_app::error::Result<SearchResults> {
    browse_directory()?.search_filtered(&query.filter(), SEARCH_PAGE_SIZE, offset)
}

/// The directory the station browser lists, made once: it remembers where
/// each list's next page starts
fn browse_directory() -> radiotrope_app::error::Result<Arc<RadiotropeProvider>> {
    static DIRECTORY: std::sync::OnceLock<Arc<RadiotropeProvider>> = std::sync::OnceLock::new();
    if let Some(directory) = DIRECTORY.get() {
        return Ok(directory.clone());
    }
    let directory = Arc::new(RadiotropeProvider::new()?);
    Ok(DIRECTORY.get_or_init(|| directory).clone())
}

/// Count a play of a station browser result in the directory, which
/// orders its lists by plays. Nobody waits on it, and a failure costs
/// only the count.
fn report_browse_play(item: &BrowseStation) {
    if item.provider_id.is_empty() {
        return;
    }
    let station = Station::new(item.name.as_str(), item.url.as_str()).with_provider(
        radiotrope_app::providers::radiotrope::PROVIDER_ID,
        Some(item.provider_id.to_string()),
    );
    let _ = std::thread::Builder::new()
        .name("report-play".into())
        .spawn(move || {
            let _ = browse_directory().and_then(|d| d.report_click(&station));
        });
}

/// Replace the browser list with the first page of `query`, fetched in the
/// background. Results of an older request that finish later are dropped.
fn start_browse(
    ui_weak: &slint::Weak<App>,
    state: &Arc<Mutex<(BrowseQuery, usize)>>,
    gen: &Arc<AtomicU64>,
    row_logos: &row_logos::RowLogos,
    favs: &Arc<Mutex<FavoritesManager>>,
    query: BrowseQuery,
) {
    *lock(state) = (query.clone(), 0);
    let my_gen = gen.fetch_add(1, Ordering::Relaxed) + 1;
    // Clear old results immediately
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_search_results(ModelRc::default());
        ui.set_browse_logos(ModelRc::default());
        ui.set_has_more(false);
        ui.set_search_error(Default::default());
        ui.set_search_loading(true);
        // A page still loading for the old list is dropped when it comes
        ui.set_search_loading_more(false);
    }
    let ui_weak = ui_weak.clone();
    let gen = gen.clone();
    let row_logos = row_logos.clone();
    let favs = favs.clone();
    std::thread::Builder::new()
        .name("station-browse".into())
        .spawn(move || {
            let results = fetch_browse_page(&query, 0);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                if gen.load(Ordering::Relaxed) != my_gen {
                    return;
                }
                match results {
                    Ok(results) => show_browse_results(&ui, results, false, &favs, &row_logos),
                    Err(e) => show_browse_error(&ui, &e),
                }
                ui.set_search_loading(false);
            });
        })
        .ok();
}

/// Show a page of browser results, appending to the list or replacing it,
/// then load the logos of the rows on screen
fn show_browse_results(
    ui: &App,
    results: SearchResults,
    append: bool,
    favs: &Arc<Mutex<FavoritesManager>>,
    row_logos: &row_logos::RowLogos,
) {
    let mut new_items: Vec<BrowseStation> =
        results.stations.iter().map(station_to_browse).collect();
    {
        let f = lock(favs);
        for item in &mut new_items {
            item.is_favorite = browse_is_favorite(&f, item);
        }
    }
    let (mut items, mut logos) = if append {
        (
            ui.get_search_results().iter().collect::<Vec<_>>(),
            ui.get_browse_logos().iter().collect::<Vec<_>>(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    // No logos yet: rows get theirs as they come on screen
    logos.resize(items.len() + new_items.len(), slint::Image::default());
    items.extend(new_items);
    ui.set_search_results(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    ui.set_browse_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
    ui.set_has_more(results.has_more);
    ui.set_search_error(Default::default());
    BROWSE_FAILURES.set(0);
    row_logos.refresh(ui);
}

thread_local! {
    /// Station browser requests failed in a row, which spaces out the retries
    static BROWSE_FAILURES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Say why a station browser request failed, and when it is tried again
fn show_browse_error(ui: &App, e: &radiotrope_app::error::AppError) {
    let failures = BROWSE_FAILURES.get() + 1;
    BROWSE_FAILURES.set(failures);
    let problem = ServiceProblem::of(e);
    ui.set_search_error(problem.message().into());
    ui.set_search_retry_in(problem.retry_delay_secs(failures) as i32);
}

fn station_to_browse(s: &Station) -> BrowseStation {
    BrowseStation {
        name: s.name.as_str().into(),
        url: s.url.as_str().into(),
        logo_url: s.logo_url.as_deref().unwrap_or("").into(),
        country: s.country.as_deref().unwrap_or("").into(),
        // radio-browser said "UNKNOWN" when it had no codec; show no badge
        codec: s
            .codec
            .as_deref()
            .filter(|c| !c.eq_ignore_ascii_case("unknown"))
            .unwrap_or("")
            .into(),
        bitrate: s.bitrate.unwrap_or(0) as i32,
        provider_id: s.provider_id.as_deref().unwrap_or("").into(),
        is_favorite: false,
    }
}

/// A search result is a favorite by URL or by the provider's station ID
fn browse_is_favorite(favs: &FavoritesManager, item: &BrowseStation) -> bool {
    favs.find_match(&item.url, Some(item.provider_id.as_str()))
        .is_some()
}

/// Update the stars on the search results after favorites changed
fn mark_browse_favorites(ui: &App, favs: &FavoritesManager) {
    let model = ui.get_search_results();
    for i in 0..model.row_count() {
        let Some(mut item) = model.row_data(i) else {
            continue;
        };
        let fav = browse_is_favorite(favs, &item);
        if item.is_favorite != fav {
            item.is_favorite = fav;
            model.set_row_data(i, item);
        }
    }
    // Lists put aside for the other mode get theirs when shown again
}

fn favorite_to_slint(
    f: &radiotrope_app::data::types::Favorite,
    stats: radiotrope_app::data::types::StationStats,
) -> FavoriteStation {
    FavoriteStation {
        id: f.id().into(),
        name: f.name().into(),
        url: f.url().into(),
        logo_url: f.station.logo_url.as_deref().unwrap_or("").into(),
        country: f.station.country.as_deref().unwrap_or("").into(),
        listen_time: format_listen_time(stats.total_listen_time_secs).into(),
        last_played: format_last_played(stats.last_played).into(),
        play_count: stats.play_count.min(i32::MAX as u32) as i32,
        session_time: format_listen_time(session_listen_secs(&f.id())).into(),
    }
}

/// Stream URL and time of the last station started from the UI, so a
/// double tap doesn't start it twice
static LAST_PLAY_CLICK: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Listening time per favorite since the app was opened, by favorite ID
static SESSION_LISTEN: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn session_listen_secs(id: &str) -> u64 {
    lock(&SESSION_LISTEN)
        .as_ref()
        .and_then(|m| m.get(id).copied())
        .unwrap_or(0)
}

/// Follow what is playing and add listening time to favorites: once a
/// minute while a station plays, and when it stops or changes
fn track_listening(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    saver: &DelayedSave,
    session: &mut Option<ListenSession>,
    playing_url: Option<&str>,
) {
    if session
        .as_ref()
        .is_some_and(|s| Some(s.url.as_str()) != playing_url)
    {
        if let Some(ended) = session.take() {
            credit_listening(ui, favorites, saver, ended);
        }
    }
    match (session.as_mut(), playing_url) {
        (None, Some(url)) => {
            *session = Some(ListenSession::new(url, Instant::now()));
        }
        (Some(s), _) => {
            let listened = s.tick(Instant::now());
            if listened >= s.credited + LISTEN_CREDIT_SECS {
                let (url, credited) = (s.url.clone(), s.credited);
                s.credited = listened;
                add_listening(
                    ui,
                    favorites,
                    saver,
                    &url,
                    listened - credited,
                    credited == 0,
                );
            }
        }
        _ => {}
    }
}

/// Add what is left of a finished session
fn credit_listening(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    saver: &DelayedSave,
    mut session: ListenSession,
) {
    let elapsed = session.tick(Instant::now());
    if elapsed < MIN_LISTEN_SECS || elapsed <= session.credited {
        return;
    }
    add_listening(
        ui,
        favorites,
        saver,
        &session.url,
        elapsed - session.credited,
        session.credited == 0,
    );
}

/// Add listening time to the favorite playing from `url`. It is saved a
/// moment later by `saver`, off the UI thread.
fn add_listening(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    saver: &DelayedSave,
    url: &str,
    secs: u64,
    new_play: bool,
) {
    let mut favs = lock(favorites);
    let Some(id) = favs.add_listening(url, secs, new_play).map(|f| f.id()) else {
        return;
    };
    *lock(&SESSION_LISTEN)
        .get_or_insert_with(HashMap::new)
        .entry(id)
        .or_default() += secs;
    saver.request();
    update_favorite_stats(ui, &favs);
}

/// Refresh the stats text of every favorites row in place, without
/// rebuilding the list (which would interrupt a drag)
fn update_favorite_stats(ui: &App, favs: &FavoritesManager) {
    let model = ui.get_favorites_list();
    for i in 0..model.row_count() {
        let Some(mut row) = model.row_data(i) else {
            continue;
        };
        if favs.get(&row.id).is_none() {
            continue;
        }
        let stats = favs.stats(&row.id);
        let listen_time: slint::SharedString =
            format_listen_time(stats.total_listen_time_secs).into();
        let last_played: slint::SharedString = format_last_played(stats.last_played).into();
        let play_count = stats.play_count.min(i32::MAX as u32) as i32;
        let session_time: slint::SharedString =
            format_listen_time(session_listen_secs(&row.id)).into();
        if row.listen_time != listen_time
            || row.last_played != last_played
            || row.play_count != play_count
            || row.session_time != session_time
        {
            row.listen_time = listen_time;
            row.last_played = last_played;
            row.play_count = play_count;
            row.session_time = session_time;
            model.set_row_data(i, row);
        }
    }
}

/// Total listening time for a favorites row: "3 h 25 min", "40 min",
/// "<1 min", or empty when never played
fn format_listen_time(secs: u64) -> String {
    text::minutes(secs)
}

/// When a favorite was last played, relative to now: "Just now",
/// "12 min ago", "3 h ago", "Yesterday", "5 days ago", "2 weeks ago",
/// "4 months ago", or empty when never played
fn format_last_played(timestamp: Option<u64>) -> String {
    let Some(ts) = timestamp else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ago = now.saturating_sub(ts);
    let (min, hour, day) = (60, 3600, 86400);
    match ago {
        a if a < 2 * min => "Just now".into(),
        a if a < hour => format!("{} min ago", a / min),
        a if a < day => format!("{} h ago", a / hour),
        a if a < 2 * day => "Yesterday".into(),
        a if a < 14 * day => format!("{} days ago", a / day),
        a if a < 60 * day => format!("{} weeks ago", a / (7 * day)),
        a if a < 365 * day => format!("{} months ago", a / (30 * day)),
        a => {
            let years = a / (365 * day);
            if years == 1 {
                "1 year ago".into()
            } else {
                format!("{years} years ago")
            }
        }
    }
}

// Decoded flag images keyed by country code. Flags are tiny and shared by many
// rows, so each is decoded once. Runs on the Slint event-loop thread only.
thread_local! {
    static FLAG_IMAGE_CACHE: std::cell::RefCell<HashMap<String, slint::Image>> =
        std::cell::RefCell::new(HashMap::new());
}

thread_local! {
    /// Every entry of the kind the filter picker loaded last; it shows a
    /// filtered copy
    static ALL_FILTER_ENTRIES: std::cell::RefCell<Vec<FilterEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The directory's countries or genres (`kind`: "country" or "genre") as
/// filter picker entries, A to Z
fn filter_entries(
    kind: &str,
    categories: Vec<radiotrope_app::providers::types::Category>,
) -> Vec<FilterEntry> {
    let wanted = match kind {
        "genre" => CategoryType::Genre,
        _ => CategoryType::Country,
    };
    let mut entries: Vec<FilterEntry> = categories
        .into_iter()
        .filter(|c| c.category_type == wanted)
        .map(|c| {
            let code = match wanted {
                CategoryType::Country => c.code.clone().unwrap_or_default(),
                _ => c.id.clone(),
            };
            FilterEntry {
                flag: if wanted == CategoryType::Country {
                    flag_image(c.code.as_deref(), Some(&c.name))
                } else {
                    Default::default()
                },
                code: code.into(),
                name: c.name.as_str().into(),
                station_count: c.station_count.unwrap_or(0) as i32,
            }
        })
        // One the directory can't be asked for is no use as a filter
        .filter(|e| !e.code.is_empty())
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

/// Show the filter picker entries whose name contains `filter`
/// (case-insensitive)
fn show_filter_entries(ui: &App, filter: &str) {
    let filter = filter.trim().to_lowercase();
    let entries: Vec<FilterEntry> = ALL_FILTER_ENTRIES.with(|all| {
        all.borrow()
            .iter()
            .filter(|e| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .cloned()
            .collect()
    });
    ui.set_filter_entries(ModelRc::from(std::rc::Rc::new(VecModel::from(entries))));
}

/// Flag image for a country (an empty image if there is no flag for it)
fn flag_image(country_code: Option<&str>, country: Option<&str>) -> slint::Image {
    use radiotrope_app::data::flags;

    let Some(code) = flags::flag_code(country_code, country) else {
        return Default::default();
    };
    FLAG_IMAGE_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(code)
            .or_insert_with_key(|code| {
                flags::flag_png(code)
                    .and_then(|png| image::load_from_memory(png).ok())
                    .map(|img| {
                        let rgba = img.to_rgba8();
                        let pixel_buf = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            rgba.as_raw(),
                            rgba.width(),
                            rgba.height(),
                        );
                        slint::Image::from_rgba8(pixel_buf)
                    })
                    .unwrap_or_default()
            })
            .clone()
    })
}

/// A favorite's decoded logo, and when the file it was decoded from was
/// written: a newer file (a logo downloaded again) is decoded again
type DecodedLogo = (slint::Image, Option<std::time::SystemTime>);

// In-memory cache of decoded slint::Image keyed by favorite ID.
// All callers run on the Slint event-loop thread, so thread_local is safe and avoids
// threading slint::Image (which is !Send) through closures.
thread_local! {
    static LOGO_IMAGE_CACHE: std::cell::RefCell<HashMap<String, DecodedLogo>> =
        std::cell::RefCell::new(HashMap::new());
}

thread_local! {
    /// The favorites generation the list on screen shows. A change made
    /// elsewhere (an agent) shows as a newer one, which the poll redraws.
    static FAVORITES_SHOWN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The favorites on screen are up to date with `favs`
fn note_favorites_shown(favs: &FavoritesManager) {
    FAVORITES_SHOWN.set(favs.generation());
}

/// A favorite just edited, in the window or from a phone
struct EditedFavorite<'a> {
    /// As it was
    old: Option<&'a radiotrope_app::data::types::Favorite>,
    /// Its id before the edit (it changes with the stream URL)
    id: &'a str,
    name: &'a str,
    url: &'a str,
    /// Empty: no logo
    logo_url: &'a str,
    /// Empty: none
    country: &'a str,
}

/// Show an edited favorite: drop its old logo, update the player when it
/// is the station there, redraw the list, and fetch the new logo
fn show_edited_favorite(
    ui: &App,
    favs: &Arc<Mutex<FavoritesManager>>,
    logo_svc: &Arc<LogoService>,
    shared_state: &Arc<Mutex<AppSnapshot>>,
    edited: EditedFavorite<'_>,
) {
    let EditedFavorite {
        old,
        id,
        name,
        url,
        logo_url,
        country,
    } = edited;
    // Drop the old cached logo (cache key = station id = url hash)
    if let Some(old_fav) = old {
        logo_svc.delete(old_fav);
    }
    invalidate_logo_image(id);

    // Update playback UI if this is the currently playing station
    let current_url = ui.get_station_url().to_string();
    // Check both old URL (by id match) and new URL
    let is_current =
        current_url == url || radiotrope_app::data::types::url_to_id(&current_url) == id;
    if is_current {
        ui.set_station_name(name.into());
        ui.set_station_logo_url(logo_url.into());
        ui.set_station_country(country.into());
        ui.set_station_url(url.into());
        // Also update shared_state so the 200ms poll timer doesn't overwrite
        let mut s = lock(shared_state);
        s.station_name = Some(name.to_string());
        s.station_url = Some(url.to_string());
        s.station_logo_url = Some(logo_url.to_string()).filter(|l| !l.is_empty());
        s.station_country = Some(country.to_string()).filter(|c| !c.is_empty());
    }

    // Refresh favorites list immediately (logos will show placeholders for new URLs)
    refresh_favorites(ui, favs, logo_svc);

    // Fetch the new logo on a background thread, then refresh UI
    if !logo_url.is_empty() {
        let logo_svc = logo_svc.clone();
        let favs = favs.clone();
        let ui_weak = ui.as_weak();
        let tmp_station = Station::new(name, url).with_logo(logo_url);
        let url = url.to_string();
        // Shown in the player only if it is the station there now
        let shown = SHOWN_STATION.with(|s| s.borrow().seq());
        std::thread::Builder::new()
            .name("edit-logo-fetch".into())
            .spawn(move || {
                if let Some((rgba, width, height)) = logo_svc.get_rgba(&tmp_station) {
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        // Update playback logo if this is the current station
                        if is_current_station(shown, &url) {
                            set_header_logo(&ui, rgba, width, height);
                        }
                        // Refresh favorites list with the now-cached logo
                        refresh_favorites(&ui, &favs, &logo_svc);
                    });
                }
            })
            .ok();
    }
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

/// Remove a cached logo image, forcing re-decode on next refresh.
fn invalidate_logo_image(id: &str) {
    LOGO_IMAGE_CACHE.with(|cache| {
        cache.borrow_mut().remove(id);
    });
}

/// Show the favorites file dialog with only a message (an export done, or
/// what went wrong)
fn show_favorites_file_message(ui: &App, title: &str, message: &str, detail: &str, error: bool) {
    ui.set_import_title(title.into());
    ui.set_import_subtitle(Default::default());
    ui.set_import_message(message.into());
    ui.set_import_message_detail(detail.into());
    ui.set_import_message_error(error);
    ui.set_show_import_favorites(true);
}

/// What taking in a list would change, for Import Favorites: "Adds 12,
/// keeps 30"
#[cfg(feature = "desktop")]
fn import_preview_text(mode: ImportMode, o: &ImportOutcome) -> String {
    if !o.changes() {
        return match mode {
            ImportMode::Add => "You have them all already.".into(),
            ImportMode::Replace => "Nothing would change.".into(),
        };
    }
    let mut parts = Vec::new();
    if o.added > 0 {
        parts.push(format!("adds {}", o.added));
    }
    if o.updated > 0 {
        parts.push(format!("updates {}", o.updated));
    }
    if o.removed > 0 {
        parts.push(format!("removes {}", o.removed));
    }
    if mode == ImportMode::Add && o.kept > 0 {
        parts.push(format!("keeps {}", o.kept));
    }
    if mode == ImportMode::Replace && o.reordered {
        parts.push("new order".into());
    }
    let mut text = parts.join(", ");
    if let Some(first) = text.get(..1) {
        text = first.to_uppercase() + &text[1..];
    }
    if o.over_cap > 0 {
        text += &format!(
            ". {} don't fit ({} favorites at most)",
            o.over_cap,
            control::MAX_FAVORITES
        );
    }
    text + "."
}

/// What Import Favorites did
fn import_done_text(mode: ImportMode, o: &ImportOutcome, listed: usize) -> String {
    if !o.changes() {
        return "Nothing changed: your favorites already match.".into();
    }
    match mode {
        ImportMode::Add => {
            let mut text = match o.added {
                1 => "Added 1 station.".to_string(),
                n => format!("Added {n} stations."),
            };
            if o.over_cap > 0 {
                text += &format!(" {} didn't fit.", o.over_cap);
            }
            text
        }
        ImportMode::Replace => format!("Your favorites are now the file's {listed} stations."),
    }
}

/// Save the favorites to a file the user picks
#[cfg(feature = "desktop")]
fn export_favorites(ui_weak: slint::Weak<App>, favs: Arc<Mutex<FavoritesManager>>) {
    let Some(ui) = ui_weak.upgrade() else { return };
    let save = rfd::AsyncFileDialog::new()
        .set_title("Export Favorites")
        .set_file_name("Radiotrope Favorites.json")
        .add_filter("Radiotrope favorites", &["json"])
        .set_parent(&ui.window().window_handle())
        .save_file();
    let spawned = slint::spawn_local(async move {
        let Some(file) = save.await else { return };
        let Some(ui) = ui_weak.upgrade() else { return };
        let path = file.path().to_path_buf();
        let saved = lock(&favs).export_to(&path);
        match saved {
            Ok(count) => show_favorites_file_message(
                &ui,
                "Export Favorites",
                &format!("Saved {count} favorites. To restore them, use Import Favorites."),
                &path.display().to_string(),
                false,
            ),
            Err(e) => {
                show_favorites_file_message(&ui, "Export Favorites", &e.to_string(), "", true)
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("Failed to open the file dialog: {e}");
    }
}

/// Pick a favorites file, then ask Add Missing or Replace
#[cfg(feature = "desktop")]
fn import_favorites(
    ui_weak: slint::Weak<App>,
    favs: Arc<Mutex<FavoritesManager>>,
    pending: std::rc::Rc<std::cell::RefCell<Vec<radiotrope_app::data::types::Favorite>>>,
) {
    let Some(ui) = ui_weak.upgrade() else { return };
    let pick = rfd::AsyncFileDialog::new()
        .set_title("Import Favorites")
        .add_filter("Radiotrope favorites", &["json"])
        .set_parent(&ui.window().window_handle())
        .pick_file();
    let spawned = slint::spawn_local(async move {
        let Some(file) = pick.await else { return };
        let Some(ui) = ui_weak.upgrade() else { return };
        let path = file.path().to_path_buf();
        let list = match radiotrope_app::data::favorites::read_list(&path) {
            Ok(list) if !list.is_empty() => list,
            Ok(_) => {
                return show_favorites_file_message(
                    &ui,
                    "Import Favorites",
                    "The file has no favorites.",
                    "",
                    true,
                )
            }
            Err(e) => {
                return show_favorites_file_message(
                    &ui,
                    "Import Favorites",
                    &e.to_string(),
                    "",
                    true,
                )
            }
        };
        let previews = {
            let mut f = lock(&favs);
            [ImportMode::Add, ImportMode::Replace].map(|mode| {
                f.import_list(mode, &list, control::MAX_FAVORITES, true)
                    .map(|o| import_preview_text(mode, &o))
                    .unwrap_or_else(|e| e.to_string())
            })
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.set_import_title("Import Favorites".into());
        ui.set_import_subtitle(
            match list.len() {
                1 => format!("{name}: 1 station"),
                n => format!("{name}: {n} stations"),
            }
            .into(),
        );
        let [add, replace] = previews;
        ui.set_import_add_summary(add.into());
        ui.set_import_replace_summary(replace.into());
        ui.set_import_message(Default::default());
        ui.set_import_message_detail(Default::default());
        ui.set_import_message_error(false);
        *pending.borrow_mut() = list;
        ui.set_show_import_favorites(true);
    });
    if let Err(e) = spawned {
        eprintln!("Failed to open the file dialog: {e}");
    }
}

/// The kiosk build has no file dialog (the menu items are greyed out)
#[cfg(not(feature = "desktop"))]
fn export_favorites(_ui_weak: slint::Weak<App>, _favs: Arc<Mutex<FavoritesManager>>) {}

#[cfg(not(feature = "desktop"))]
fn import_favorites(
    _ui_weak: slint::Weak<App>,
    _favs: Arc<Mutex<FavoritesManager>>,
    _pending: std::rc::Rc<std::cell::RefCell<Vec<radiotrope_app::data::types::Favorite>>>,
) {
}

/// Redraw the favorites list, and start fetching the logos it lacks
fn refresh_favorites(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
) {
    // The logos are read and decoded after the lock is let go: an agent
    // waiting on the favorites isn't held up by them
    let (sorted, items): (
        Vec<radiotrope_app::data::types::Favorite>,
        Vec<FavoriteStation>,
    ) = {
        let favs = lock(favorites);
        mark_browse_favorites(ui, &favs);
        note_favorites_shown(&favs);
        favs.sorted(FavoriteSort::Manual)
            .into_iter()
            .map(|f| (f.clone(), favorite_to_slint(f, favs.stats(&f.id()))))
            .unzip()
    };

    // Build the parallel logo model, using in-memory cache to avoid
    // re-decoding
    let mut missing = Vec::new();
    let logos: Vec<slint::Image> = LOGO_IMAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        sorted
            .iter()
            .map(|f| {
                let key = f.id();
                let written = logo_service.cached_modified(f);
                let logo = match cache.get(&key) {
                    Some((logo, at)) if *at == written => Some(logo.clone()),
                    _ => logo_service
                        .get_cached_rgba(f)
                        .map(|(rgba, width, height)| {
                            let logo = logo_image(rgba, width, height);
                            cache.insert(key, (logo.clone(), written));
                            logo
                        }),
                };
                match logo {
                    // An out of date logo shows until its new one arrives
                    Some(logo) => {
                        if logo_service.is_stale(f) {
                            missing.push(f.clone());
                        }
                        logo
                    }
                    None => {
                        if logo_service.is_missing(f) {
                            missing.push(f.clone());
                        }
                        Default::default()
                    }
                }
            })
            .collect()
    });

    ui.set_favorites_list(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    ui.set_favorite_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));

    if !missing.is_empty() {
        let ui_weak = ui.as_weak();
        let favorites = favorites.clone();
        let service = logo_service.clone();
        logo_service.prefetch_in_background(missing, move || {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    refresh_favorites(&ui, &favorites, &service);
                }
            });
        });
    }
}

/// The name shown for a stream without one
const UNNAMED_STATION: &str = "Radiotrope";

/// The status of a station that is starting (as the controller sets it)
const STARTING_STATUS: &str = status::CONNECTING;

/// The codec line of a station not playing yet
const AWAITING_STREAM: &str = "Waiting for stream";

fn format_codec_line(s: &AppSnapshot) -> String {
    if s.codec_name.is_empty() {
        return AWAITING_STREAM.to_string();
    }
    let mut parts = Vec::new();
    if s.stream_type == "HLS" {
        parts.push(format!("{} (HLS)", s.codec_name));
    } else {
        parts.push(s.codec_name.clone());
    }
    if let Some(br) = s.bitrate.filter(|&b| b > 0) {
        parts.push(format!("{} kbps", br));
    }
    if s.sample_rate > 0 {
        parts.push(text::sample_rate(s.sample_rate));
    }
    if s.channels > 0 {
        parts.push(match s.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{} ch", n),
        });
    }
    parts.join(SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_line_joins_with_middle_dots() {
        let mut s = AppSnapshot::default();
        assert_eq!(format_codec_line(&s), "Waiting for stream");
        s.codec_name = "MP3".into();
        s.bitrate = Some(128);
        s.sample_rate = 44_100;
        s.channels = 2;
        assert_eq!(
            format_codec_line(&s),
            "MP3 \u{b7} 128 kbps \u{b7} 44.1 kHz \u{b7} Stereo"
        );
    }

    #[test]
    fn listening_time_says_min() {
        assert_eq!(format_listen_time(40 * 60), "40 min");
        assert_eq!(format_listen_time(3 * 3600 + 25 * 60), "3 h 25 min");
    }

    #[test]
    fn filter_entries_take_the_kind_asked_for_with_the_code_the_directory_wants() {
        use radiotrope_app::providers::types::Category;
        let categories = || {
            vec![
                Category::new("rock", "Rock", CategoryType::Genre).with_station_count(9),
                Category::new("ambient", "Ambient", CategoryType::Genre),
                Category::new("GR", "Greece", CategoryType::Country)
                    .with_code(Some("GR".into()))
                    .with_station_count(4),
                // No code: the directory can't be asked for it
                Category::new("Atlantis", "Atlantis", CategoryType::Country),
                Category::new("ell", "Greek", CategoryType::Language),
            ]
        };

        let genres = filter_entries("genre", categories());
        let names: Vec<_> = genres.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, ["Ambient", "Rock"], "A to Z");
        assert_eq!(genres[1].code, "rock", "the slug");
        assert_eq!(genres[1].station_count, 9);

        let countries = filter_entries("country", categories());
        assert_eq!(countries.len(), 1);
        assert_eq!(
            (countries[0].name.as_str(), countries[0].code.as_str()),
            ("Greece", "GR")
        );

        // Languages aren't a filter
        assert!(filter_entries("genre", categories())
            .iter()
            .all(|e| e.name != "Greek"));
    }

    #[test]
    fn a_browse_query_asks_only_for_what_is_given() {
        assert_eq!(BrowseQuery::default().filter(), StationFilter::default());
        let query = BrowseQuery {
            name: "kiss".into(),
            country: "GR".into(),
            ..Default::default()
        };
        let filter = query.filter();
        assert_eq!(filter.name.as_deref(), Some("kiss"));
        assert_eq!(filter.country.as_deref(), Some("GR"));
        assert_eq!(filter.genre, None);
    }
}
