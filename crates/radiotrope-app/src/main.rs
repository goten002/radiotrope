// On Windows the release build is a GUI program, so no console window opens
// next to it. Debug builds keep the console for their logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod instance;
mod mcp;
mod row_logos;
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

use radiotrope::audio::health::HealthState;
use radiotrope::audio::{AudioAnalysis, PlaybackState, SharedStats, StreamStats};
use radiotrope::stream::StreamType;

use radiotrope_app::config::ui::{
    FAVORITES_SAVE_DELAY, LISTEN_CREDIT_SECS, MIN_LISTEN_SECS, PLAY_TAKE_TIMEOUT,
    RECORDING_NOTICE_TIME, SEARCH_PAGE_SIZE, SHUTDOWN_GRACE, SHUTDOWN_SEND_TIMEOUT,
};
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::recordings;
use radiotrope_app::data::types::{FavoriteSort, Station};
use radiotrope_app::error::ServiceProblem;
use radiotrope_app::network::browse_logos::BrowseLogos;
use radiotrope_app::network::logo::LogoService;
use radiotrope_app::providers::types::{Category, CategoryType, SearchResults};
use radiotrope_app::providers::ProviderRegistry;
use radiotrope_app::visual::{self, gate, logo_palette, LevelSmoother};

use app::controller::AppController;
use app::delayed_save::DelayedSave;
use app::listening::ListenSession;
use app::picked_station::PendingPick;
use app::shown_station::ShownStation;
use app::state::AppSnapshot;
use app::ui_sender::UiSender;

/// Radiotrope — Internet radio player
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Connect an AI agent over MCP on stdio. Talks to the running player,
    /// starting it first if needed, so all agents share one player.
    #[arg(long)]
    mcp: bool,

    /// With --mcp: run a separate player for this agent alone instead
    #[arg(long, requires = "mcp")]
    standalone: bool,
}

