mod app;
mod mcp;

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

use radiotrope_app::config::ui::SEARCH_PAGE_SIZE;
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::types::{url_to_id, FavoriteSort, Station};
use radiotrope_app::network::logo::LogoService;
use radiotrope_app::providers::types::{Category, CategoryType, SearchResults};
use radiotrope_app::providers::ProviderRegistry;

use app::controller::AppController;
use app::state::AppSnapshot;

/// Radiotrope — Internet radio player
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Enable MCP server on stdio (for AI agent integration)
    #[arg(long)]
    mcp: bool,
}

fn main() {
    let args = Args::parse();

    // Shared command channel + state
    let (cmd_tx, cmd_rx) = bounded(64);
    let shared_state = Arc::new(Mutex::new(AppSnapshot::default()));

    // Channel for the engine's analysis Arc (one-shot handshake)
    let (analysis_tx, analysis_rx) = bounded::<Arc<Mutex<AudioAnalysis>>>(1);

    // Channel for the engine's SharedStats (one-shot handshake)
    let (stats_tx, stats_rx) = bounded::<SharedStats>(1);

    // Favorites manager + shared logo service (created early so MCP can use them)
    let favorites = Arc::new(Mutex::new(
        FavoritesManager::load().unwrap_or_else(|_| FavoritesManager::new()),
    ));
    let logo_service = Arc::new(LogoService::new().expect("Failed to create logo service"));

    // Generation counter for browse logo fetches (to cancel stale requests)
    let browse_logo_gen = Arc::new(AtomicU64::new(0));

    // If --mcp, spawn MCP stdio server on a background thread
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
    let settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
    {
        let mut state = shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.volume = settings.volume;
        state.is_muted = settings.muted;
        state.eq_gains = settings.eq_gains;
        state.eq_preamp = settings.eq_preamp;
        state.eq_enabled = settings.eq_enabled;
        state.eq_preset_name = settings.eq_preset_name.clone();
        state.accent_color = settings.accent_color.clone();
    }

    // Create Slint UI
    let ui = App::new().unwrap();

    // Initial load of favorites into UI model
    refresh_favorites(&ui, &favorites, &logo_service);

    // Background: prefetch uncached logos for favorites
    {
        let logo_svc = logo_service.clone();
        let fav_clone = favorites.clone();
        let ui_weak = ui.as_weak();
        std::thread::Builder::new()
            .name("fav-logo-prefetch".into())
            .spawn(move || {
                let favs = fav_clone.lock().unwrap_or_else(|e| e.into_inner());
                let all: Vec<_> = favs
                    .sorted(FavoriteSort::Manual)
                    .into_iter()
                    .cloned()
                    .collect();
                drop(favs);

                // Clean up cached logos not belonging to any current favorite
                let valid_ids: std::collections::HashSet<String> =
                    all.iter().map(|f| f.id()).collect();
                let removed = logo_svc.cache().cleanup_orphaned(&valid_ids);
                if removed > 0 {
                    eprintln!("Logo cache: cleaned up {removed} orphaned image(s)");
                }

                let fetched = logo_svc.prefetch(&all);
                if fetched > 0 {
                    let logo_svc2 = logo_svc.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            refresh_favorites(&ui, &fav_clone, &logo_svc2);
                        }
                    });
                }
            })
            .ok();
    }

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

    // Apply saved accent color
    if let Some((r, g, b)) = settings.accent_color_rgb() {
        ui.set_accent_color(slint::Color::from_rgb_u8(r, g, b));
    }

    // Apply saved theme and viz mode
    ui.set_dark_mode(settings.theme.is_dark());
    ui.set_viz_mode(settings.viz_mode.as_str().into());

    // Apply saved window size
    if let (Some(w), Some(h)) = (settings.window_width, settings.window_height) {
        ui.window()
            .set_size(slint::LogicalSize::new(w as f32, h as f32));
    }

    // Restore last station to UI (so user can hit Play to resume)
    if let Some(ref station) = settings.last_station {
        ui.set_station_name(station.name.as_str().into());
        ui.set_station_url(station.url.as_str().into());
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
                }
            }
        }
        // Update shared state so controller knows about the last station
        {
            let mut s = shared_state.lock().unwrap_or_else(|e| e.into_inner());
            s.station_name = Some(station.name.clone());
            s.station_url = Some(station.url.clone());
        }
    }

    // Send initial volume/mute to controller so engine starts at the correct level
    {
        let _ = cmd_tx.send(app::state::AppCommand::SetVolume(settings.volume));
        if settings.muted {
            let _ = cmd_tx.send(app::state::AppCommand::Mute);
        }
    }

    // Send initial EQ state to controller (which will forward to engine once started)
    {
        let _ = cmd_tx.send(app::state::AppCommand::SetEqEnabled(settings.eq_enabled));
        if let Some(ref preset_name) = settings.eq_preset_name {
            let _ = cmd_tx.send(app::state::AppCommand::SetEqPreset(preset_name.clone()));
        } else {
            let _ = cmd_tx.send(app::state::AppCommand::SetEqGains(settings.eq_gains));
        }
        let _ = cmd_tx.send(app::state::AppCommand::SetEqPreamp(settings.eq_preamp));
    }

    // Wire Slint callbacks → cmd_tx
    let play_tx = cmd_tx.clone();
    let play_url_weak = ui.as_weak();
    let play_url_favs = favorites.clone();
    let play_url_logo_svc = logo_service.clone();
    let play_url_state = shared_state.clone();
    ui.on_play_url(move |url| {
        if let Some(ui) = play_url_weak.upgrade() {
            play_station_with_metadata(
                &ui,
                &play_tx,
                &play_url_state,
                &play_url_favs,
                &play_url_logo_svc,
                PlayMetadata {
                    url: url.to_string(),
                    name: None,
                    logo_url: None,
                    country: None,
                    provider_id: None,
                },
            );
        }
    });

    let stop_tx = cmd_tx.clone();
    ui.on_stop_clicked(move || {
        let _ = stop_tx.send(app::state::AppCommand::Stop);
    });

    let vol_tx = cmd_tx.clone();
    ui.on_volume_changed(move |vol| {
        let _ = vol_tx.send(app::state::AppCommand::SetVolume(vol));
    });

    let mute_tx = cmd_tx.clone();
    let mute_state = shared_state.clone();
    ui.on_mute_clicked(move || {
        let is_muted = mute_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_muted;
        if is_muted {
            let _ = mute_tx.send(app::state::AppCommand::Unmute);
        } else {
            let _ = mute_tx.send(app::state::AppCommand::Mute);
        }
    });

    // toggle-favorite callback
    {
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_favorite(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let url = ui.get_station_url().to_string();
            if url.is_empty() {
                return;
            }
            let name = ui.get_station_name().to_string();
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
        let play_tx = cmd_tx.clone();
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

            // Invalidate old cached logo before updating (cache key = station id = url hash)
            {
                let f = favs.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(old_fav) = f.get(&id) {
                    logo_svc.delete(old_fav);
                }
                drop(f);
            }
            invalidate_logo_image(&id);

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

            let mut f = favs.lock().unwrap_or_else(|e| e.into_inner());
            let _ = f.update(&id, update);
            let _ = f.save();
            drop(f);

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
                std::thread::Builder::new()
                    .name("edit-logo-fetch".into())
                    .spawn(move || {
                        let tmp_station = Station::new(&name, &url).with_logo(&logo_url);
                        if let Some((rgba, width, height)) = logo_svc.get_rgba(&tmp_station) {
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = ui_weak.upgrade() else { return };
                                // Update playback logo if this is the current station
                                let current_url = ui.get_station_url().to_string();
                                if current_url == url {
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

                if from < sorted.len() && to < sorted.len() {
                    let id = sorted.remove(from);
                    sorted.insert(to, id);
                    let id_refs: Vec<&str> = sorted.iter().map(|s| s.as_str()).collect();
                    let _ = f.reorder(&id_refs);
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
                    let mut f = favs_bg.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = f.save();
                })
                .ok();
        });
    }

    // EQ callbacks
    {
        let tx = cmd_tx.clone();
        ui.on_eq_band_changed(move |band, gain| {
            let _ = tx.send(app::state::AppCommand::SetEqBand {
                band: band as usize,
                gain_db: gain,
            });
        });
    }
    {
        let tx = cmd_tx.clone();
        ui.on_eq_preamp_changed(move |val| {
            let _ = tx.send(app::state::AppCommand::SetEqPreamp(val));
        });
    }
    {
        let tx = cmd_tx.clone();
        ui.on_eq_preset_selected(move |name| {
            let _ = tx.send(app::state::AppCommand::SetEqPreset(name.to_string()));
        });
    }
    {
        let tx = cmd_tx.clone();
        ui.on_eq_enabled_toggled(move |val| {
            let _ = tx.send(app::state::AppCommand::SetEqEnabled(val));
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
            let hex = hex_str.trim().to_string();
            let hex_clean = hex.strip_prefix('#').unwrap_or(&hex);
            if hex_clean.len() != 6 {
                return;
            }
            let Ok(r) = u8::from_str_radix(&hex_clean[0..2], 16) else {
                return;
            };
            let Ok(g) = u8::from_str_radix(&hex_clean[2..4], 16) else {
                return;
            };
            let Ok(b) = u8::from_str_radix(&hex_clean[4..6], 16) else {
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

    // WiFi settings (embedded only)
    // TODO: gate with feature flag once #[cfg(feature = "embedded")] on function calls is verified
    setup_wifi(&ui);

    // Rotary encoder for volume control (GPIO 5=CLK, GPIO 6=DT, GPIO 13=SW)
    // Disabled: embedded-only hardware, parked for now
    // setup_rotary_encoder(&ui, cmd_tx.clone(), shared_state.clone());

    // What the station browser shows, and how far "Load More" has paged
    let browse_state = Arc::new(Mutex::new((BrowseQuery::Top, 0usize)));

    // search-stations callback (an empty query shows the top stations)
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let logo_svc = logo_service.clone();
        ui.on_search_stations(move |query| {
            let query = query.trim();
            let query = if query.is_empty() {
                BrowseQuery::Top
            } else {
                BrowseQuery::Search(query.to_string())
            };
            start_browse(&ui_weak, &state, &gen, &logo_svc, query);
        });
    }

    // load-top-stations callback
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let logo_svc = logo_service.clone();
        ui.on_load_top_stations(move || {
            start_browse(&ui_weak, &state, &gen, &logo_svc, BrowseQuery::Top);
        });
    }

    // browse-country callback (an empty query lists the whole country)
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let logo_svc = logo_service.clone();
        ui.on_browse_country(move |name, code, query| {
            let category = Category::new(name.as_str(), name.as_str(), CategoryType::Country)
                .with_code(Some(code.to_string()).filter(|c| !c.is_empty()));
            let query = BrowseQuery::Category {
                category,
                query: query.trim().to_string(),
            };
            start_browse(&ui_weak, &state, &gen, &logo_svc, query);
        });
    }

    // load-more-stations callback
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let logo_svc = logo_service.clone();
        ui.on_load_more_stations(move || {
            let (query, offset) = {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.1 += SEARCH_PAGE_SIZE;
                (s.0.clone(), s.1)
            };
            let my_gen = gen.load(Ordering::Relaxed);
            let ui_weak = ui_weak.clone();
            let gen = gen.clone();
            let logo_svc = logo_svc.clone();
            std::thread::Builder::new()
                .name("load-more".into())
                .spawn(move || {
                    let results = fetch_browse_page(&query, offset);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        ui.set_search_loading_more(false);
                        // A new search replaced the list meanwhile
                        if gen.load(Ordering::Relaxed) != my_gen {
                            return;
                        }
                        match results {
                            Ok(results) => {
                                show_browse_results(&ui, results, true, logo_svc, gen, my_gen)
                            }
                            Err(e) => ui.set_search_error(format!("{e}").into()),
                        }
                    });
                })
                .ok();
        });
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
        let play_tx = cmd_tx.clone();
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
    std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || {
            let mut ctrl = AppController::new(cmd_rx, ctrl_tx, ctrl_state, analysis_tx, stats_tx);
            ctrl.run();
        })
        .expect("Failed to spawn controller thread");

    // Wait for engine to initialize and send us the analysis Arc + SharedStats
    let analysis = analysis_rx.recv_timeout(Duration::from_secs(5)).ok();
    let shared_stats = stats_rx.recv_timeout(Duration::from_secs(5)).ok();

    // Visualization timer — 30ms (~33 FPS)
    let _viz_timer = slint::Timer::default();
    if let Some(analysis) = analysis {
        let ui_weak = ui.as_weak();
        // Pre-allocate the spectrum model once; update in-place each tick
        let spectrum_model = std::rc::Rc::new(VecModel::from(
            vec![0.0f32; radiotrope::config::audio::SPECTRUM_BANDS],
        ));
        let spectrum_rc = ModelRc::from(spectrum_model.clone());
        ui.set_spectrum(spectrum_rc);
        _viz_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(33),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                // Skip polling when not playing — zero out once on stop transition
                if !ui.get_is_playing() {
                    if ui.get_vu_left() != 0.0 || ui.get_vu_right() != 0.0 {
                        ui.set_vu_left(0.0);
                        ui.set_vu_right(0.0);
                        for i in 0..spectrum_model.row_count() {
                            spectrum_model.set_row_data(i, 0.0);
                        }
                    }
                    return;
                }
                // try_lock: skip this tick if engine/analyzer holds the lock
                let Ok(a) = analysis.try_lock() else { return };
                let (vu_l, vu_r, spectrum) = (a.vu_left, a.vu_right, a.spectrum);
                drop(a);
                ui.set_vu_left(vu_l);
                ui.set_vu_right(vu_r);
                // Update model in-place — no allocation
                for (i, &val) in spectrum.iter().enumerate() {
                    spectrum_model.set_row_data(i, val);
                }
            },
        );
    }

    // Poll SharedStats → statistics dialog properties (200ms)
    let _stats_timer = slint::Timer::default();
    if let Some(shared_stats) = shared_stats {
        let ui_weak = ui.as_weak();
        _stats_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                // Skip polling when not playing — stats are stale anyway
                if !ui.get_is_playing() {
                    return;
                }
                // try_lock: skip this tick if engine holds shared_stats
                let Ok(s) = shared_stats.try_lock() else {
                    return;
                };
                let stats_copy = s.clone();
                drop(s);
                update_stats_ui(&ui, &stats_copy);
            },
        );
    }

    // Poll shared state → Slint properties (runs on UI thread via Timer)
    let ui_weak = ui.as_weak();
    let poll_state = shared_state.clone();
    let poll_favs = favorites.clone();
    let poll_logo_svc = logo_service.clone();
    let last_fav_generation = std::cell::Cell::new(
        favorites
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation(),
    );
    let last_poll_url = std::cell::RefCell::new(String::new());
    let _timer = slint::Timer::default();
    _timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };

            // Check for external favorites changes (e.g. from MCP)
            if let Ok(f) = poll_favs.try_lock() {
                let gen = f.generation();
                drop(f);
                if gen != last_fav_generation.get() {
                    last_fav_generation.set(gen);
                    refresh_favorites(&ui, &poll_favs, &poll_logo_svc);

                    // Prefetch missing logos on background thread, then refresh UI
                    let prefetch_favs = poll_favs.clone();
                    let prefetch_logo_svc = poll_logo_svc.clone();
                    let prefetch_ui_weak = ui.as_weak();
                    std::thread::Builder::new()
                        .name("fav-logo-prefetch-poll".into())
                        .spawn(move || {
                            let favs = prefetch_favs.lock().unwrap_or_else(|e| e.into_inner());
                            let all: Vec<_> = favs
                                .sorted(FavoriteSort::Manual)
                                .into_iter()
                                .cloned()
                                .collect();
                            drop(favs);
                            let fetched = prefetch_logo_svc.prefetch(&all);
                            if fetched > 0 {
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = prefetch_ui_weak.upgrade() {
                                        refresh_favorites(&ui, &prefetch_favs, &prefetch_logo_svc);
                                    }
                                });
                            }
                        })
                        .ok();
                }
            }

            // try_lock: skip this tick if controller holds the lock
            let Ok(s) = poll_state.try_lock() else { return };
            // Copy all data under lock, then drop before touching UI
            let station_name: slint::SharedString =
                s.station_name.as_deref().unwrap_or("Radiotrope").into();
            let codec_info: slint::SharedString = format_codec_line(&s).into();
            let status_text: slint::SharedString = s.status_text.as_ref().into();
            let is_error = s.is_error;
            let is_loading = s.is_resolving || s.status_text == "Connecting...";
            let is_playing = s.playback == PlaybackState::Playing;
            let now_playing: slint::SharedString = if !s.title.is_empty() {
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
            let station_url: Option<slint::SharedString> = s.station_url.as_deref().map(Into::into);
            let eq_gains = s.eq_gains;
            let eq_preamp = s.eq_preamp;
            let eq_enabled = s.eq_enabled;
            let eq_preset: slint::SharedString = s
                .eq_preset_name
                .as_deref()
                .unwrap_or("")
                .into();
            drop(s);

            // Set UI properties without holding any lock
            ui.set_station_name(station_name);
            ui.set_codec_info(codec_info);
            ui.set_status_text(status_text);
            ui.set_is_error(is_error);
            ui.set_is_playing(is_playing);
            ui.set_now_playing_title(now_playing);
            if !ui.get_volume_dragging() {
                ui.set_volume(volume);
            }
            ui.set_is_muted(is_muted);
            if let Some(url) = station_url {
                let url_changed = {
                    let last = last_poll_url.borrow();
                    *last != url.as_str()
                };
                ui.set_station_url(url.clone());
                // Update favorite star based on current station
                let is_fav = poll_favs
                    .lock()
                    .map(|f| f.is_favorite(url.as_str()))
                    .unwrap_or(false);
                ui.set_is_station_favorited(is_fav);

                // When station URL changes (e.g. MCP play), update logo
                if url_changed {
                    *last_poll_url.borrow_mut() = url.to_string();

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
                        // Not cached — look up logo URL from favorites and fetch
                        let logo_url = poll_favs
                            .lock()
                            .ok()
                            .and_then(|f| {
                                f.get_by_url(url.as_str())
                                    .and_then(|fav| fav.station.logo_url.clone())
                            });
                        if let Some(logo) = logo_url {
                            if !logo.is_empty() {
                                ui.set_station_logo_url(logo.as_str().into());
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
                                                if ui.get_station_url() == station_url.as_str() {
                                                    let pb = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                                                    ui.set_current_logo(slint::Image::from_rgba8(pb));
                                                }
                                            });
                                        }
                                    })
                                    .ok();
                            }
                        } else {
                            ui.set_station_logo_url(Default::default());
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

    // Handle SIGTERM/SIGINT gracefully so settings are saved on shutdown.
    // systemd sends SIGTERM on stop/reboot — without this, the process is
    // killed before the save-on-exit code below can run.
    #[cfg(unix)]
    {
        use std::sync::atomic::AtomicBool;
        static QUIT_FLAG: AtomicBool = AtomicBool::new(false);

        unsafe {
            for sig in [libc::SIGTERM, libc::SIGINT] {
                libc::signal(sig, handle_quit_signal as *const () as libc::sighandler_t);
            }
        }

        extern "C" fn handle_quit_signal(_sig: libc::c_int) {
            QUIT_FLAG.store(true, Ordering::SeqCst);
            let _ = slint::quit_event_loop();
        }
    }

    // Run Slint event loop (blocks main thread)
    ui.run().unwrap();

    // Final save before shutdown
    save_settings(&shared_state, &ui);

    // UI closed — tell controller to shut down
    let _ = cmd_tx.send(app::state::AppCommand::Shutdown);
}

