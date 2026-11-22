//! What the favorites list and the header's star do, and the listening time
//! credited to the favorite playing

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use radiotrope::audio::PlaybackState;
use radiotrope_app::config::ui::{LISTEN_CREDIT_SECS, MIN_LISTEN_SECS};
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::types::{FavoriteSort, Station};
use radiotrope_app::network::logo::LogoService;

use crate::app::delayed_save::DelayedSave;
use crate::app::listening::ListenSession;
use crate::app::state::AppSnapshot;
use crate::context::{lock, AppContext};
use crate::favorites_list::{
    invalidate_logo_image, note_favorites_shown, refresh_favorites, update_favorite_stats,
};
use crate::player_ui::{fetch_station_logo, play_station_with_metadata, SHOWN_STATION};
use crate::{shortcuts, App, FavoriteStation};

/// Wire what the favorites list and the header's star do: play, step,
/// star, edit, remove, move and reorder, and reset a row's figures
pub(crate) fn setup(ui: &App, ctx: &AppContext) {
    let (ui_tx, shared_state) = (&ctx.tx, &ctx.state);
    let (favorites, logo_service) = (&ctx.favorites, &ctx.logos);
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
}

/// Start the timers that keep the list current. They run while the
/// returned handles are kept.
pub(crate) fn start_timers(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
) -> [slint::Timer; 2] {
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
    [stats_timer, follow_timer]
}

/// Listening time per favorite since the app was opened, by favorite ID
pub(crate) static SESSION_LISTEN: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

pub(crate) fn session_listen_secs(id: &str) -> u64 {
    lock(&SESSION_LISTEN)
        .as_ref()
        .and_then(|m| m.get(id).copied())
        .unwrap_or(0)
}

/// Follow what is playing and add listening time to favorites: once a
/// minute while a station plays, and when it stops or changes
pub(crate) fn track_listening(
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
pub(crate) fn credit_listening(
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
pub(crate) fn add_listening(
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

/// A favorite just edited, in the window or from a phone
pub(crate) struct EditedFavorite<'a> {
    /// As it was
    pub(crate) old: Option<&'a radiotrope_app::data::types::Favorite>,
    /// Its id before the edit (it changes with the stream URL)
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) url: &'a str,
    /// Empty: no logo
    pub(crate) logo_url: &'a str,
    /// Empty: none
    pub(crate) country: &'a str,
}

/// Show an edited favorite: drop its old logo, update the player when it
/// is the station there, redraw the list, and fetch the new logo
pub(crate) fn show_edited_favorite(
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
        let favs = favs.clone();
        let logos = logo_svc.clone();
        // Shown in the player only if it is the station there now
        let shown = SHOWN_STATION.with(|s| s.borrow().seq());
        let station = Station::new(name, url).with_logo(logo_url);
        fetch_station_logo(ui, logo_svc, station, shown, move |ui| {
            // Refresh favorites list with the now-cached logo
            refresh_favorites(ui, &favs, &logos);
        });
    }
}