fn main() {
    #[cfg(windows)]
    windows_console::set_up();

    let args = Args::parse();

    // `--mcp` relays the agent to the running player and never opens a
    // window of its own
    if args.mcp && !args.standalone {
        std::process::exit(mcp::local::run_relay());
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
            let mut favs = favorites.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut state = shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.volume = settings.volume;
        state.is_muted = settings.muted;
        state.eq_gains = settings.eq_gains;
        state.eq_preamp = settings.eq_preamp;
        state.eq_enabled = settings.eq_enabled;
        state.eq_preset_name = settings.eq_preset_name.clone();
        state.accent_color = settings.accent_color.clone();
        state.recording_setup = app::state::RecordingSetup::from_settings(&settings);
    }

    // Create Slint UI
    let ui = App::new().unwrap();

    // Wayland app_id / X11 WM_CLASS: desktops match it against the
    // `radiotrope.desktop` file (and its StartupWMClass) to find the icon.
    // A no-op on Windows and macOS.
    let _ = slint::set_xdg_app_id("radiotrope");

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
        std::thread::Builder::new()
            .name("mcp-local".into())
            .spawn(move || {
                mcp::local::serve(&instance, tools, show_window);
            })
            .expect("Failed to spawn MCP thread");
        agents
    });
    // Network agents, when turned on, and the Agents dialog
    let _network_timer = setup_agents(&ui, agents.clone(), &settings);
    let _agents_timer = watch_agents(&ui, agents.as_ref().map(|a| a.tools().presence()));

    // Initial load of favorites into UI model
    migrate_logo_ids(&favorites, settings.last_station.as_ref(), &logo_service);
    refresh_favorites(&ui, &favorites, &logo_service);

    // Apply initial settings to UI
    ui.set_volume(settings.volume);
    ui.set_is_muted(settings.muted);
    ui.set_eq_enabled(settings.eq_enabled);
    ui.set_eq_preset_name(settings.eq_preset_name.as_deref().unwrap_or("").into());
    ui.set_eq_preamp(settings.eq_preamp);
    ui.set_eq_band0(settings.eq_gains[0]);
    ui.set_eq_band1(settings.eq_gains[1]);
    ui.set_eq_band2(settings.eq_gains[2]);
    ui.set_eq_band3(settings.eq_gains[3]);
    ui.set_eq_band4(settings.eq_gains[4]);
    ui.set_eq_band5(settings.eq_gains[5]);
    ui.set_eq_band6(settings.eq_gains[6]);
    ui.set_eq_band7(settings.eq_gains[7]);
    ui.set_eq_band8(settings.eq_gains[8]);
    ui.set_eq_band9(settings.eq_gains[9]);
    show_eq_presets(&ui);

    // Apply saved accent color
    if let Some((r, g, b)) = settings.accent_color_rgb() {
        ui.set_accent_color(slint::Color::from_rgb_u8(r, g, b));
    }

    ui.set_touch_scroll(cfg!(feature = "embedded"));

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

    // Apply saved theme and viz mode
    ui.set_dark_mode(settings.theme.is_dark());
    // Modes that no longer exist (the old curve, Waterfall) fall back to Wave
    let viz_mode = match settings.viz_mode.as_str() {
        m @ ("wave" | "spectrum" | "mirror" | "dots" | "vu" | "hbars") => m,
        _ => "wave",
    };
    ui.set_viz_mode(viz_mode.into());
    ui.global::<VizStyle>()
        .set_palette(settings.viz_palette.as_str().into());
    ui.set_show_station_stats(settings.show_station_stats);
    ui.set_show_visualizer(settings.show_visualizer);
    ui.set_panel_gradient(settings.panel_gradient);
    // The Pi build is full screen with no frame to replace
    if cfg!(not(feature = "embedded")) {
        ui.set_custom_frame(settings.custom_title_bar);
        #[cfg(feature = "desktop")]
        {
            let ui_weak = ui.as_weak();
            ui.on_minimize_window(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    window_frame::minimize(ui.window());
                }
            });
            let ui_weak = ui.as_weak();
            ui.on_switch_title_bar(move |custom| {
                if let Some(ui) = ui_weak.upgrade() {
                    window_frame::set_system_frame(ui.window(), !custom);
                    ui.set_custom_frame(custom);
                }
            });
            let ui_weak = ui.as_weak();
            ui.on_show_system_menu(move |x, y, pressed| {
                ui_weak
                    .upgrade()
                    .is_some_and(|ui| window_frame::show_system_menu(ui.window(), x, y, pressed))
            });
        }
        // Window managers ignore the level on a window that isn't mapped
        // yet (seen on X11), so raise it once the window is up
        if settings.always_on_top {
            let ui_weak = ui.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_keep_on_top(true);
                }
            });
        }
    }

    // Apply saved window size
    if let (Some(w), Some(h)) = (settings.window_width, settings.window_height) {
        ui.window()
            .set_size(slint::LogicalSize::new(w as f32, h as f32));
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
            let favs = favorites.lock().unwrap_or_else(|e| e.into_inner());
            ui.set_is_station_favorited(favs.is_favorite(&station.url));
        }
        // Try to load cached logo
        if let Some(ref logo_url) = station.logo_url {
            if !logo_url.is_empty() {
                let tmp = Station::new(&station.name, &station.url).with_logo(logo_url);
                if let Some((rgba, w, h)) = logo_service.get_cached_rgba(&tmp) {
                    let pb = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                    ui.set_current_logo(slint::Image::from_rgba8(pb));
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
                                        let pb = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                                        ui.set_current_logo(slint::Image::from_rgba8(pb));
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
            let mut s = shared_state.lock().unwrap_or_else(|e| e.into_inner());
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
        let is_muted = mute_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_muted;
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
            let name = state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .station_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| radiotrope_app::data::types::name_from_url(&url));
            let logo_url = ui.get_station_logo_url().to_string();
            let country = ui.get_station_country().to_string();

            let mut f = favs.lock().unwrap_or_else(|e| e.into_inner());
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

    // reset-favorite-stats callback
    {
        let favs = favorites.clone();
        let ui_weak = ui.as_weak();
        ui.on_reset_favorite_stats(move |id| {
            let mut f = favs.lock().unwrap_or_else(|e| e.into_inner());
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
            if let Some(map) = SESSION_LISTEN
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
            {
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
            let mut f = favs.lock().unwrap_or_else(|e| e.into_inner());
            let old_fav = f.get(&id).cloned();
            if let Err(e) = f.update(&id, update) {
                return if f.get_id_for_url(&url) != id && f.is_favorite(&url) {
                    "Another favorite already has this stream URL".into()
                } else {
                    e.to_string().into()
                };
            }
            let _ = f.save();
            drop(f);

            // Drop the old cached logo (cache key = station id = url hash)
            if let Some(old_fav) = &old_fav {
                logo_svc.delete(old_fav);
            }
            invalidate_logo_image(&id);

            // Update playback UI if this is the currently playing station
            if let Some(ui) = ui_weak.upgrade() {
                let current_url = ui.get_station_url().to_string();
                // Check both old URL (by id match) and new URL
                let is_current = current_url == url
                    || radiotrope_app::data::types::url_to_id(&current_url) == id;
                if is_current {
                    ui.set_station_name(name.as_str().into());
                    ui.set_station_logo_url(logo_url.as_str().into());
                    ui.set_station_country(country.as_str().into());
                    ui.set_station_url(url.as_str().into());
                    // Also update shared_state so the 200ms poll timer doesn't overwrite
                    let mut s = edit_shared_state.lock().unwrap_or_else(|e| e.into_inner());
                    s.station_name = Some(name.clone());
                    s.station_url = Some(url.clone());
                    s.station_logo_url = Some(logo_url.clone()).filter(|l| !l.is_empty());
                    s.station_country = Some(country.clone()).filter(|c| !c.is_empty());
                }

                // Refresh favorites list immediately (logos will show placeholders for new URLs)
                refresh_favorites(&ui, &favs, &logo_svc);
            }

            // Fetch the new logo on a background thread, then refresh UI
            if !logo_url.is_empty() {
                let logo_svc = logo_svc.clone();
                let favs = favs.clone();
                let ui_weak = ui_weak.clone();
                let url = url.clone();
                // Shown in the player only if it is the station there now
                let shown = SHOWN_STATION.with(|s| s.borrow().seq());
                std::thread::Builder::new()
                    .name("edit-logo-fetch".into())
                    .spawn(move || {
                        let tmp_station = Station::new(&name, &url).with_logo(&logo_url);
                        if let Some((rgba, width, height)) = logo_svc.get_rgba(&tmp_station) {
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = ui_weak.upgrade() else { return };
                                // Update playback logo if this is the current station
                                if is_current_station(shown, &url) {
                                    let pixel_buf =
                                        SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                            &rgba, width, height,
                                        );
                                    ui.set_current_logo(slint::Image::from_rgba8(pixel_buf));
                                }
                                // Refresh favorites list with the now-cached logo
                                refresh_favorites(&ui, &favs, &logo_svc);
                            });
                        }
                    })
                    .ok();
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
            let mut f = favs.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut f = favs.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut f = favs.lock().unwrap_or_else(|e| e.into_inner());
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
                let fogs_model = ui.get_favorite_logo_fogs();
                let mut items: Vec<FavoriteStation> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .collect();
                let mut logos: Vec<slint::Image> = (0..logos_model.row_count())
                    .filter_map(|i| logos_model.row_data(i))
                    .collect();
                let mut fogs: Vec<LogoFog> = fogs_model.iter().collect();

                if from < items.len() && to < items.len() {
                    let item = items.remove(from);
                    items.insert(to, item);
                    if from < logos.len() && to < logos.len() {
                        let logo = logos.remove(from);
                        logos.insert(to, logo);
                    }
                    if from < fogs.len() && to < fogs.len() {
                        let fog = fogs.remove(from);
                        fogs.insert(to, fog);
                    }
                }

                ui.set_favorites_list(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                ui.set_favorite_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
                ui.set_favorite_logo_fogs(ModelRc::from(std::rc::Rc::new(VecModel::from(fogs))));
            }

            // Save to disk in background
            let favs_bg = favs.clone();
            std::thread::Builder::new()
                .name("fav-save".into())
                .spawn(move || {
                    let mut f = favs_bg.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = f.save();
                })
                .ok();
        });
    }

    // EQ callbacks
    {
        let tx = ui_tx.clone();
        ui.on_eq_band_changed(move |band, gain| {
            tx.send(app::state::AppCommand::SetEqBand {
                band: band as usize,
                gain_db: gain,
            });
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_eq_preamp_changed(move |val| {
            tx.send(app::state::AppCommand::SetEqPreamp(val));
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_eq_preset_selected(move |name| {
            tx.send(app::state::AppCommand::SetEqPreset(name.to_string()));
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_eq_enabled_toggled(move |val| {
            tx.send(app::state::AppCommand::SetEqEnabled(val));
        });
    }

    // Accent color callbacks
    {
        let state = shared_state.clone();
        ui.on_accent_color_changed(move |color| {
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                color.red(),
                color.green(),
                color.blue()
            );
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.accent_color = Some(hex);
        });
    }
    {
        let state = shared_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_apply_custom_hex(move |hex_str| {
            let Some((r, g, b)) = radiotrope_app::data::settings::parse_hex_rgb(&hex_str) else {
                return;
            };
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_accent_color(slint::Color::from_rgb_u8(r, g, b));
            }
            let hex_with_hash = format!("#{:02x}{:02x}{:02x}", r, g, b);
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.accent_color = Some(hex_with_hash);
        });
    }

    // WiFi settings (Raspberry Pi build only)
    #[cfg(feature = "embedded")]
    setup_wifi(&ui);

    setup_about(&ui);
    setup_text_util(&ui);
    setup_recording(
        &ui,
        &settings,
        ui_tx.clone(),
        shared_state.clone(),
        logo_service.clone(),
    );

    // Rotary encoder for volume control (GPIO 5=CLK, GPIO 6=DT, GPIO 13=SW)
    // Disabled: embedded-only hardware, parked for now
    // setup_rotary_encoder(&ui, cmd_tx.clone(), shared_state.clone());

    // What the station browser shows, and how far "Load More" has paged
    let browse_state = Arc::new(Mutex::new((BrowseQuery::Top, 0usize)));

    // Logos of the browser rows on screen, kept small on disk
    let browse_logos = Arc::new(BrowseLogos::open());
    // The caches are trimmed now (the logos the favorites lack are being
    // fetched already, and the restored station is in the shared state)
    // and then once a day
    prune_caches_daily(
        favorites.clone(),
        favorites_loaded,
        shared_state.clone(),
        logo_service.clone(),
        browse_logos.clone(),
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

    // search-stations callback (an empty query shows the top stations)
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_search_stations(move |query| {
            let query = query.trim();
            let query = if query.is_empty() {
                BrowseQuery::Top
            } else {
                BrowseQuery::Search(query.to_string())
            };
            start_browse(&ui_weak, &state, &gen, &row_logos, &favs, query);
        });
    }

    // load-top-stations callback
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_load_top_stations(move || {
            start_browse(&ui_weak, &state, &gen, &row_logos, &favs, BrowseQuery::Top);
        });
    }

    // browse-country callback (an empty query lists the whole country)
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_browse_country(move |name, code, query| {
            let category = Category::new(name.as_str(), name.as_str(), CategoryType::Country)
                .with_code(Some(code.to_string()).filter(|c| !c.is_empty()));
            let query = BrowseQuery::Category {
                category,
                query: query.trim().to_string(),
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
                let s = state.lock().unwrap_or_else(|e| e.into_inner());
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
                                state.lock().unwrap_or_else(|e| e.into_inner()).1 = offset;
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
                let query = state.lock().unwrap_or_else(|e| e.into_inner()).0.clone();
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
            let mut f = favs.lock().unwrap_or_else(|e| e.into_inner());
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
                            "radio-browser",
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

    // Keep each browser mode's list (search, and the last country) while
    // the other is shown, so reopening either finds it as it was left
    {
        let stash: std::rc::Rc<std::cell::RefCell<HashMap<String, BrowseStash>>> =
            Default::default();
        {
            let ui_weak = ui.as_weak();
            let stash = stash.clone();
            let state = browse_state.clone();
            let row_logos = row_logos.clone();
            ui.on_stash_browse(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                // Nothing worth keeping while it loads or after an error
                if ui.get_search_loading() || ui.get_search_results().row_count() == 0 {
                    return;
                }
                let (query, offset) = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                // The list is kept without logos; they load again when shown
                row_logos.release(&ui);
                stash.borrow_mut().insert(
                    ui.get_browse_mode().to_string(),
                    BrowseStash {
                        country: ui.get_browse_country_name().to_string(),
                        query,
                        offset,
                        results: ui.get_search_results(),
                        logos: ui.get_browse_logos(),
                        fogs: ui.get_browse_logo_fogs(),
                        has_more: ui.get_has_more(),
                        typed: ui.get_browse_typed(),
                        shown: ui.get_browse_shown(),
                        scroll: ui.get_browse_scroll(),
                    },
                );
            });
        }
        {
            let ui_weak = ui.as_weak();
            let state = browse_state.clone();
            let gen = browse_logo_gen.clone();
            let favs = favorites.clone();
            let row_logos = row_logos.clone();
            ui.on_restore_browse(move |mode, country| {
                let Some(ui) = ui_weak.upgrade() else {
                    return false;
                };
                let kept = stash.borrow().get(mode.as_str()).cloned();
                let Some(kept) =
                    kept.filter(|k| mode != "country" || k.country == country.as_str())
                else {
                    return false;
                };
                // Drop replies still on their way for the list being hidden
                gen.fetch_add(1, Ordering::Relaxed);
                *state.lock().unwrap_or_else(|e| e.into_inner()) = (kept.query, kept.offset);
                ui.set_browse_mode(mode);
                ui.set_search_results(kept.results);
                ui.set_browse_logos(kept.logos.clone());
                ui.set_browse_logo_fogs(kept.fogs.clone());
                ui.set_has_more(kept.has_more);
                ui.set_search_error(Default::default());
                ui.set_search_loading(false);
                ui.set_search_loading_more(false);
                ui.set_browse_typed(kept.typed);
                ui.set_browse_shown(kept.shown);
                ui.set_browse_scroll(kept.scroll);
                mark_browse_favorites(&ui, &favs.lock().unwrap_or_else(|e| e.into_inner()));
                row_logos.refresh(&ui);
                true
            });
        }
    }

    // load-countries callback
    {
        let ui_weak = ui.as_weak();
        ui.on_load_countries(move || {
            let ui_weak = ui_weak.clone();
            std::thread::Builder::new()
                .name("load-countries".into())
                .spawn(move || {
                    let results = ProviderRegistry::with_defaults().and_then(|r| {
                        r.get("radio-browser")
                            .ok_or_else(|| {
                                radiotrope_app::error::AppError::NotFound(
                                    "radio-browser provider not found".into(),
                                )
                            })
                            .and_then(|p| p.browse_categories())
                    });
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        match results {
                            Ok(categories) => {
                                let mut countries: Vec<CountryEntry> = categories
                                    .into_iter()
                                    .filter(|c| c.category_type == CategoryType::Country)
                                    .map(|c| CountryEntry {
                                        flag: flag_image(c.code.as_deref(), Some(&c.name)),
                                        code: c.code.as_deref().unwrap_or("").into(),
                                        name: c.name.as_str().into(),
                                        station_count: c.station_count.unwrap_or(0) as i32,
                                    })
                                    .collect();
                                countries.sort_by(|a, b| {
                                    a.name.to_lowercase().cmp(&b.name.to_lowercase())
                                });
                                ALL_COUNTRIES.with(|all| *all.borrow_mut() = countries);
                                show_countries(&ui, &ui.get_country_filter());
                                ui.set_country_error(Default::default());
                            }
                            Err(e) => {
                                ui.set_country_error(format!("{e}").into());
                            }
                        }
                        ui.set_country_loading(false);
                    });
                })
                .ok();
        });
    }

    // filter-countries callback
    {
        let ui_weak = ui.as_weak();
        ui.on_filter_countries(move |text| {
            if let Some(ui) = ui_weak.upgrade() {
                show_countries(&ui, &text);
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
            }
        });
    }

    // TODO: set up system tray when mcp_mode is true

    // Spawn controller on its own thread
    let ctrl_state = shared_state.clone();
    let ctrl_tx = cmd_tx.clone();
    let controller = std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || {
            let mut ctrl = AppController::new(cmd_rx, ctrl_tx, ctrl_state, analysis_tx, stats_tx);
            ctrl.run();
        })
        .expect("Failed to spawn controller thread");

    // The engine sends its analysis Arc and SharedStats once its audio
    // device is open, which can take a while (a Bluetooth device waking
    // up). The window doesn't wait for them: the timers below pick them up
    // when they come.

    // Visualization timer, about 30 frames a second. It runs only while
    // there is something to show (see `viz_wanted`): it stops itself, and
    // the poll below or `live_views_changed` start it again.
    let viz_timer = std::rc::Rc::new(slint::Timer::default());
    {
        let mut analysis: Option<Arc<Mutex<AudioAnalysis>>> = None;
        let ui_weak = ui.as_weak();
        let bands = radiotrope::config::audio::SPECTRUM_BANDS;
        // Pre-allocate the model once; update in place each tick
        let spectrum_model = std::rc::Rc::new(VecModel::from(vec![0.0f32; bands]));
        let viz = ui.global::<VizData>();
        viz.set_spectrum(ModelRc::from(spectrum_model.clone()));
        let mut spectrum_smooth = LevelSmoother::new(
            bands,
            visual::SPECTRUM_RISE_SECS,
            visual::SPECTRUM_FALL_SECS,
        );
        let mut vu_smooth = LevelSmoother::new(2, visual::VU_RISE_SECS, visual::VU_FALL_SECS);
        // Dot Matrix columns and peaks
        let extras = VizExtras::new(&viz);
        let mut peak_hold = visual::PeakHold::new(visual::MATRIX_COLUMNS);
        let mut shown_mode = slint::SharedString::default();
        let mut gated = vec![0.0f32; bands];
        let mut idle = true;
        let mut last_frame = Instant::now();
        let this = std::rc::Rc::downgrade(&viz_timer);
        viz_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(33),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let viz = ui.global::<VizData>();
                // Seconds since the last frame, capped so a stall doesn't
                // make the bars jump
                let now = Instant::now();
                let dt = now.duration_since(last_frame).as_secs_f32().min(0.1);
                last_frame = now;
                // Nothing to show: zero out once, and stop until wanted
                if !viz_wanted(&ui) {
                    if !idle {
                        idle = true;
                        viz.set_active(false);
                        spectrum_smooth.reset();
                        vu_smooth.reset();
                        gated.fill(0.0);
                        show_viz_frame(&viz, &[0.0, 0.0], &gated, &spectrum_model);
                        peak_hold.reset();
                        extras.clear();
                    }
                    if let Some(timer) = this.upgrade() {
                        timer.stop();
                    }
                    return;
                }
                if analysis.is_none() {
                    analysis = analysis_rx.try_recv().ok();
                }
                let Some(analysis) = &analysis else { return };
                if idle {
                    idle = false;
                    viz.set_active(true);
                }
                // try_lock: skip this tick if engine/analyzer holds the lock
                let Ok(mut a) = analysis.try_lock() else {
                    return;
                };
                // On screen: keep the analyzer working it out (it stops
                // soon after nobody shows it)
                a.mark_shown();
                let (vu_l, vu_r, spectrum) = (a.vu_left, a.vu_right, a.spectrum);
                drop(a);
                for (g, &level) in gated.iter_mut().zip(spectrum.iter()) {
                    *g = gate(level);
                }
                let vu = vu_smooth.update(&[vu_l, vu_r], dt).to_vec();
                let smooth = spectrum_smooth.update(&gated, dt);
                show_viz_frame(&viz, &vu, smooth, &spectrum_model);
                // The Dot Matrix data is only worked out while it shows,
                // starting fresh each time it is picked
                let mode = ui.get_viz_mode();
                if mode != shown_mode {
                    peak_hold.reset();
                    extras.clear();
                    shown_mode = mode.clone();
                }
                if mode == "dots" {
                    let cols = visual::column_levels(smooth, visual::MATRIX_COLUMNS);
                    let peaks = peak_hold.update(&cols, dt).to_vec();
                    extras.show_columns(&cols, &peaks);
                }
            },
        );
    }

    // The engine's SharedStats, once it has sent them
    let engine_stats = {
        let shared_stats: std::cell::RefCell<Option<SharedStats>> = Default::default();
        std::rc::Rc::new(move || -> Option<SharedStats> {
            let mut shared_stats = shared_stats.borrow_mut();
            if shared_stats.is_none() {
                *shared_stats = stats_rx.try_recv().ok();
            }
            shared_stats.clone()
        })
    };
    // SharedStats → statistics dialog properties
    let show_stats = {
        let ui_weak = ui.as_weak();
        let engine_stats = engine_stats.clone();
        std::rc::Rc::new(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            // Stopped: clear what the last station left behind, so the
            // dialog doesn't keep showing it as live. Connecting still
            // polls (the buffer fills, the status reads Connecting)
            if !ui.get_is_playing() && !ui.get_is_loading() {
                clear_stats_ui(&ui);
                return;
            }
            let Some(shared_stats) = engine_stats() else {
                return;
            };
            // try_lock: skip this tick if engine holds shared_stats
            let Ok(s) = shared_stats.try_lock() else {
                return;
            };
            let stats_copy = s.clone();
            drop(s);
            update_stats_ui(&ui, &stats_copy);
        })
    };
    // Every 200 ms while the dialog is open; it stops itself once closed
    let stats_dialog_timer = std::rc::Rc::new(slint::Timer::default());
    {
        let ui_weak = ui.as_weak();
        let show_stats = show_stats.clone();
        let this = std::rc::Rc::downgrade(&stats_dialog_timer);
        stats_dialog_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                if !ui.get_stats_open() {
                    if let Some(timer) = this.upgrade() {
                        timer.stop();
                    }
                    return;
                }
                show_stats();
            },
        );
    }
    // The visualizer was shown or hidden, or the statistics dialog opened
    // or closed: start what feeds them
    {
        let ui_weak = ui.as_weak();
        let viz_timer = viz_timer.clone();
        let stats_dialog_timer = stats_dialog_timer.clone();
        ui.on_live_views_changed(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if viz_wanted(&ui) && !viz_timer.running() {
                viz_timer.restart();
            }
            if ui.get_stats_open() && !stats_dialog_timer.running() {
                // Right away, not with the last station's figures
                show_stats();
                stats_dialog_timer.restart();
            }
        });
    }

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
            let mut is_loading = s.is_resolving || s.status_text == "Connecting...";
            let mut is_playing = s.playback == PlaybackState::Playing;
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
            let eq_preset: slint::SharedString = s
                .eq_preset_name
                .as_deref()
                .unwrap_or("")
                .into();
            let recording = s.recording.clone();
            let recording_notice = s.recording_notice.clone();
            drop(s);

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

            show_recording_state(
                &ui,
                recording.as_ref(),
                recording_notice.as_ref(),
                is_playing,
                &mut poll_notice.borrow_mut(),
            );

            // Credit listening time to the favorite being played
            let playing_url = station_url.as_deref().filter(|_| is_playing);
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
                    let tmp_station = Station::new(
                        ui.get_station_name().as_str(),
                        url.as_str(),
                    );
                    if let Some((rgba, w, h)) = poll_logo_svc.get_cached_rgba(&tmp_station) {
                        let pb = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                        ui.set_current_logo(slint::Image::from_rgba8(pb));
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
                                    let tmp = Station::new(&station_name, &station_url)
                                        .with_logo(&logo);
                                    if let Some((rgba, w, h)) = logo_svc.get_rgba(&tmp) {
                                        let _ = slint::invoke_from_event_loop(move || {
                                            let Some(ui) = ui_weak2.upgrade() else { return };
                                            if is_current_station(shown, &station_url) {
                                                let pb = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                                                ui.set_current_logo(slint::Image::from_rgba8(pb));
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
        },
    );

    // Quit like a closed window on SIGTERM (systemd on stop or reboot),
    // SIGINT (Ctrl+C) and SIGHUP (the terminal closed), so the exit below
    // saves and finishes a recording
    #[cfg(unix)]
    quit_on_signals();

    // Run Slint event loop (blocks main thread). An error (the display
    // went away, e.g. at logout) ends it like closing the window does.
    if let Err(e) = ui.run() {
        eprintln!("Event loop ended: {e}");
    }

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

/// Logos cached by builds before the stable favorite ids (FNV-1a) are named
/// by the old id. Give them the current name before anything looks them up,
/// and before the startup cleanup takes them for orphans and deletes them.
/// A rename per logo, once; later starts find nothing to move.
fn migrate_logo_ids(
    favorites: &Mutex<FavoritesManager>,
    last_station: Option<&Station>,
    logo_service: &LogoService,
) {
    use radiotrope_app::data::types::{legacy_url_to_id, url_to_id};
    let urls: Vec<String> = {
        let favs = favorites.lock().unwrap_or_else(|e| e.into_inner());
        favs.sorted(FavoriteSort::Manual)
            .into_iter()
            .map(|f| f.url().to_string())
            .chain(last_station.map(|s| s.url.clone()))
            .collect()
    };
    let renamed = urls
        .iter()
        .filter(|url| {
            logo_service
                .cache()
                .rename_id(&legacy_url_to_id(url), &url_to_id(url))
        })
        .count();
    if renamed > 0 {
        eprintln!("Logo cache: renamed {renamed} logo(s) to the current favorite ids");
    }
}

/// Trim the disk caches on a thread of their own, now and then once a day:
/// API responses older than a month, browser logos not shown for 90 days,
/// and the logos of stations that are neither favorites nor the one in the
/// player. Those are kept when the favorites couldn't be read: they may
/// still be needed once the file is recovered.
fn prune_caches_daily(
    favorites: Arc<Mutex<FavoritesManager>>,
    favorites_loaded: bool,
    shared_state: Arc<Mutex<AppSnapshot>>,
    logo_service: Arc<LogoService>,
    browse_logos: Arc<BrowseLogos>,
) {
    use radiotrope_app::config::caches::{ORPHAN_LOGO_MIN_AGE, PRUNE_EVERY};
    let prune = move || {
        if let Some(cache) = radiotrope_app::network::ApiCache::open_default() {
            cache.prune(radiotrope_app::config::providers::API_CACHE_MAX_AGE);
        }
        browse_logos.remove_unused();
        if !favorites_loaded {
            return;
        }
        let mut valid: std::collections::HashSet<String> = favorites
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .all()
            .into_iter()
            .map(|f| f.id())
            .collect();
        // The station in the player keeps its logo even when it isn't a
        // favorite
        let playing = shared_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .station_url
            .clone();
        valid.extend(playing.map(|url| radiotrope_app::data::types::url_to_id(&url)));
        let removed = logo_service
            .cache()
            .cleanup_orphaned(&valid, ORPHAN_LOGO_MIN_AGE);
        if removed > 0 {
            eprintln!("Logo cache: cleaned up {removed} orphaned image(s)");
        }
    };
    let spawned = std::thread::Builder::new()
        .name("cache-prune".into())
        .spawn(move || loop {
            prune();
            std::thread::sleep(PRUNE_EVERY);
        });
    if let Err(e) = spawned {
        eprintln!("Failed to start the cache prune thread: {e}");
    }
}

/// Show the window and ask for focus (a second launch of radiotrope)
fn bring_to_front(window: &slint::Window) {
    let _ = window.show();
    #[cfg(feature = "desktop")]
    window_frame::bring_to_front(window);
}

/// Rotary encoder for volume control (KY-040 on GPIO 5/6/13)
/// Uses the kernel `rotary-encoder` driver via /dev/input/eventN for reliable
/// quadrature decoding. Push button on GPIO 13 via gpiomon.
#[cfg(feature = "embedded")]
#[allow(dead_code)] // currently disabled
fn setup_rotary_encoder(
    _ui: &App,
    cmd_tx: crossbeam_channel::Sender<app::state::AppCommand>,
    shared_state: Arc<Mutex<AppSnapshot>>,
) {
    // Push button thread (GPIO 13 via gpiomon — simple single pin, works fine)
    {
        let cmd_tx = cmd_tx.clone();
        let state = shared_state.clone();
        std::thread::Builder::new()
            .name("rotary-sw".into())
            .spawn(move || {
                use std::process::{Command, Stdio};
                let sw_pin = "13";
                loop {
                    let result = Command::new("gpiomon")
                        .args(["-e", "falling", "-n", "1", "-c", "gpiochip0", sw_pin])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();

                    if result.is_err() {
                        return;
                    }

                    let s = state.lock().unwrap_or_else(|e| e.into_inner());
                    let is_playing = s.playback == radiotrope::audio::PlaybackState::Playing;
                    let station_url = s.station_url.clone();
                    let station_name = s.station_name.clone();
                    let station_logo = s.station_logo_url.clone();
                    let station_country = s.station_country.clone();
                    drop(s);

                    if is_playing {
                        let _ = cmd_tx.send(app::state::AppCommand::Stop);
                    } else if let Some(url) = station_url {
                        let _ = cmd_tx.send(app::state::AppCommand::Play {
                            url,
                            name: station_name,
                            logo_url: station_logo,
                            country: station_country,
                            taken: None,
                        });
                    }

                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .ok();
    }

    // Rotary encoder thread — reads kernel input device
    std::thread::Builder::new()
        .name("rotary-encoder".into())
        .spawn(move || {
            use std::fs;
            use std::io::Read;

            // Velocity-sensitive volume step
            fn velocity_step(elapsed_ms: u128) -> f32 {
                if elapsed_ms < 50 {
                    0.05
                } else if elapsed_ms < 100 {
                    0.03
                } else if elapsed_ms < 200 {
                    0.02
                } else {
                    0.01
                }
            }

            // Find the rotary-encoder input device
            let find_encoder_device = || -> Option<String> {
                let input_dir = "/sys/class/input";
                for entry in fs::read_dir(input_dir).ok()? {
                    let entry = entry.ok()?;
                    let name_path = entry.path().join("device/name");
                    if let Ok(name) = fs::read_to_string(&name_path) {
                        if name.trim() == "rotary-encoder" || name.trim().contains("rotary") {
                            return Some(format!(
                                "/dev/input/{}",
                                entry.file_name().to_string_lossy()
                            ));
                        }
                    }
                }
                None
            };

            // Wait for the device to appear (overlay may load after boot)
            let dev_path;
            loop {
                if let Some(path) = find_encoder_device() {
                    dev_path = path;
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }

            eprintln!("rotary-encoder: using kernel input device {}", dev_path);

            // Open the input device
            let mut file = match fs::File::open(&dev_path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("rotary-encoder: failed to open {}: {}", dev_path, e);
                    return;
                }
            };

            // input_event struct: 16 bytes on 32-bit, 24 bytes on 64-bit
            // struct input_event { struct timeval time; __u16 type; __u16 code; __s32 value; }
            // On aarch64: timeval is 16 bytes (tv_sec: i64 + tv_usec: i64) + type: u16 + code: u16 + value: i32 = 24 bytes
            const EVENT_SIZE: usize = 24;
            let mut buf = [0u8; EVENT_SIZE];
            let mut last_event = std::time::Instant::now();

            loop {
                // Read one input event (blocks until encoder moves)
                if file.read_exact(&mut buf).is_err() {
                    break;
                }

                // Parse the event: type at offset 16, code at 18, value at 20
                let ev_type = u16::from_ne_bytes([buf[16], buf[17]]);
                let _ev_code = u16::from_ne_bytes([buf[18], buf[19]]);
                let ev_value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);

                // EV_REL = 2, REL_X = 0
                if ev_type != 2 {
                    continue;
                }

                // ev_value is +1 (clockwise) or -1 (counter-clockwise)
                let elapsed = last_event.elapsed();
                let elapsed_ms = elapsed.as_millis();
                last_event = std::time::Instant::now();

                let step = velocity_step(elapsed_ms);

                let s = shared_state.lock().unwrap_or_else(|e| e.into_inner());
                let current_vol = s.volume;
                drop(s);

                let new_vol = if ev_value > 0 {
                    (current_vol + step).min(1.0)
                } else {
                    (current_vol - step).max(0.0)
                };

                let _ = cmd_tx.send(app::state::AppCommand::SetVolume(new_vol));
            }
        })
        .ok();
}

/// Whether the visualizer has something to show: a station playing, the
/// visualizer on, and the window on screen
fn viz_wanted(ui: &App) -> bool {
    let window = ui.window();
    ui.get_is_playing() && ui.get_show_visualizer() && window.is_visible() && !window.is_minimized()
}

/// The equalizer's preset menu, from the engine's preset list: the presets
/// without a group (Flat) on their own, then a column per group, in the
/// list's order
fn show_eq_presets(ui: &App) {
    let mut top: Vec<slint::SharedString> = Vec::new();
    let mut groups: Vec<(&str, Vec<slint::SharedString>)> = Vec::new();
    for preset in radiotrope::audio::PRESETS {
        match preset.group.title() {
            None => top.push(preset.name.into()),
            Some(title) => match groups.iter_mut().find(|(t, _)| *t == title) {
                Some((_, names)) => names.push(preset.name.into()),
                None => groups.push((title, vec![preset.name.into()])),
            },
        }
    }
    let groups: Vec<EqPresetGroup> = groups
        .into_iter()
        .map(|(title, names)| EqPresetGroup {
            title: title.into(),
            names: ModelRc::new(VecModel::from(names)),
        })
        .collect();
    ui.set_eq_top_presets(ModelRc::new(VecModel::from(top)));
    ui.set_eq_preset_groups(ModelRc::new(VecModel::from(groups)));
}

/// Push one visualizer frame to the UI (levels already gated and smoothed).
/// The model is updated in place.
fn show_viz_frame(viz: &VizData, vu: &[f32], spectrum: &[f32], spectrum_model: &VecModel<f32>) {
    // Only the bands that changed: every write makes the visualizer update
    // (Wave redraws on any write), and in silence nothing changes
    set_levels(spectrum_model, spectrum);
    viz.set_vu_left(vu[0]);
    viz.set_vu_right(vu[1]);
    viz.set_has_signal(spectrum.iter().any(|&level| level > 0.01));
}

/// Models behind the Dot Matrix mode, updated in place
struct VizExtras {
    columns: std::rc::Rc<VecModel<f32>>,
    peaks: std::rc::Rc<VecModel<f32>>,
}

impl VizExtras {
    fn new(viz: &VizData) -> Self {
        let model = |n: usize| std::rc::Rc::new(VecModel::from(vec![0.0f32; n]));
        let extras = Self {
            columns: model(visual::MATRIX_COLUMNS),
            peaks: model(visual::MATRIX_COLUMNS),
        };
        viz.set_columns(ModelRc::from(extras.columns.clone()));
        viz.set_peaks(ModelRc::from(extras.peaks.clone()));
        extras
    }

    fn show_columns(&self, columns: &[f32], peaks: &[f32]) {
        set_levels(&self.columns, columns);
        set_levels(&self.peaks, peaks);
    }

    /// Clear the columns and peaks (playback stopped)
    fn clear(&self) {
        for model in [&self.columns, &self.peaks] {
            for i in 0..model.row_count() {
                model.set_row_data(i, 0.0);
            }
        }
    }
}

/// Write `levels` into `model` row by row, skipping unchanged rows
fn set_levels(model: &VecModel<f32>, levels: &[f32]) {
    for (i, &level) in levels.iter().enumerate().take(model.row_count()) {
        if model.row_data(i) != Some(level) {
            model.set_row_data(i, level);
        }
    }
}

/// The header logo's visualizer colours on each theme (dark, light) and its
/// fog, worked out once per logo: a theme switch only picks
struct HeaderLogo {
    image: slint::Image,
    colors: [Vec<slint::Color>; 2],
    fog: LogoFog,
}

thread_local! {
    static HEADER_LOGO: std::cell::RefCell<Option<HeaderLogo>> =
        const { std::cell::RefCell::new(None) };
}

/// Colour the visualizer with the main colours of the station logo, and
/// give the header logo its colour fog if it would vanish on the tile
fn apply_logo_palette(ui: &App) {
    let dark = ui.get_dark_mode();
    let image = ui.get_current_logo();
    HEADER_LOGO.with(|header| {
        let mut header = header.borrow_mut();
        if header.as_ref().is_none_or(|h| h.image != image) {
            let pixels = image.to_rgba8();
            let colors = |dark| {
                pixels
                    .as_ref()
                    .map(|buf| logo_palette(buf.as_bytes(), dark))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|[r, g, b]| slint::Color::from_rgb_u8(r, g, b))
                    .collect()
            };
            *header = Some(HeaderLogo {
                colors: [colors(true), colors(false)],
                fog: pixels
                    .as_ref()
                    .map(|buf| logo_fog(buf.as_bytes(), buf.width()))
                    .unwrap_or_default(),
                image,
            });
        }
        let Some(logo) = header.as_ref() else { return };
        let colors = logo.colors[!dark as usize].clone();
        ui.global::<VizStyle>()
            .set_logo_colors(ModelRc::from(std::rc::Rc::new(VecModel::from(colors))));
        ui.set_current_logo_fog(logo.fog.clone());
    });
}

/// The colour fog a logo gets behind it on each theme (off where it shows
/// well as it is). `rgba` is the logo's pixels in RGBA order, `width` to a
/// row.
pub(crate) fn logo_fog(rgba: &[u8], width: u32) -> LogoFog {
    let colors = |fog: Option<visual::Backdrop>| match fog {
        Some(fog) => {
            let color = |[r, g, b]: [u8; 3]| slint::Color::from_rgb_u8(r, g, b);
            FogColors {
                on: true,
                first: color(fog.first),
                second: color(fog.second),
                ground: color(fog.ground),
            }
        }
        None => FogColors::default(),
    };
    let (dark, light) = visual::logo_backdrops(rgba, width as usize);
    LogoFog {
        dark: colors(dark),
        light: colors(light),
    }
}

/// Text helpers the UI calls back into
fn setup_text_util(ui: &App) {
    ui.global::<TextUtil>()
        .on_steady_digits(|text| radiotrope_app::text::steady_digits(&text).into());
}

/// Fill in the About dialog and handle its links
fn setup_about(ui: &App) {
    let info = ui.global::<AboutInfo>();
    info.set_version(env!("CARGO_PKG_VERSION").into());
    info.set_commit(env!("RADIOTROPE_GIT_HASH").into());
    info.set_commit_date(env!("RADIOTROPE_GIT_DATE").into());
    info.set_platform(platform_name().into());
    let path_text = |p: radiotrope_app::error::Result<std::path::PathBuf>| {
        p.map(|p| p.display().to_string()).unwrap_or_default()
    };
    info.set_config_dir(path_text(radiotrope_app::data::storage::config_dir()).into());
    info.set_cache_dir(path_text(radiotrope_app::data::cache::cache_dir()).into());
    info.set_repository(env!("CARGO_PKG_REPOSITORY").into());
    info.set_license(license_label(env!("CARGO_PKG_LICENSE")).into());
    info.set_authors(author_names(env!("CARGO_PKG_AUTHORS")).into());

    ui.on_open_url(|url| open_url(&url));
}

/// Human-readable OS and CPU, e.g. "Linux aarch64"
fn platform_name() -> String {
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        other => other,
    };
    format!("{os} {}", std::env::consts::ARCH)
}

/// Short display name for an SPDX license id, e.g. "GPLv3+" for
/// GPL-3.0-or-later; other ids are shown as they are
fn license_label(spdx: &str) -> &str {
    match spdx {
        "GPL-3.0-or-later" => "GPLv3+",
        "GPL-3.0-only" => "GPLv3",
        other => other,
    }
}

/// Cargo's `authors` list without the email addresses
fn author_names(authors: &str) -> String {
    authors
        .split(':')
        .map(|a| a.split('<').next().unwrap_or(a).trim())
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Open a web link in the default browser. Does nothing if there is no
/// browser (e.g. the embedded kiosk build).
fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    match std::process::Command::new(program).arg(url).spawn() {
        // Reap the launcher so it doesn't linger as a zombie
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("Failed to open {url}: {e}"),
    }
}

/// Set up WiFi settings UI callbacks
#[cfg(feature = "embedded")]
fn setup_wifi(ui: &App) {
    // Backspace handler for virtual keyboard (Slint has no string substring)
    ui.on_wifi_backspace(|text| {
        let s = text.to_string();
        let mut chars: Vec<char> = s.chars().collect();
        chars.pop();
        let result: String = chars.into_iter().collect();
        result.into()
    });

    let wifi_mgr = Arc::new(radiotrope_app::wifi::WifiManager::new().ok());

    // Scan callback
    {
        let mgr = wifi_mgr.clone();
        let ui_weak = ui.as_weak();
        ui.on_wifi_scan_requested(move || {
            let mgr = mgr.clone();
            let ui_weak = ui_weak.clone();
            std::thread::Builder::new()
                .name("wifi-scan".into())
                .spawn(move || {
                    if let Some(ref mgr) = *mgr {
                        let ui_weak2 = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak2.upgrade() {
                                ui.set_wifi_scanning(true);
                            }
                        });

                        let networks = mgr.scan().unwrap_or_default();
                        let ui_weak2 = ui_weak.clone();
                        let entries: Vec<_> = networks
                            .iter()
                            .map(|n| WifiNetworkEntry {
                                ssid: n.ssid.clone().into(),
                                signal_percent: n.signal_percent() as i32,
                                security: n.security.to_string().into(),
                                connected: n.connected,
                                object_path: n.object_path.clone().into(),
                            })
                            .collect();

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak2.upgrade() {
                                let model = std::rc::Rc::new(slint::VecModel::from(entries));
                                ui.set_wifi_networks(slint::ModelRc::from(model));
                                ui.set_wifi_scanning(false);
                            }
                        });
                    }
                })
                .ok();
        });
    }

    // Connect callback
    {
        let mgr = wifi_mgr.clone();
        let ui_weak = ui.as_weak();
        ui.on_wifi_connect_requested(move |path, password| {
            let mgr = mgr.clone();
            let ui_weak = ui_weak.clone();
            let path = path.to_string();
            let password = password.to_string();
            std::thread::Builder::new()
                .name("wifi-connect".into())
                .spawn(move || {
                    if let Some(ref mgr) = *mgr {
                        let pass = if password.is_empty() {
                            None
                        } else {
                            Some(password.as_str())
                        };

                        // Show connecting status immediately
                        let ui_weak2 = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak2.upgrade() {
                                ui.set_wifi_connection_status("Connecting...".into());
                            }
                        });

                        match mgr.connect(&path, pass) {
                            Ok(()) => {
                                // Wait for connection to establish
                                std::thread::sleep(Duration::from_secs(3));
                                let ssid = mgr.current_ssid().unwrap_or_default();

                                // Rescan to refresh the list with connected state
                                let networks = mgr.scan().unwrap_or_default();
                                let entries: Vec<_> = networks
                                    .iter()
                                    .map(|n| WifiNetworkEntry {
                                        ssid: n.ssid.clone().into(),
                                        signal_percent: n.signal_percent() as i32,
                                        security: n.security.to_string().into(),
                                        connected: n.connected,
                                        object_path: n.object_path.clone().into(),
                                    })
                                    .collect();

                                let ui_weak2 = ui_weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak2.upgrade() {
                                        if ssid.is_empty() {
                                            ui.set_wifi_connection_status("Connected".into());
                                        } else {
                                            ui.set_wifi_connection_status(
                                                format!("Connected to {}", ssid).into(),
                                            );
                                        }
                                        ui.set_wifi_current_ssid(ssid.into());
                                        let model =
                                            std::rc::Rc::new(slint::VecModel::from(entries));
                                        ui.set_wifi_networks(slint::ModelRc::from(model));
                                    }
                                });

                                // Clear status after 5 seconds
                                std::thread::sleep(Duration::from_secs(5));
                                let ui_weak2 = ui_weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak2.upgrade() {
                                        ui.set_wifi_connection_status("".into());
                                    }
                                });
                            }
                            Err(e) => {
                                let ui_weak2 = ui_weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak2.upgrade() {
                                        ui.set_wifi_connection_status(
                                            format!("Failed: {}", e).into(),
                                        );
                                    }
                                });

                                // Clear error after 5 seconds
                                std::thread::sleep(Duration::from_secs(5));
                                let ui_weak2 = ui_weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak2.upgrade() {
                                        ui.set_wifi_connection_status("".into());
                                    }
                                });
                            }
                        }
                    }
                })
                .ok();
        });
    }

    // Disconnect callback
    {
        let mgr = wifi_mgr.clone();
        let ui_weak = ui.as_weak();
        ui.on_wifi_disconnect_requested(move || {
            let mgr = mgr.clone();
            let ui_weak = ui_weak.clone();
            std::thread::Builder::new()
                .name("wifi-disconnect".into())
                .spawn(move || {
                    if let Some(ref mgr) = *mgr {
                        let _ = mgr.disconnect();

                        // Show disconnected status
                        let ui_weak2 = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak2.upgrade() {
                                ui.set_wifi_connection_status("Disconnected".into());
                                ui.set_wifi_current_ssid("".into());
                            }
                        });

                        // Wait then rescan to refresh the list
                        std::thread::sleep(Duration::from_secs(2));
                        let networks = mgr.scan().unwrap_or_default();
                        let entries: Vec<_> = networks
                            .iter()
                            .map(|n| WifiNetworkEntry {
                                ssid: n.ssid.clone().into(),
                                signal_percent: n.signal_percent() as i32,
                                security: n.security.to_string().into(),
                                connected: n.connected,
                                object_path: n.object_path.clone().into(),
                            })
                            .collect();

                        let ui_weak2 = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak2.upgrade() {
                                let model = std::rc::Rc::new(slint::VecModel::from(entries));
                                ui.set_wifi_networks(slint::ModelRc::from(model));
                            }
                        });

                        // Clear status after 5 seconds
                        std::thread::sleep(Duration::from_secs(5));
                        let ui_weak2 = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak2.upgrade() {
                                ui.set_wifi_connection_status("".into());
                            }
                        });
                    }
                })
                .ok();
        });
    }
}