/// Set up WiFi settings UI callbacks
/// Rotary encoder for volume control (KY-040 on GPIO 5/6/13)
/// Uses the kernel `rotary-encoder` driver via /dev/input/eventN for reliable
/// quadrature decoding. Push button on GPIO 13 via gpiomon.
#[allow(dead_code)] // embedded-only, currently disabled
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
                    drop(s);

                    if is_playing {
                        let _ = cmd_tx.send(app::state::AppCommand::Stop);
                    } else if let Some(url) = station_url {
                        let _ = cmd_tx.send(app::state::AppCommand::Play {
                            url,
                            name: station_name,
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

fn setup_wifi(ui: &App) {
    // TODO: gate properly with feature flag
    eprintln!("setup_wifi: enabling WiFi UI");
    ui.set_wifi_embedded_mode(true);

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

/// Persist current app state to settings.json
fn save_settings(shared_state: &Arc<Mutex<AppSnapshot>>, ui: &App) {
    let s = shared_state.lock().unwrap_or_else(|e| e.into_inner());
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

    if let Some(ref url) = s.station_url {
        if !url.is_empty() {
            let name = s.station_name.as_deref().unwrap_or("Unknown");
            let mut station = Station::new(name, url);
            let logo_url = ui.get_station_logo_url().to_string();
            if !logo_url.is_empty() {
                station = station.with_logo(&logo_url);
            }
            settings.last_station = Some(station);
        }
    }

    let size = ui.window().size();
    if size.width > 0 && size.height > 0 {
        settings.window_width = Some(size.width);
        settings.window_height = Some(size.height);
    }

    drop(s);
    let _ = settings.save();
}

/// Shared helper for all play actions. Enriches metadata from favorites if available,
/// sets all UI properties consistently, sends the Play command, and spawns logo fetch.
fn play_station_with_metadata(
    ui: &App,
    cmd_tx: &crossbeam_channel::Sender<app::state::AppCommand>,
    shared_state: &Arc<Mutex<AppSnapshot>>,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
    req: PlayMetadata,
) {
    // A favorite's URL, name and logo take priority over the caller's
    let PlayMetadata {
        url,
        name,
        logo_url,
        country,
        ..
    } = favorites
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .resolve_play(req);

    // Set UI metadata properties
    ui.set_station_logo_url(logo_url.as_deref().unwrap_or("").into());
    ui.set_station_country(country.as_deref().unwrap_or("").into());

    // Try cached logo first to avoid placeholder flash
    let cache_hit = if let Some(ref logo) = logo_url {
        if !logo.is_empty() {
            let tmp = Station::new(name.as_deref().unwrap_or(""), &url).with_logo(logo);
            if let Some((rgba, w, h)) = logo_service.get_cached_rgba(&tmp) {
                let pb = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                ui.set_current_logo(slint::Image::from_rgba8(pb));
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    if !cache_hit {
        ui.set_current_logo(Default::default());
    }

    // Send play command
    let _ = cmd_tx.send(app::state::AppCommand::Play {
        url: url.clone(),
        name: name.clone(),
    });

    // Save settings (persists last_station, volume, eq, etc.)
    {
        let mut s = shared_state.lock().unwrap_or_else(|e| e.into_inner());
        s.station_url = Some(url.clone());
        s.station_name = name.clone();
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
    ui.set_stat_uptime(format_uptime(s.play_started_at).into());

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

fn format_health(state: &HealthState) -> String {
    match state {
        HealthState::WaitingForAudio => "Waiting".into(),
        HealthState::Healthy => "Healthy".into(),
        HealthState::Stalled => "Stalled".into(),
        HealthState::Failed(reason) => format!("Failed ({reason:?})"),
    }
}

fn format_uptime(started: Option<Instant>) -> String {
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

/// Replace the browser list with the first page of `query`, fetched in the
/// background. Results of an older request that finish later are dropped.
fn start_browse(
    ui_weak: &slint::Weak<App>,
    state: &Arc<Mutex<(BrowseQuery, usize)>>,
    gen: &Arc<AtomicU64>,
    logo_svc: &Arc<LogoService>,
    query: BrowseQuery,
) {
    *state.lock().unwrap_or_else(|e| e.into_inner()) = (query.clone(), 0);
    let my_gen = gen.fetch_add(1, Ordering::Relaxed) + 1;
    // Clear old results immediately
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_search_results(ModelRc::default());
        ui.set_browse_logos(ModelRc::default());
        ui.set_has_more(false);
        ui.set_search_error(Default::default());
        ui.set_search_loading(true);
    }
    let ui_weak = ui_weak.clone();
    let gen = gen.clone();
    let logo_svc = logo_svc.clone();
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
                    Ok(results) => show_browse_results(&ui, results, false, logo_svc, gen, my_gen),
                    Err(e) => ui.set_search_error(format!("{e}").into()),
                }
                ui.set_search_loading(false);
            });
        })
        .ok();
}

/// Show a page of browser results, appending to the list or replacing it,
/// then fetch the logos that aren't cached
fn show_browse_results(
    ui: &App,
    results: SearchResults,
    append: bool,
    logo_svc: Arc<LogoService>,
    gen: Arc<AtomicU64>,
    my_gen: u64,
) {
    let new_items: Vec<BrowseStation> = results.stations.iter().map(station_to_browse).collect();
    let (mut items, mut logos) = if append {
        let existing = ui.get_search_results();
        let existing_logos = ui.get_browse_logos();
        (
            existing.iter().collect::<Vec<_>>(),
            existing_logos.iter().collect::<Vec<_>>(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let (new_logos, misses) = build_browse_logos_from_cache(&new_items, items.len());
    items.extend(new_items);
    logos.extend(new_logos);
    ui.set_search_results(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    ui.set_browse_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
    ui.set_has_more(results.has_more);
    ui.set_search_error(Default::default());
    spawn_browse_logo_fetch(ui.as_weak(), logo_svc, misses, gen, my_gen);
}

fn station_to_browse(s: &Station) -> BrowseStation {
    BrowseStation {
        name: s.name.as_str().into(),
        url: s.url.as_str().into(),
        logo_url: s.logo_url.as_deref().unwrap_or("").into(),
        country: s.country.as_deref().unwrap_or("").into(),
        codec: s.codec.as_deref().unwrap_or("").into(),
        bitrate: s.bitrate.unwrap_or(0) as i32,
        provider_id: s.provider_id.as_deref().unwrap_or("").into(),
    }
}

/// Number of concurrent logo fetch threads
const BROWSE_LOGO_WORKERS: usize = 6;

/// Max logo thumbnail size (2x display size for HiDPI)
const BROWSE_LOGO_SIZE: u32 = 64;

/// Spawn background threads to fetch browse logos and progressively update the UI.
///
/// `work` contains only cache misses: `(model_idx, station_url, logo_url)`.
fn spawn_browse_logo_fetch(
    ui_weak: slint::Weak<App>,
    logo_svc: Arc<LogoService>,
    work: Vec<(usize, String, String)>,
    gen: Arc<AtomicU64>,
    my_gen: u64,
) {
    if work.is_empty() {
        return;
    }
    // Shared work queue: each worker grabs the next item atomically
    let next_idx = Arc::new(AtomicU64::new(0));
    let work = Arc::new(work);
    let worker_count = BROWSE_LOGO_WORKERS.min(work.len());

    for w in 0..worker_count {
        let ui_weak = ui_weak.clone();
        let logo_svc = logo_svc.clone();
        let work = work.clone();
        let gen = gen.clone();
        let next_idx = next_idx.clone();
        std::thread::Builder::new()
            .name(format!("browse-logo-{w}"))
            .spawn(move || {
                loop {
                    if gen.load(Ordering::Relaxed) != my_gen {
                        return;
                    }
                    let i = next_idx.fetch_add(1, Ordering::Relaxed) as usize;
                    if i >= work.len() {
                        return;
                    }
                    let (idx, ref station_url, ref logo_url) = work[i];
                    let data = logo_svc.fetch_raw(logo_url).ok();

                    if gen.load(Ordering::Relaxed) != my_gen {
                        return;
                    }
                    if let Some(data) = data {
                        if let Ok(img) = image::load_from_memory(&data) {
                            let thumb = if img.width() > BROWSE_LOGO_SIZE
                                || img.height() > BROWSE_LOGO_SIZE
                            {
                                img.thumbnail(BROWSE_LOGO_SIZE, BROWSE_LOGO_SIZE)
                            } else {
                                img
                            };
                            let rgba = thumb.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            let pixels = rgba.into_raw();

                            let cache_key = url_to_id(station_url);
                            let ui_weak = ui_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = ui_weak.upgrade() else { return };
                                let pixel_buf =
                                    SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        &pixels, w, h,
                                    );
                                let img = slint::Image::from_rgba8(pixel_buf);
                                // Insert into in-memory cache for instant reuse
                                BROWSE_IMAGE_CACHE.with(|cache| {
                                    cache.borrow_mut().insert(cache_key, img.clone());
                                });
                                let model = ui.get_browse_logos();
                                if idx < model.row_count() {
                                    model.set_row_data(idx, img);
                                }
                            });
                        }
                    }
                }
            })
            .ok();
    }
}

fn favorite_to_slint(f: &radiotrope_app::data::types::Favorite) -> FavoriteStation {
    FavoriteStation {
        id: f.id().into(),
        name: f.name().into(),
        url: f.url().into(),
        logo_url: f.station.logo_url.as_deref().unwrap_or("").into(),
        country: f.station.country.as_deref().unwrap_or("").into(),
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
    static LOGO_IMAGE_CACHE: std::cell::RefCell<HashMap<String, slint::Image>> =
        std::cell::RefCell::new(HashMap::new());

    static BROWSE_IMAGE_CACHE: std::cell::RefCell<HashMap<String, slint::Image>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Build browse logos from in-memory cache, returning cached images and work items for misses.
///
/// `offset` is the starting index in the UI model (0 for fresh results, N for load-more).
fn build_browse_logos_from_cache(
    items: &[BrowseStation],
    offset: usize,
) -> (Vec<slint::Image>, Vec<(usize, String, String)>) {
    BROWSE_IMAGE_CACHE.with(|cache| {
        let cache = cache.borrow();
        let mut logos = Vec::with_capacity(items.len());
        let mut misses = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let key = url_to_id(item.url.as_ref());
            if let Some(img) = cache.get(&key) {
                logos.push(img.clone());
            } else {
                logos.push(Default::default());
                let logo_url = item.logo_url.to_string();
                if !logo_url.is_empty() {
                    misses.push((offset + i, item.url.to_string(), logo_url));
                }
            }
        }
        (logos, misses)
    })
}

/// Remove a cached logo image, forcing re-decode on next refresh.
fn invalidate_logo_image(id: &str) {
    LOGO_IMAGE_CACHE.with(|cache| {
        cache.borrow_mut().remove(id);
    });
}

fn refresh_favorites(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
) {
    let favs = favorites.lock().unwrap_or_else(|e| e.into_inner());
    let sorted = favs.sorted(FavoriteSort::Manual);
    let items: Vec<FavoriteStation> = sorted.iter().map(|f| favorite_to_slint(f)).collect();

    // Build parallel logos model, using in-memory cache to avoid re-decoding
    let logos: Vec<slint::Image> = LOGO_IMAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        sorted
            .iter()
            .map(|f| {
                let key = f.id();
                if let Some(img) = cache.get(&key) {
                    return img.clone();
                }
                if let Some((rgba, width, height)) = logo_service.get_cached_rgba(*f) {
                    let pixel_buf = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &rgba, width, height,
                    );
                    let img = slint::Image::from_rgba8(pixel_buf);
                    cache.insert(key, img.clone());
                    img
                } else {
                    Default::default()
                }
            })
            .collect()
    });

    drop(favs);

    ui.set_favorites_list(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    ui.set_favorite_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
}

fn format_codec_line(s: &AppSnapshot) -> String {
    if s.codec_name.is_empty() {
        return "Awaiting stream".to_string();
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
