//! What the favorites list and the header's star do, and the listening time
//! credited to the favorite playing

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelExt, ModelRc, VecModel};

use radiotrope_app::config::ui::{FAVORITES_SAVE_DELAY, LISTEN_CREDIT_SECS, MIN_LISTEN_SECS};
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::types::{Favorite, FavoriteSort, Station};
use radiotrope_app::network::logo::LogoService;

use crate::app::delayed_save::{DelayedSave, SaveErrors, Saved, SAVE_RETRY};
use crate::app::listening::ListenSession;
use crate::app::state::AppSnapshot;
use crate::context::{lock, AppContext};
use crate::favorites_list::{
    invalidate_logo_image, note_favorites_shown, refresh_favorites, update_favorite_stats,
};
use crate::player_ui::{
    fetch_station_logo, play_station_with_metadata, step_to_station, SHOWN_STATION,
};
use crate::window_settings::save_reason;
use crate::{shortcuts, App, FavoriteStation, FavoritesFind};

/// The thread that saves the favorites, once [`start_saver`] started it
static SAVER: OnceLock<DelayedSave> = OnceLock::new();

/// Start the thread that saves the favorites: a moment after listening
/// time is added ([`DelayedSave::request`]), at once after a change the
/// user made ([`save_favorites`]). A save that fails is logged, shown
/// through [`SaveErrors`] until one works, and tried again.
pub(crate) fn start_saver(favorites: &Arc<Mutex<FavoritesManager>>) -> DelayedSave {
    let saver = saver_of(
        favorites.clone(),
        None,
        SaveErrors::shared().clone(),
        SAVE_RETRY,
    );
    let _ = SAVER.set(saver.clone());
    saver
}

/// A saver of `favorites` to `path`, or to where they are kept
fn saver_of(
    favorites: Arc<Mutex<FavoritesManager>>,
    path: Option<PathBuf>,
    errors: SaveErrors,
    retry: std::time::Duration,
) -> DelayedSave {
    DelayedSave::start_reporting(
        "favorites-save",
        FAVORITES_SAVE_DELAY,
        "favorites",
        errors,
        retry,
        move || save_unheld(&favorites, path.as_deref()),
    )
}

/// Save `favorites`, holding them only to copy what there is to save and
/// to note what was written: the window, agents and phones aren't kept
/// waiting while the disk is written and the file lock waited for
fn save_unheld(favorites: &Mutex<FavoritesManager>, path: Option<&Path>) -> Result<Saved, String> {
    let job = {
        let favorites = lock(favorites);
        match path {
            Some(path) => favorites.begin_save_to(path),
            None => favorites.begin_save().map_err(|e| save_reason(&e))?,
        }
    };
    let Some(job) = job else {
        return Ok(Saved::All);
    };
    let done = job.write();
    let mut favorites = lock(favorites);
    favorites.finish_save(done).map_err(|e| save_reason(&e))?;
    Ok(if favorites.is_dirty() {
        Saved::More
    } else {
        Saved::All
    })
}

/// Save the favorites after a change made in the window. The handler only
/// changed them in memory: the file is written at once, on the saving
/// thread, and a failure is shown and retried there.
pub(crate) fn save_favorites() {
    match SAVER.get() {
        Some(saver) => saver.request_now(),
        None => eprintln!("Favorites: changed before their saving thread was started"),
    }
}

/// Save the favorites at exit, on this thread, whatever is or isn't due
pub(crate) fn save_at_exit(favorites: &Mutex<FavoritesManager>, saver: &DelayedSave) {
    // A save being run is waited for; one that is due is run
    saver.flush();
    // And what a failed save left (nothing is written when nothing is)
    if let Err(e) = lock(favorites).save() {
        eprintln!("Can't save favorites: {}", save_reason(&e));
    }
}

/// What Find in favorites looks for: the text typed, without the spaces
/// around it and whatever the case. Empty finds everything.
fn find_needle(text: &str) -> String {
    text.trim().to_lowercase()
}

/// Whether Find shows a favorite called `name` for a needle of
/// [`find_needle`]: its name contains it, whatever the case
fn find_matches(name: &str, needle: &str) -> bool {
    needle.is_empty() || name.to_lowercase().contains(needle)
}