/// Largest station logo embedded as cover art in a recording
const MAX_COVER_BYTES: usize = 512 * 1024;

/// Wire the Agents (MCP) dialog, and start network agents when they are on.
/// `agents` is `None` in a `--mcp --standalone` player, which serves one
/// agent on stdio only: the dialog shows the settings greyed out. Keep the
/// returned timer: it starts the network server again when the network
/// changes.
fn setup_agents(
    ui: &App,
    agents: Option<Arc<mcp::agents::Agents>>,
    settings: &radiotrope_app::data::settings::Settings,
) -> slint::Timer {
    use mcp::setup::AgentApp;
    use radiotrope_app::data::settings::McpAuth;
    ui.set_agents_available(agents.is_some());
    ui.set_agents_network(settings.mcp_network);
    let listen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    show_listen_choices(ui, settings, &listen);
    let auths: Vec<slint::SharedString> = McpAuth::ALL.iter().map(|a| a.label().into()).collect();
    ui.set_agents_auths(std::rc::Rc::new(slint::VecModel::from(auths)).into());
    let auth = McpAuth::ALL
        .iter()
        .position(|a| *a == settings.mcp_auth)
        .unwrap_or(0);
    ui.set_agents_auth(auth as i32);

    // Setup lines for the agent picked in the dialog
    let labels: Vec<slint::SharedString> =
        AgentApp::ALL.iter().map(|app| app.label().into()).collect();
    ui.set_agents_apps(std::rc::Rc::new(slint::VecModel::from(labels)).into());
    let picked = AgentApp::from_id(settings.mcp_client.as_deref());
    let index = AgentApp::ALL
        .iter()
        .position(|app| *app == picked)
        .unwrap_or(0);
    ui.set_agents_app(index as i32);
    ui.on_agents_app_changed({
        let ui_weak = ui.as_weak();
        move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(app) = AgentApp::ALL.get(index as usize) else {
                return;
            };
            save_agent_settings(|s| s.mcp_client = Some(app.id().to_string()));
            show_agent_lines(&ui);
        }
    });
    show_agent_lines(ui);

    let timer = slint::Timer::default();
    let Some(agents) = agents else { return timer };
    agents.apply_network_later(settings, show_agents_network_later(ui));

    // Every change saves and restarts the network server with it
    let apply = {
        let ui_weak = ui.as_weak();
        let agents = agents.clone();
        let listen_for_apply = listen.clone();
        move |change: &dyn Fn(&mut radiotrope_app::data::settings::Settings)| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
            change(&mut settings);
            if let Err(e) = settings.save() {
                eprintln!("Failed to save agent settings: {e}");
            }
            show_listen_choices(&ui, &settings, &listen_for_apply);
            agents.apply_network_later(&settings, show_agents_network_later(&ui));
        }
    };
    let apply = std::rc::Rc::new(apply);

    ui.on_agents_network_toggled({
        let apply = apply.clone();
        move |on| apply(&|s| s.mcp_network = on)
    });
    ui.on_agents_auth_changed({
        let apply = apply.clone();
        move |index| {
            let Some(auth) = McpAuth::ALL.get(index as usize).copied() else {
                return;
            };
            apply(&|s| s.mcp_auth = auth)
        }
    });
    ui.on_agents_listen_changed({
        let apply = apply.clone();
        let listen = listen.clone();
        move |index| {
            let Some(choice) = listen.borrow().get(index as usize).cloned() else {
                return;
            };
            apply(&|s| {
                s.mcp_address = std::net::SocketAddr::new(choice.ip, saved_port(s)).to_string();
                s.mcp_interface = choice.interface.clone();
            })
        }
    });
    ui.on_agents_apply_port({
        let apply = apply.clone();
        let ui_weak = ui.as_weak();
        move |text| {
            let text = text.trim();
            let port = if text.is_empty() {
                Some(default_port())
            } else {
                text.parse::<u16>().ok().filter(|p| *p != 0)
            };
            let Some(port) = port else {
                // Not a port: show the saved one again
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_agents_edit_port(ui.get_agents_port());
                }
                return;
            };
            apply(&|s| {
                let ip = mcp::network::split_address(&s.mcp_address)
                    .map(|a| a.ip())
                    .unwrap_or(std::net::Ipv4Addr::LOCALHOST.into());
                s.mcp_address = std::net::SocketAddr::new(ip, port).to_string();
            })
        }
    });
    ui.on_agents_refresh_listen({
        let ui_weak = ui.as_weak();
        let listen = listen.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
            show_listen_choices(&ui, &settings, &listen);
        }
    });
    ui.on_agents_regenerate_token({
        let apply = apply.clone();
        move || {
            if let Err(e) = radiotrope_app::data::agent_token::regenerate() {
                eprintln!("Failed to make a new token: {e}");
            }
            apply(&|_| {})
        }
    });

    // A picked interface gets a new address with a new lease, and at login
    // the network may come up after the player: listen again then
    let ui_weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        radiotrope_app::config::mcp::NETWORK_RECHECK,
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if let Some(settings) = agents.network_to_recheck() {
                agents.apply_network_later(&settings, show_agents_network_later(&ui));
            }
        },
    );
    timer
}

/// Keep the menu bar's agents chip up to date: who uses the player now
fn watch_agents(ui: &App, presence: Option<mcp::presence::Presence>) -> slint::Timer {
    let timer = slint::Timer::default();
    let Some(presence) = presence else {
        return timer;
    };
    let ui_weak = ui.as_weak();
    // Compared as shown, so "4 min ago" redraws once a minute, not every tick
    let mut shown: Option<Vec<AgentRow>> = None;
    timer.start(
        slint::TimerMode::Repeated,
        radiotrope_app::config::ui::AGENTS_REFRESH,
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let rows: Vec<AgentRow> = presence
                .agents()
                .iter()
                .map(|a| AgentRow {
                    name: a.name().into(),
                    place: a.place_label().into(),
                    network: a.is_network(),
                    seen: a.seen_label().into(),
                    connected: a.is_connected(),
                })
                .collect();
            if shown.as_ref() == Some(&rows) {
                return;
            }
            ui.set_agent_count(rows.len() as i32);
            ui.set_agent_list(std::rc::Rc::new(slint::VecModel::from(rows.clone())).into());
            shown = Some(rows);
        },
    );
    timer
}

/// Show the network status once the server is set up, on its own thread
fn show_agents_network_later(ui: &App) -> mcp::agents::ShowStatus {
    let ui_weak = ui.as_weak();
    Box::new(move |status| {
        let _ = ui_weak.upgrade_in_event_loop(move |ui| show_agents_network(&ui, &status));
    })
}