/// Give the favorites list its Find: the rows whose name contains the
/// text, and where each is in the whole list (its logo is read there).
/// The rows stay those of the whole list, so a figure updated there shows
/// in a found row too.
pub(crate) fn install_find(ui: &App) {
    let find = ui.global::<FavoritesFind>();
    find.on_stations(|stations, text| found_stations(stations, &text));
    find.on_rows(|stations, text| found_rows(&stations, &text));
}

/// The rows of `stations` Find shows for `text`, in their order. They
/// follow the rows they come from: a figure updated there shows here.
fn found_stations(stations: ModelRc<FavoriteStation>, text: &str) -> ModelRc<FavoriteStation> {
    let needle = find_needle(text);
    if needle.is_empty() {
        return stations;
    }
    ModelRc::new(stations.filter(move |station| find_matches(&station.name, &needle)))
}

/// Where each row [`found_stations`] shows is in `stations`: its logo is
/// read there. Empty when nothing was typed (every row is its own).
fn found_rows(stations: &ModelRc<FavoriteStation>, text: &str) -> ModelRc<i32> {
    let needle = find_needle(text);
    if needle.is_empty() {
        return ModelRc::default();
    }
    let rows: Vec<i32> = stations
        .iter()
        .enumerate()
        .filter(|(_, station)| find_matches(&station.name, &needle))
        .filter_map(|(row, _)| i32::try_from(row).ok())
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// A favorite just removed, and its place in the list, for Undo
pub(crate) struct Removed {
    favorite: Favorite,
    /// Its row, counted from the top
    index: usize,
}

thread_local! {
    /// The favorite removed last, while its Undo is offered
    static REMOVED: RefCell<Option<Removed>> = const { RefCell::new(None) };
    /// Counts the removals: each one offers its Undo anew
    static UNDO_OFFERS: Cell<i32> = const { Cell::new(0) };
}

/// Remove the favorite with this id, noting its place in the list
pub(crate) fn remove_keeping_place(favorites: &mut FavoritesManager, id: &str) -> Option<Removed> {
    // What another player saved since is taken in first: the place is in
    // the list as it is now
    favorites.reload_if_changed();
    let index = favorites
        .sorted(FavoriteSort::Manual)
        .iter()
        .position(|favorite| favorite.id() == id)?;
    let favorite = favorites.remove(id).ok()?;
    Some(Removed { favorite, index })
}

/// Put a removed favorite back at its old place (the end, when the list
/// got shorter than that since). False when its stream is a favorite
/// again already.
fn put_back(favorites: &mut FavoritesManager, removed: Removed) -> bool {
    let id = removed.favorite.id();
    if favorites.add(removed.favorite).is_err() {
        return false;
    }
    let mut ids: Vec<String> = favorites
        .sorted(FavoriteSort::Manual)
        .iter()
        .map(|favorite| favorite.id())
        .filter(|other| *other != id)
        .collect();
    ids.insert(removed.index.min(ids.len()), id);
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    favorites.reorder(&ids).is_ok()
}

/// Offer to put back the favorite just removed: "Removed from Favorites ·
/// Undo" under the list, for a few seconds
pub(crate) fn offer_undo(ui: &App, removed: Option<Removed>) {
    let Some(removed) = removed else { return };
    REMOVED.set(Some(removed));
    let offer = UNDO_OFFERS.get().wrapping_add(1).max(1);
    UNDO_OFFERS.set(offer);
    ui.set_favorite_undo(offer);
}

/// The favorite whose Undo is offered was starred again by hand: its Undo
/// has nothing left to do
pub(crate) fn forget_undo_of(ui: &App, starred_again: impl Fn(&Favorite) -> bool) {
    let was_removed =
        REMOVED.with_borrow(|removed| removed.as_ref().is_some_and(|r| starred_again(&r.favorite)));
    if was_removed {
        REMOVED.set(None);
        ui.set_favorite_undo(0);
    }
}

/// What to play for a favorite's row (the player adds the favorite's
/// other streams as backups when it starts it)
fn play_of(station: &FavoriteStation) -> PlayMetadata {
    let filled = |text: &slint::SharedString| (!text.is_empty()).then(|| text.to_string());
    PlayMetadata {
        url: station.url.to_string(),
        backups: Vec::new(),
        name: Some(station.name.to_string()),
        logo_url: filled(&station.logo_url),
        country: filled(&station.country),
        provider_id: None,
    }
}

/// The station Next and Previous step from: the one in the player,
/// whatever it is doing. One still connecting, reconnecting or failed
/// counts like one playing, so a press doesn't wait for it to play (it
/// used to count only then, and a press before that went back to the
/// first favorite). None while an alarm's fallback sound plays: that is
/// no station.
fn step_from(state: &AppSnapshot) -> Option<String> {
    state.station_url.clone().filter(|_| !state.alarm_beep)
}

/// Wire what the favorites list and the header's star do: play, step,
/// star, edit, remove, move and reorder, and reset a row's figures
pub(crate) fn setup(ui: &App, ctx: &AppContext) {
    install_find(ui);
    crate::stream_edit::install(ui);
    // A save that failed, for the header to show until one works
    {
        let ui_weak = ui.as_weak();
        SaveErrors::shared().show_with(move |text| {
            let _ = ui_weak.upgrade_in_event_loop(move |ui| ui.set_save_error(text.into()));
        });
    }
    // toggle-favorite callback
    {
        let ctx = ctx.clone();
        ui.on_toggle_favorite(move || {
            let Some(ui) = ctx.ui.upgrade() else { return };
            let url = ui.get_station_url().to_string();
            if url.is_empty() {
                return;
            }
            // A stream without a name shows as "Radiotrope"; it is saved
            // under its address instead
            let name = lock(&ctx.state)
                .station_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| radiotrope_app::data::types::name_from_url(&url));
            let logo_url = ui.get_station_logo_url().to_string();
            let country = ui.get_station_country().to_string();

            let mut f = lock(&ctx.favorites);
            let mut removed = None;
            if f.is_favorite(&url) {
                let id = f.get_id_for_url(&url);
                removed = remove_keeping_place(&mut f, &id);
            } else {
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
                forget_undo_of(&ui, |removed| removed.url() == url);
            }
            let is_fav = f.is_favorite(&url);
            drop(f);
            save_favorites();

            ui.set_is_station_favorited(is_fav);
            refresh_favorites(&ui, &ctx.favorites, &ctx.logos);
            // One click removes; the offer under the list puts it back
            offer_undo(&ui, removed);
        });
    }

    // undo-remove-favorite callback: the favorite removed last goes back
    // to its place
    {
        let ctx = ctx.clone();
        ui.on_undo_remove_favorite(move || {
            let Some(ui) = ctx.ui.upgrade() else { return };
            ui.set_favorite_undo(0);
            let Some(removed) = REMOVED.take() else {
                return;
            };
            let url = removed.favorite.url().to_string();
            let back = put_back(&mut lock(&ctx.favorites), removed);
            if !back {
                return;
            }
            save_favorites();
            if ui.get_station_url() == url.as_str() {
                ui.set_is_station_favorited(true);
            }
            refresh_favorites(&ui, &ctx.favorites, &ctx.logos);
        });
    }

    // play-favorite callback
    {
        let ctx = ctx.clone();
        ui.on_play_favorite(move |station| {
            if let Some(ui) = ctx.ui.upgrade() {
                play_station_with_metadata(&ui, &ctx, play_of(&station));
            }
        });
    }

    // step-favorite callback: Next and Previous
    {
        let ctx = ctx.clone();
        ui.on_step_favorite(move |forward| {
            let Some(ui) = ctx.ui.upgrade() else { return };
            let current_url = step_from(&lock(&ctx.state));
            let favorites = ui.get_favorites_list();
            let index = shortcuts::favorite_step(
                favorites.iter().map(|station| station.url),
                current_url.as_deref(),
                forward,
            );
            if let Some(station) = index.and_then(|index| favorites.row_data(index)) {
                // While recording, the first press only arms the row
                if ui.invoke_confirm_switch(station.url.clone()) {
                    step_to_station(&ui, &ctx, play_of(&station));
                }
            }
        });
    }

    // reset-favorite-stats callback
    {
        let ctx = ctx.clone();
        ui.on_reset_favorite_stats(move |id| {
            let mut f = lock(&ctx.favorites);
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
            save_favorites();
            if let Some(map) = lock(&SESSION_LISTEN).as_mut() {
                map.remove(id.as_str());
            }
            if let Some(ui) = ctx.ui.upgrade() {
                update_favorite_stats(&ui, &f);
            }
        });
    }

    // edit-favorite callback
    {
        let ctx = ctx.clone();
        ui.on_edit_favorite(move |station| {
            let id = station.id.to_string();
            let name = station.name.to_string();
            let url = station.url.to_string();
            let logo_url = station.logo_url.to_string();
            let country = station.country.to_string();

            // The streams as the dialog lists them, and the default picked
            let urls: Vec<String> = station
                .streams
                .iter()
                .map(|stream| stream.url.to_string())
                .collect();

            // A failed update (another favorite has the new default) changes
            // nothing; the dialog stays open and shows why
            let mut f = lock(&ctx.favorites);
            let old_fav = f.get(&id).cloned();
            let Some(old) = &old_fav else {
                return FAVORITE_GONE.into();
            };
            let mut update = radiotrope_app::data::types::FavoriteUpdate::new()
                .name(name.clone())
                .streams(
                    crate::stream_edit::edited_streams(&old.station, &urls),
                    url.clone(),
                );
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
            if f.update(&id, update).is_err() {
                return if f.get_id_for_url(&url) != id && f.is_favorite(&url) {
                    "Another favorite already has this stream URL".into()
                } else {
                    FAVORITE_GONE.into()
                };
            }
            drop(f);
            save_favorites();

            if let Some(ui) = ctx.ui.upgrade() {
                let edited = EditedFavorite {
                    old: old_fav.as_ref(),
                    id: &id,
                    name: &name,
                    url: &url,
                    logo_url: &logo_url,
                    country: &country,
                };
                show_edited_favorite(&ui, &ctx.favorites, &ctx.logos, &ctx.state, edited);
            }
            slint::SharedString::new()
        });
    }

    // delete-favorite callback
    {
        let ctx = ctx.clone();
        ui.on_delete_favorite(move |station| {
            let id = station.id.to_string();
            let mut f = lock(&ctx.favorites);
            let removed = remove_keeping_place(&mut f, &id);
            drop(f);
            save_favorites();
            invalidate_logo_image(&id);

            if let Some(ui) = ctx.ui.upgrade() {
                let current_url = ui.get_station_url().to_string();
                if station.url == current_url {
                    ui.set_is_station_favorited(false);
                }
                refresh_favorites(&ui, &ctx.favorites, &ctx.logos);
                offer_undo(&ui, removed);
            }
        });
    }

    // move-favorite callback: Move to Top / Move to Bottom from a row's menu
    {
        let ctx = ctx.clone();
        ui.on_move_favorite(move |station, to_top| {
            {
                let mut f = lock(&ctx.favorites);
                if f.move_to_edge(&station.id, to_top).is_err() {
                    return;
                }
            }
            save_favorites();
            if let Some(ui) = ctx.ui.upgrade() {
                refresh_favorites(&ui, &ctx.favorites, &ctx.logos);
            }
        });
    }

    // reorder-favorites callback
    {
        let ctx = ctx.clone();
        ui.on_reorder_favorites(move |from, to| {
            let from = from as usize;
            let to = to as usize;

            // Update data model
            {
                let mut f = lock(&ctx.favorites);
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
            if let Some(ui) = ctx.ui.upgrade() {
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

            save_favorites();
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
static SESSION_LISTEN: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

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

/// Why Edit Favorite can't save a favorite that is no longer there
const FAVORITE_GONE: &str = "Can't save: this favorite was removed. Close and try again.";

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(favorites: &FavoritesManager) -> Vec<String> {
        favorites
            .sorted(FavoriteSort::Manual)
            .iter()
            .map(|f| f.name().to_string())
            .collect()
    }

    fn four_favorites() -> FavoritesManager {
        let mut favorites = FavoritesManager::new();
        for name in ["Jazz", "News", "Rock", "Talk"] {
            favorites
                .add(Favorite::new(name, format!("http://{name}.test")))
                .unwrap();
        }
        favorites
    }

    fn id_of(favorites: &FavoritesManager, name: &str) -> String {
        favorites.get_id_for_url(&format!("http://{name}.test"))
    }

    #[test]
    fn undo_puts_a_removed_favorite_back_in_its_place() {
        for (name, at) in [("Jazz", 0), ("News", 1), ("Talk", 3)] {
            let mut favorites = four_favorites();
            let id = id_of(&favorites, name);
            let removed = remove_keeping_place(&mut favorites, &id).unwrap();
            assert_eq!(removed.index, at);
            assert_eq!(names(&favorites).len(), 3);
            assert!(put_back(&mut favorites, removed));
            assert_eq!(names(&favorites), ["Jazz", "News", "Rock", "Talk"]);
        }
    }

    #[test]
    fn undo_after_the_list_changed() {
        // The list got shorter than the old place: it goes to the end
        let mut favorites = four_favorites();
        let talk = id_of(&favorites, "Talk");
        let removed = remove_keeping_place(&mut favorites, &talk).unwrap();
        let jazz = id_of(&favorites, "Jazz");
        favorites.remove(&jazz).unwrap();
        assert!(put_back(&mut favorites, removed));
        assert_eq!(names(&favorites), ["News", "Rock", "Talk"]);

        // Added again meanwhile (the star, an agent): nothing to put back
        let mut favorites = four_favorites();
        let news = id_of(&favorites, "News");
        let removed = remove_keeping_place(&mut favorites, &news).unwrap();
        favorites
            .add(Favorite::new("News", "http://News.test"))
            .unwrap();
        assert!(!put_back(&mut favorites, removed));
        assert_eq!(names(&favorites), ["Jazz", "Rock", "Talk", "News"]);

        // Not a favorite: nothing is removed
        assert!(remove_keeping_place(&mut favorites, "no-such-id").is_none());
    }

    #[test]
    fn next_and_previous_step_from_the_station_in_the_player() {
        use radiotrope::audio::PlaybackState;
        let urls = ["http://a.test", "http://b.test", "http://c.test"];
        let next = |state: &AppSnapshot| {
            shortcuts::favorite_step(urls.into_iter(), step_from(state).as_deref(), true)
        };
        // Still connecting to the second: Next is the third, not the first
        let mut state = AppSnapshot {
            station_url: Some(urls[1].into()),
            is_resolving: true,
            ..Default::default()
        };
        assert_eq!(next(&state), Some(2));
        // The same once it plays, and when it failed or was stopped
        state.is_resolving = false;
        state.playback = PlaybackState::Playing;
        assert_eq!(next(&state), Some(2));
        state.playback = PlaybackState::Stopped;
        state.is_error = true;
        assert_eq!(next(&state), Some(2));
        // An alarm's fallback sound is no station: the first favorite
        state.alarm_beep = true;
        assert_eq!(next(&state), Some(0));
        // Nothing in the player yet
        assert_eq!(next(&AppSnapshot::default()), Some(0));
    }

    #[test]
    fn find_matches_names_whatever_the_case() {
        let needle = find_needle("  RaD ");
        assert_eq!(needle, "rad");
        assert!(find_matches("Sundial Radio", &needle));
        assert!(find_matches("radio verde", &needle));
        assert!(!find_matches("Harbour Jazz", &needle));
        assert!(find_matches("Küste Kultur", &find_needle("KÜST")));
        // Nothing typed (or only spaces) finds everything
        assert!(find_matches("Harbour Jazz", &find_needle("  ")));
    }

    #[test]
    fn find_narrows_the_rows_and_says_where_they_are() {
        let list = std::rc::Rc::new(VecModel::from(
            ["Harbour Jazz", "Radio Verde", "Sundial Radio"]
                .iter()
                .map(|name| FavoriteStation {
                    name: (*name).into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        ));
        let stations: ModelRc<FavoriteStation> = list.clone().into();
        let shown = |text: &str| -> Vec<String> {
            found_stations(stations.clone(), text)
                .iter()
                .map(|station| station.name.to_string())
                .collect()
        };
        let rows = |text: &str| -> Vec<i32> { found_rows(&stations, text).iter().collect() };

        assert_eq!(shown("radio"), ["Radio Verde", "Sundial Radio"]);
        assert_eq!(rows("radio"), [1, 2]);
        assert_eq!(shown("JAZZ"), ["Harbour Jazz"]);
        assert_eq!(rows("JAZZ"), [0]);
        assert!(shown("no such station").is_empty());
        assert!(rows("no such station").is_empty());
        // Nothing typed: the whole list, in its order
        assert_eq!(shown(" ").len(), 3);
        assert!(rows("").is_empty());

        // A found row follows the row it comes from (its figures change
        // while it plays)
        let found = found_stations(stations.clone(), "verde");
        let mut row = list.row_data(1).unwrap();
        row.listen_time = "5 min".into();
        list.set_row_data(1, row);
        assert_eq!(found.row_data(0).unwrap().listen_time, "5 min");
    }

    /// A folder of this test's own
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radiotrope-favorites-ui-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn saved_names(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = FavoritesManager::load_from(path)
            .unwrap()
            .sorted(FavoriteSort::Manual)
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_failed_save_sets_the_error_and_is_retried_until_it_works() {
        let dir = test_dir("retry");
        // Where the favorites folder should be there is a file: nothing
        // can be written under it
        let folder = dir.join("config");
        fs::write(&folder, "in the way").unwrap();
        let path = folder.join("favorites.json");

        let favorites = Arc::new(Mutex::new(FavoritesManager::new()));
        let errors = SaveErrors::default();
        let saver = saver_of(
            favorites.clone(),
            Some(path.clone()),
            errors.clone(),
            Duration::from_millis(50),
        );
        lock(&favorites)
            .add(Favorite::new("Jazz", "http://jazz.test"))
            .unwrap();
        saver.request_now();

        let waited = Instant::now();
        while errors.text().is_empty() && waited.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let shown = errors.text();
        assert!(shown.starts_with("Can't save favorites: "), "{shown}");
        assert!(!shown.contains("os error"), "{shown}");
        assert!(lock(&favorites).is_dirty(), "still to be saved");

        // The folder can be written again: the retry saves, asked by nobody
        fs::remove_file(&folder).unwrap();
        fs::create_dir_all(&folder).unwrap();
        let waited = Instant::now();
        while !errors.text().is_empty() && waited.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(errors.text(), "");
        assert!(!lock(&favorites).is_dirty());
        assert_eq!(saved_names(&path), ["Jazz"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_favorites_are_free_while_the_file_lock_is_waited_for() {
        let dir = test_dir("unheld");
        let path = dir.join("favorites.json");
        let favorites = Arc::new(Mutex::new(FavoritesManager::new()));
        lock(&favorites)
            .add(Favorite::new("Jazz", "http://jazz.test"))
            .unwrap();
        save_unheld(&favorites, Some(&path)).unwrap();

        // Another player is saving: it holds the folder's lock
        let other = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut other = FavoritesManager::load_from(&path).unwrap();
                other
                    .add(Favorite::new("News", "http://news.test"))
                    .unwrap();
                other.save_to(&path).unwrap();
            }
        });
        other.join().unwrap();

        lock(&favorites)
            .add(Favorite::new("Rock", "http://rock.test"))
            .unwrap();
        let saving = std::thread::spawn({
            let favorites = favorites.clone();
            let path = path.clone();
            move || save_unheld(&favorites, Some(&path))
        });
        // The window can still take the favorites while that runs
        let waited = Instant::now();
        while !saving.is_finished() {
            drop(favorites.try_lock().ok());
            assert!(waited.elapsed() < Duration::from_secs(10));
            std::thread::yield_now();
        }
        assert_eq!(saving.join().unwrap(), Ok(Saved::All));
        assert_eq!(saved_names(&path), ["Jazz", "News", "Rock"]);
        // What the other player saved is here too
        assert_eq!(lock(&favorites).sorted(FavoriteSort::Manual).len(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_reason_is_in_plain_words() {
        let e: radiotrope_app::error::AppError =
            std::io::Error::other("Permission denied (os error 13)").into();
        assert_eq!(save_reason(&e), "permission denied");
        let e = radiotrope_app::error::AppError::Config(
            "Timed out waiting for the lock (os error 11) on x".into(),
        );
        assert_eq!(save_reason(&e), "timed out waiting for the lock on x");
    }
}