fn show_agents_network(ui: &App, status: &mcp::agents::NetworkStatus) {
    ui.set_agents_status(status.text.as_str().into());
    ui.set_agents_status_error(status.is_error);
    ui.set_agents_token(status.token.as_str().into());
    ui.set_agents_url(status.url.as_str().into());
    show_agent_lines(ui);
}

/// The lines that add Radiotrope to the agent picked in the dialog
fn show_agent_lines(ui: &App) {
    use mcp::setup::{self, AgentApp};
    use radiotrope_app::data::settings::McpAuth;
    let app = AgentApp::ALL
        .get(ui.get_agents_app().max(0) as usize)
        .copied()
        .unwrap_or(AgentApp::ClaudeCode);
    let url = ui.get_agents_url();
    let token = ui.get_agents_token();
    let wants_token =
        McpAuth::ALL.get(ui.get_agents_auth().max(0) as usize) == Some(&McpAuth::Token);
    ui.set_agents_local_command(setup::local_line(app, &setup::this_program()).into());
    let network = (!url.is_empty())
        .then(|| setup::network_line(app, &url, wants_token.then_some(token.as_str())))
        .flatten();
    ui.set_agents_command(network.unwrap_or_default().into());
    ui.set_agents_network_placeholder(setup::no_network_line(app).into());
    ui.set_agents_local_note(app.local_note().into());
    ui.set_agents_network_note(app.network_note(wants_token).into());
}

/// One entry of the Agents dialog's "Listen on" list
#[derive(Clone)]
struct ListenChoice {
    ip: std::net::IpAddr,
    /// The interface it belongs to, saved so its next address is used too
    interface: Option<String>,
}

/// Fill the "Listen on" list: this computer only, each network interface,
/// then all networks; and pick the saved one (added if it's not there)
fn show_listen_choices(
    ui: &App,
    settings: &radiotrope_app::data::settings::Settings,
    listen: &std::cell::RefCell<Vec<ListenChoice>>,
) {
    use std::net::{IpAddr, Ipv4Addr};
    let mut labels: Vec<slint::SharedString> = vec!["This computer only".into()];
    let mut choices = vec![ListenChoice {
        ip: Ipv4Addr::LOCALHOST.into(),
        interface: None,
    }];
    // Networks for containers and virtual machines stay out, unless picked
    let interfaces = mcp::network::interfaces()
        .into_iter()
        .filter(|i| i.usable || settings.mcp_interface.as_deref() == Some(i.name.as_str()));
    for i in interfaces {
        labels.push(format!("{} ({})", i.name, i.ip).into());
        choices.push(ListenChoice {
            ip: i.ip,
            interface: Some(i.name),
        });
    }
    labels.push("All networks".into());
    choices.push(ListenChoice {
        ip: Ipv4Addr::UNSPECIFIED.into(),
        interface: None,
    });

    let address = mcp::agents::listen_address(settings);
    let saved: Option<IpAddr> = mcp::network::split_address(&address).map(|a| a.ip());
    let picked = choices
        .iter()
        .position(|c| settings.mcp_interface.is_some() && c.interface == settings.mcp_interface);
    let picked = picked.or_else(|| choices.iter().position(|c| Some(c.ip) == saved));
    let picked = picked.unwrap_or_else(|| {
        // An address no interface has now, e.g. from an unplugged network
        let ip = saved
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| address.clone());
        labels.push(format!("{ip} (not found)").into());
        choices.push(ListenChoice {
            ip: saved.unwrap_or(Ipv4Addr::LOCALHOST.into()),
            interface: settings.mcp_interface.clone(),
        });
        choices.len() - 1
    });

    ui.set_agents_listen_options(std::rc::Rc::new(slint::VecModel::from(labels)).into());
    ui.set_agents_listen(picked as i32);
    ui.set_agents_listen_local(choices[picked].ip.is_loopback());
    let port: slint::SharedString = saved_port(settings).to_string().into();
    ui.set_agents_port(port.clone());
    ui.set_agents_edit_port(port);
    *listen.borrow_mut() = choices;
}

/// The port of the saved network address
fn saved_port(settings: &radiotrope_app::data::settings::Settings) -> u16 {
    mcp::network::split_address(&settings.mcp_address)
        .map(|a| a.port())
        .unwrap_or_else(default_port)
}

fn default_port() -> u16 {
    radiotrope_app::config::mcp::DEFAULT_ADDRESS
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8765)
}

fn save_agent_settings(change: impl FnOnce(&mut radiotrope_app::data::settings::Settings)) {
    let mut settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
    change(&mut settings);
    if let Err(e) = settings.save() {
        eprintln!("Failed to save agent settings: {e}");
    }
}

/// Wire the Tools menu recording items and the Recording Settings
/// dialog. The format, folder and before/after-EQ choice are saved right away.
fn setup_recording(
    ui: &App,
    settings: &radiotrope_app::data::settings::Settings,
    cmd_tx: UiSender,
    shared_state: Arc<Mutex<AppSnapshot>>,
    logo_service: Arc<LogoService>,
) {
    show_recording_folder(ui, settings.recording_dir.as_deref());
    ui.set_record_with_eq(settings.record_with_eq);
    ui.set_recording_format(settings.recording_format.id().into());
    ui.set_recording_bitrate(settings.recording_bitrate.unwrap_or(0) as i32);

    ui.on_toggle_recording({
        let ui_weak = ui.as_weak();
        let shared_state = shared_state.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (recording, setup) = {
                let s = shared_state.lock().unwrap_or_else(|e| e.into_inner());
                (s.recording.is_some(), s.recording_setup.clone())
            };
            if recording {
                cmd_tx.send(app::state::AppCommand::StopRecording);
                return;
            }
            let station = Station::new(
                ui.get_station_name().as_str(),
                ui.get_station_url().as_str(),
            );
            cmd_tx.send(app::state::AppCommand::StartRecording {
                folder: recordings::folder(setup.dir.as_deref()),
                format: setup.format,
                bitrate: setup.bitrate,
                // The switch keeps its state but only counts with the EQ on
                with_eq: setup.with_eq && ui.get_eq_enabled(),
                cover: station_cover_png(&logo_service, &station),
            });
        }
    });

    ui.on_apply_recording_folder({
        let ui_weak = ui.as_weak();
        let state = shared_state.clone();
        move |path| {
            let Some(ui) = ui_weak.upgrade() else { return };
            apply_recording_folder(&ui, &state, &path);
        }
    });

    ui.on_restore_recording_folder({
        let ui_weak = ui.as_weak();
        let state = shared_state.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            // A folder still being checked isn't saved over it
            new_folder_check();
            save_recording_settings(&state, |s| s.recording_dir = None);
            show_recording_folder(&ui, None);
        }
    });

    ui.on_record_with_eq_toggled({
        let state = shared_state.clone();
        move |on| save_recording_settings(&state, |s| s.record_with_eq = on)
    });

    ui.on_recording_format_changed({
        let state = shared_state.clone();
        move |id| {
            let format = radiotrope_app::data::settings::RecordingFormat::from_id(id.as_str());
            save_recording_settings(&state, |s| s.recording_format = format);
        }
    });

    // 0 is Auto
    ui.on_recording_bitrate_changed({
        let state = shared_state.clone();
        move |kbps| {
            save_recording_settings(&state, |s| {
                s.recording_bitrate = (kbps > 0).then_some(kbps as u32)
            });
        }
    });

    ui.on_open_recordings_folder({
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let dir = std::path::PathBuf::from(ui.get_recording_folder().as_str());
            let ui_weak = ui_weak.clone();
            // Opening a folder that doesn't exist yet would show an error.
            // Making it can wait on a slow or sleeping drive, so it isn't
            // done on the UI thread.
            let spawned = std::thread::Builder::new()
                .name("open-recordings".into())
                .spawn(move || match recordings::prepare_dir(&dir) {
                    Ok(()) => open_folder(&dir),
                    Err(e) => {
                        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.set_recording_folder_error(e.into())
                        });
                    }
                });
            if let Err(e) = spawned {
                ui.set_recording_folder_error(format!("Cannot open the folder: {e}").into());
            }
        }
    });

    ui.on_browse_recording_folder({
        let ui_weak = ui.as_weak();
        move || browse_recording_folder(ui_weak.clone(), shared_state.clone())
    });
}

/// Save a folder typed or picked in the Recording Settings dialog, or show
/// why it can't be used (the folder in use stays as it was). The folder is
/// made and checked on a thread of its own, as a slow or sleeping drive
/// can take a while, then saved back on the UI thread unless another
/// folder was asked for meanwhile.
fn apply_recording_folder(ui: &App, state: &Arc<Mutex<AppSnapshot>>, path: &str) {
    let path = std::path::PathBuf::from(path.trim());
    if path.as_os_str().is_empty() {
        ui.set_recording_folder_error("Enter a folder, or use Restore Default.".into());
        return;
    }
    if !path.is_absolute() {
        ui.set_recording_folder_error("Enter a full path to a folder.".into());
        return;
    }
    let check = new_folder_check();
    let ui_weak = ui.as_weak();
    let state = state.clone();
    let spawned = std::thread::Builder::new()
        .name("recording-folder".into())
        .spawn(move || {
            let checked = recordings::prepare_dir(&path);
            // Choosing the default folder by hand keeps following the default
            let custom = (path != recordings::default_dir()).then_some(path);
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                if FOLDER_CHECK.get() != check {
                    return;
                }
                if let Err(e) = checked {
                    ui.set_recording_folder_error(e.into());
                    return;
                }
                save_recording_settings(&state, |s| s.recording_dir = custom.clone());
                show_recording_folder(&ui, custom.as_deref());
            });
        });
    if let Err(e) = spawned {
        ui.set_recording_folder_error(format!("Cannot check the folder: {e}").into());
    }
}

thread_local! {
    /// Counts the recording folders asked for, so only the last one asked
    /// for is saved when its check comes back
    static FOLDER_CHECK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A new recording folder was asked for (or the default restored): a
/// check still running for an earlier one is ignored when it ends
fn new_folder_check() -> u64 {
    FOLDER_CHECK.set(FOLDER_CHECK.get() + 1);
    FOLDER_CHECK.get()
}

/// Show `custom` (or the default folder) in the Recording Settings dialog.
fn show_recording_folder(ui: &App, custom: Option<&std::path::Path>) {
    let folder = recordings::folder(custom);
    let text: slint::SharedString = folder.display().to_string().into();
    ui.set_recording_folder(text.clone());
    ui.set_recording_folder_edit(text);
    ui.set_recording_default_folder(recordings::default_dir().display().to_string().into());
    ui.set_recording_folder_error(Default::default());
}

/// Change recording settings on disk right away, and in the shared state,
/// where an agent's recording takes them from.
fn save_recording_settings(
    state: &Mutex<AppSnapshot>,
    change: impl FnOnce(&mut radiotrope_app::data::settings::Settings),
) {
    let mut settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
    change(&mut settings);
    state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .recording_setup = app::state::RecordingSetup::from_settings(&settings);
    if let Err(e) = settings.save() {
        eprintln!("Failed to save recording settings: {e}");
    }
}

/// The station's cached logo as PNG, for a recording's cover art.
fn station_cover_png(logo_service: &LogoService, station: &Station) -> Option<Vec<u8>> {
    let (rgba, w, h) = logo_service.get_cached_rgba(station)?;
    let img = image::RgbaImage::from_raw(w, h, rgba)?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    (png.len() <= MAX_COVER_BYTES).then_some(png)
}

/// Mirror recording progress and notices into the UI (called every 200 ms).
fn show_recording_state(
    ui: &App,
    recording: Option<&app::state::RecordingProgress>,
    notice: Option<&app::state::RecordingNotice>,
    is_playing: bool,
    shown: &mut (u64, Instant),
) {
    ui.set_is_recording(recording.is_some());
    ui.set_can_record(is_playing);
    if let Some(rec) = recording {
        let secs = rec.duration.as_secs();
        let time = if secs >= 3600 {
            format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
        } else {
            format!("{}:{:02}", secs / 60, secs % 60)
        };
        ui.set_recording_text(time.into());
        ui.set_recording_size(format_bytes(rec.bytes).into());
    }

    match notice {
        Some(n) if n.seq != shown.0 => {
            *shown = (n.seq, Instant::now());
            ui.set_recording_notice(n.text.as_str().into());
            ui.set_recording_notice_error(n.is_error);
        }
        _ => {
            if !ui.get_recording_notice().is_empty() && shown.1.elapsed() >= RECORDING_NOTICE_TIME {
                ui.set_recording_notice(Default::default());
            }
        }
    }
}

/// Let the user pick the recording folder with the system's folder dialog.
/// The choice is saved straight away.
#[cfg(feature = "desktop")]
fn browse_recording_folder(ui_weak: slint::Weak<App>, state: Arc<Mutex<AppSnapshot>>) {
    let Some(ui) = ui_weak.upgrade() else { return };
    let start = std::path::PathBuf::from(ui.get_recording_folder_edit().as_str());
    // Start in the folder being edited, or the nearest parent that exists.
    // Looking can wake a sleeping drive, so it is done off the UI thread,
    // and the dialog opened back on it.
    let spawned = std::thread::Builder::new()
        .name("recording-folder".into())
        .spawn(move || {
            let dir = start
                .ancestors()
                .find(|p| p.is_dir())
                .map(std::path::Path::to_path_buf);
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let mut dialog = rfd::AsyncFileDialog::new().set_title("Choose Recording Folder");
                if let Some(dir) = dir {
                    dialog = dialog.set_directory(dir);
                }
                dialog = dialog.set_parent(&ui.window().window_handle());
                let pick = dialog.pick_folder();
                let ui_weak = ui.as_weak();
                let spawned = slint::spawn_local(async move {
                    let picked = pick.await;
                    let Some(ui) = ui_weak.upgrade() else { return };
                    // A picked folder saves straight away, like the other settings
                    if let Some(folder) = picked {
                        apply_recording_folder(&ui, &state, &folder.path().display().to_string());
                    }
                });
                if let Err(e) = spawned {
                    eprintln!("Failed to open the folder dialog: {e}");
                }
            });
        });
    if let Err(e) = spawned {
        eprintln!("Failed to open the folder dialog: {e}");
    }
}

/// The kiosk build has no folder dialog; the path is typed instead.
#[cfg(not(feature = "desktop"))]
fn browse_recording_folder(_ui_weak: slint::Weak<App>, _state: Arc<Mutex<AppSnapshot>>) {}

/// Open a folder in the system file manager.
fn open_folder(dir: &std::path::Path) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    command.arg(explorer_path(dir));
    #[cfg(not(windows))]
    command.arg(dir);
    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("Failed to open {}: {e}", dir.display()),
    }
}

/// `dir` as Explorer takes it: given forward slashes ("C:/Users/..."), as a
/// typed or saved folder can have, it opens Documents instead
#[cfg(any(windows, test))]
fn explorer_path(dir: &std::path::Path) -> std::ffi::OsString {
    match dir.to_str() {
        Some(text) => text.replace('/', "\\").into(),
        // Not Unicode, so not typed in: left as it is
        None => dir.as_os_str().to_owned(),
    }
}

/// Persist current app state to settings.json
fn save_settings(shared_state: &Arc<Mutex<AppSnapshot>>, ui: &App) {
    // A copy: the controller and agents aren't kept waiting on the state
    // while the file is read and written
    let s = shared_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
    settings.volume = s.volume;
    settings.muted = s.is_muted;
    settings.eq_gains = s.eq_gains;
    settings.eq_preamp = s.eq_preamp;
    settings.eq_enabled = s.eq_enabled;
    settings.eq_preset_name = s.eq_preset_name.clone();
    settings.accent_color = s.accent_color.clone();

    settings.theme = if ui.get_dark_mode() {
        radiotrope_app::data::settings::Theme::Dark
    } else {
        radiotrope_app::data::settings::Theme::Light
    };

    settings.viz_mode = ui.get_viz_mode().to_string();
    settings.viz_palette = ui.global::<VizStyle>().get_palette().to_string();
    settings.show_station_stats = ui.get_show_station_stats();
    settings.show_visualizer = ui.get_show_visualizer();
    settings.panel_gradient = ui.get_panel_gradient();
    if cfg!(not(feature = "embedded")) {
        settings.custom_title_bar = ui.get_custom_frame();
        settings.always_on_top = ui.get_keep_on_top();
    }

    if let Some(ref url) = s.station_url {
        if !url.is_empty() {
            let name = s
                .station_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| radiotrope_app::data::types::name_from_url(url));
            let mut station = Station::new(&name, url);
            let logo_url = ui.get_station_logo_url().to_string();
            if !logo_url.is_empty() {
                station = station.with_logo(&logo_url);
            }
            station.country = Some(ui.get_station_country().to_string()).filter(|c| !c.is_empty());
            settings.last_station = Some(station);
        }
    }

    // Saved in logical pixels, the unit the size is restored in at startup.
    // The window reports physical pixels, so saving those made the window
    // grow by the display scale (e.g. 125%) on every launch. A maximized
    // window keeps the size it had before it was maximized.
    let window = ui.window();
    if !window.is_maximized() {
        let size = window.size().to_logical(window.scale_factor());
        if size.width >= 1.0 && size.height >= 1.0 {
            settings.window_width = Some(size.width.round() as u32);
            settings.window_height = Some(size.height.round() as u32);
        }
    }

    let _ = settings.save();
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
        let favs = favorites.lock().unwrap_or_else(|e| e.into_inner());
        let play = favs.resolve_play(req);
        let is_favorite = favs.is_favorite(&play.url);
        (play, is_favorite)
    };

    // A second click on the same station right after the first is ignored:
    // a touchpad can turn one tap into two, which restarted the stream.
    // The first click acts at once, and another station always switches.
    {
        let now = Instant::now();
        let mut last = LAST_PLAY_CLICK.lock().unwrap_or_else(|e| e.into_inner());
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
        let pb = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
        ui.set_current_logo(slint::Image::from_rgba8(pb));
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
    let play_seq = shared_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .play_seq;
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
        let mut s = shared_state.lock().unwrap_or_else(|e| e.into_inner());
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
                                let pixel_buf =
                                    SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        &rgba, width, height,
                                    );
                                let image = slint::Image::from_rgba8(pixel_buf);
                                ui.set_current_logo(image);
                            });
                        }
                    })
                    .ok();
            }
        }
    }
}

fn update_stats_ui(ui: &App, s: &StreamStats) {
    // Stream section
    ui.set_stat_health(format_health(&s.health_state).into());
    ui.set_stat_playtime(format_playtime(s.play_started_at).into());

    // Codec section
    let (codec, bitrate, sample_rate, channels) = if let Some(ref ci) = s.codec_info {
        let codec_str = match s.stream_type {
            Some(StreamType::Hls) => format!("{} (HLS)", ci.codec_name),
            Some(StreamType::Direct) => format!("{} (ICY)", ci.codec_name),
            None => ci.codec_name.clone(),
        };
        let br = ci
            .bitrate
            .map(|b| format!("{b} kbps"))
            .unwrap_or_else(|| "--".into());
        let sr = if ci.sample_rate > 0 {
            format!("{} Hz", ci.sample_rate)
        } else {
            "--".into()
        };
        let ch = match ci.channels {
            0 => "--".into(),
            1 => "Mono".into(),
            2 => "Stereo".into(),
            n => format!("{n} ch"),
        };
        (codec_str, br, sr, ch)
    } else {
        ("--".into(), "--".into(), "--".into(), "--".into())
    };
    ui.set_stat_codec(codec.into());
    ui.set_stat_bitrate(bitrate.into());
    ui.set_stat_sample_rate(sample_rate.into());
    ui.set_stat_channels(channels.into());

    // Network section
    ui.set_stat_received(format_bytes(s.bytes_received).into());
    let segments_str = if s.segments_downloaded > 0 {
        format_number(s.segments_downloaded)
    } else {
        "--".into()
    };
    ui.set_stat_segments(segments_str.into());
    ui.set_stat_throughput(format!("{:.0} kbps", s.throughput_kbps).into());

    // Buffer section
    ui.set_stat_buffer_level(format_bytes(s.buffer_level_bytes as u64).into());
    ui.set_stat_buffer_capacity(format_bytes(s.buffer_capacity_bytes as u64).into());
    // Decode section
    ui.set_stat_frames_played(format_number(s.frames_played).into());
    ui.set_stat_decode_errors(format_number(s.decode_errors).into());
    ui.set_stat_underruns(format_number(s.underrun_count as u64).into());
}

/// Statistics dialog with nothing playing
fn clear_stats_ui(ui: &App) {
    ui.set_stat_health("Stopped".into());
    for set in [
        App::set_stat_playtime,
        App::set_stat_codec,
        App::set_stat_bitrate,
        App::set_stat_sample_rate,
        App::set_stat_channels,
        App::set_stat_received,
        App::set_stat_segments,
        App::set_stat_throughput,
        App::set_stat_buffer_level,
        App::set_stat_buffer_capacity,
        App::set_stat_frames_played,
        App::set_stat_decode_errors,
        App::set_stat_underruns,
    ] {
        set(ui, "--".into());
    }
}

/// Stream status shown on the Statistics dialog's first tile
fn format_health(state: &HealthState) -> String {
    match state {
        HealthState::WaitingForAudio => "Connecting".into(),
        HealthState::Healthy => "OK".into(),
        HealthState::Stalled => "Stalled".into(),
        HealthState::Failed(reason) => format!("Failed ({reason:?})"),
    }
}

fn format_playtime(started: Option<Instant>) -> String {
    let Some(t) = started else {
        return "--".into();
    };
    let secs = t.elapsed().as_secs();
    let m = secs / 60;
    let s = secs % 60;
    if m >= 60 {
        let h = m / 60;
        format!("{h:02}:{:02}:{s:02}", m % 60)
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn format_number(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// What the station browser is listing
#[derive(Clone)]
enum BrowseQuery {
    /// Most played stations
    Top,
    /// Stations whose name matches
    Search(String),
    /// Stations in a category, optionally filtered by name
    Category { category: Category, query: String },
}

/// Fetch one page of browser results from radio-browser
fn fetch_browse_page(
    query: &BrowseQuery,
    offset: usize,
) -> radiotrope_app::error::Result<SearchResults> {
    let registry = ProviderRegistry::with_defaults()?;
    let radio_browser = || {
        registry.get("radio-browser").ok_or_else(|| {
            radiotrope_app::error::AppError::NotFound("radio-browser provider not found".into())
        })
    };
    let first_page = |stations: Vec<Station>| SearchResults {
        total: None,
        has_more: stations.len() >= SEARCH_PAGE_SIZE,
        stations,
    };
    match query {
        BrowseQuery::Top if offset == 0 => {
            Ok(first_page(radio_browser()?.get_popular(SEARCH_PAGE_SIZE)?))
        }
        // Later pages of the top list: every station, most clicked first
        BrowseQuery::Top => radio_browser()?.search("", SEARCH_PAGE_SIZE, offset),
        BrowseQuery::Search(q) if offset == 0 => {
            Ok(first_page(registry.search_all(q, SEARCH_PAGE_SIZE)?))
        }
        BrowseQuery::Search(q) => radio_browser()?.search(q, SEARCH_PAGE_SIZE, offset),
        BrowseQuery::Category { category, query } => {
            radio_browser()?.search_category(category, query, SEARCH_PAGE_SIZE, offset)
        }
    }
}

/// A station browser list put aside while the other mode is shown
#[derive(Clone)]
struct BrowseStash {
    // Country name (country mode)
    country: String,
    query: BrowseQuery,
    offset: usize,
    results: ModelRc<BrowseStation>,
    logos: ModelRc<slint::Image>,
    fogs: ModelRc<LogoFog>,
    has_more: bool,
    typed: slint::SharedString,
    shown: slint::SharedString,
    scroll: f32,
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
    *state.lock().unwrap_or_else(|e| e.into_inner()) = (query.clone(), 0);
    let my_gen = gen.fetch_add(1, Ordering::Relaxed) + 1;
    // Clear old results immediately
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_search_results(ModelRc::default());
        ui.set_browse_logos(ModelRc::default());
        ui.set_browse_logo_fogs(ModelRc::default());
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
        let f = favs.lock().unwrap_or_else(|e| e.into_inner());
        for item in &mut new_items {
            item.is_favorite = browse_is_favorite(&f, item);
        }
    }
    let (mut items, mut logos, mut fogs) = if append {
        (
            ui.get_search_results().iter().collect::<Vec<_>>(),
            ui.get_browse_logos().iter().collect::<Vec<_>>(),
            ui.get_browse_logo_fogs().iter().collect::<Vec<_>>(),
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    // No logos yet: rows get theirs as they come on screen
    logos.resize(items.len() + new_items.len(), slint::Image::default());
    fogs.resize(logos.len(), LogoFog::default());
    items.extend(new_items);
    ui.set_search_results(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    ui.set_browse_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
    ui.set_browse_logo_fogs(ModelRc::from(std::rc::Rc::new(VecModel::from(fogs))));
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
        // radio-browser reports "UNKNOWN" when it has no codec; show no badge
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

fn favorite_to_slint(f: &radiotrope_app::data::types::Favorite) -> FavoriteStation {
    FavoriteStation {
        id: f.id().into(),
        name: f.name().into(),
        url: f.url().into(),
        logo_url: f.station.logo_url.as_deref().unwrap_or("").into(),
        country: f.station.country.as_deref().unwrap_or("").into(),
        listen_time: format_listen_time(f.total_listen_time_secs).into(),
        last_played: format_last_played(f.last_played).into(),
        play_count: f.play_count.min(i32::MAX as u32) as i32,
        session_time: format_listen_time(session_listen_secs(&f.id())).into(),
    }
}

/// Stream URL and time of the last station started from the UI, so a
/// double tap doesn't start it twice
static LAST_PLAY_CLICK: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Listening time per favorite since the app was opened, by favorite ID
static SESSION_LISTEN: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn session_listen_secs(id: &str) -> u64 {
    SESSION_LISTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
    let mut favs = favorites.lock().unwrap_or_else(|e| e.into_inner());
    let Some(id) = favs.add_listening(url, secs, new_play).map(|f| f.id()) else {
        return;
    };
    *SESSION_LISTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
        let Some(fav) = favs.get(&row.id) else {
            continue;
        };
        let listen_time: slint::SharedString =
            format_listen_time(fav.total_listen_time_secs).into();
        let last_played: slint::SharedString = format_last_played(fav.last_played).into();
        let play_count = fav.play_count.min(i32::MAX as u32) as i32;
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

/// Total listening time for a favorites row: "3 h 25 m", "40 m", "<1 m",
/// or empty when never played
fn format_listen_time(secs: u64) -> String {
    let mins = secs / 60;
    match mins {
        _ if secs == 0 => String::new(),
        0 => "<1 m".into(),
        1..=59 => format!("{mins} m"),
        _ => format!("{} h {} m", mins / 60, mins % 60),
    }
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
    /// Every country from the last load; the picker shows a filtered copy
    static ALL_COUNTRIES: std::cell::RefCell<Vec<CountryEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Show the countries whose name contains `filter` (case-insensitive)
fn show_countries(ui: &App, filter: &str) {
    let filter = filter.trim().to_lowercase();
    let countries: Vec<CountryEntry> = ALL_COUNTRIES.with(|all| {
        all.borrow()
            .iter()
            .filter(|c| filter.is_empty() || c.name.to_lowercase().contains(&filter))
            .cloned()
            .collect()
    });
    ui.set_countries(ModelRc::from(std::rc::Rc::new(VecModel::from(countries))));
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

// In-memory cache of decoded slint::Image keyed by favorite ID.
// All callers run on the Slint event-loop thread, so thread_local is safe and avoids
// threading slint::Image (which is !Send) through closures.
thread_local! {
    static LOGO_IMAGE_CACHE: std::cell::RefCell<HashMap<String, (slint::Image, LogoFog)>> =
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

/// Remove a cached logo image, forcing re-decode on next refresh.
fn invalidate_logo_image(id: &str) {
    LOGO_IMAGE_CACHE.with(|cache| {
        cache.borrow_mut().remove(id);
    });
}

/// Redraw the favorites list, and start fetching the logos it lacks
fn refresh_favorites(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
) {
    // The logos are read and decoded after the lock is let go: an agent
    // waiting on the favorites isn't held up by them
    let sorted: Vec<radiotrope_app::data::types::Favorite> = {
        let favs = favorites.lock().unwrap_or_else(|e| e.into_inner());
        mark_browse_favorites(ui, &favs);
        note_favorites_shown(&favs);
        favs.sorted(FavoriteSort::Manual)
            .into_iter()
            .cloned()
            .collect()
    };
    let items: Vec<FavoriteStation> = sorted.iter().map(favorite_to_slint).collect();

    // Build parallel logo and fog models, using in-memory cache to avoid
    // re-decoding
    let mut missing = Vec::new();
    let (logos, fogs): (Vec<slint::Image>, Vec<LogoFog>) = LOGO_IMAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        sorted
            .iter()
            .map(|f| {
                let key = f.id();
                if let Some(logo) = cache.get(&key) {
                    return logo.clone();
                }
                if let Some((rgba, width, height)) = logo_service.get_cached_rgba(f) {
                    let pixel_buf = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &rgba, width, height,
                    );
                    let logo = (slint::Image::from_rgba8(pixel_buf), logo_fog(&rgba, width));
                    cache.insert(key, logo.clone());
                    logo
                } else {
                    if logo_service.is_missing(f) {
                        missing.push(f.clone());
                    }
                    Default::default()
                }
            })
            .unzip()
    });

    ui.set_favorites_list(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    ui.set_favorite_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
    ui.set_favorite_logo_fogs(ModelRc::from(std::rc::Rc::new(VecModel::from(fogs))));

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
const STARTING_STATUS: &str = "Resolving...";

/// The codec line of a station not playing yet
const AWAITING_STREAM: &str = "Awaiting stream";

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
        parts.push(format!("{} Hz", s.sample_rate));
    }
    if s.channels > 0 {
        parts.push(match s.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{} ch", n),
        });
    }
    parts.join(" \u{2022} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_gets_backslashes() {
        let path = |p: &str| explorer_path(std::path::Path::new(p));
        assert_eq!(
            path("C:/Users/José/Music/Radiotrope"),
            r"C:\Users\José\Music\Radiotrope"
        );
        assert_eq!(path(r"\\nas\music/Radiotrope"), r"\\nas\music\Radiotrope");
        assert_eq!(path(r"D:\Recordings"), r"D:\Recordings");
    }
}
