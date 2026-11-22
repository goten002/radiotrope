//! What the favorites list and the header's star do, and the listening time
//! credited to the favorite playing

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use radiotrope::audio::PlaybackState;
use radiotrope_app::config::ui::{FAVORITES_SAVE_DELAY, LISTEN_CREDIT_SECS, MIN_LISTEN_SECS};
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::types::{FavoriteSort, Station};
use radiotrope_app::network::logo::LogoService;

use crate::app::delayed_save::{DelayedSave, SaveErrors, Saved, SAVE_RETRY};
use crate::app::listening::ListenSession;
use crate::app::state::AppSnapshot;
use crate::context::{lock, AppContext};
use crate::favorites_list::{
    invalidate_logo_image, note_favorites_shown, refresh_favorites, update_favorite_stats,
};
use crate::player_ui::{fetch_station_logo, play_station_with_metadata, SHOWN_STATION};
use crate::window_settings::save_reason;
use crate::{shortcuts, App, FavoriteStation};

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

/// Wire what the favorites list and the header's star do: play, step,
/// star, edit, remove, move and reorder, and reset a row's figures
pub(crate) fn setup(ui: &App, ctx: &AppContext) {
    let shared_state = &ctx.state;
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
            let is_fav = f.is_favorite(&url);
            drop(f);
            save_favorites();

            ui.set_is_station_favorited(is_fav);
            refresh_favorites(&ui, &ctx.favorites, &ctx.logos);
        });
    }

    // play-favorite callback
    {
        let ctx = ctx.clone();
        ui.on_play_favorite(move |station| {
            if let Some(ui) = ctx.ui.upgrade() {
                let logo_url = station.logo_url.to_string();
                let country = station.country.to_string();
                play_station_with_metadata(
                    &ui,
                    &ctx,
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
            let mut f = lock(&ctx.favorites);
            let old_fav = f.get(&id).cloned();
            if f.update(&id, update).is_err() {
                return if f.get_id_for_url(&url) != id && f.is_favorite(&url) {
                    "Another favorite already has this stream URL".into()
                } else {
                    // The only other failure: the favorite is gone
                    "Can't save: this favorite was removed. Close and try again.".into()
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
            let _ = f.remove(&id);
            drop(f);
            save_favorites();
            invalidate_logo_image(&id);

            if let Some(ui) = ctx.ui.upgrade() {
                let current_url = ui.get_station_url().to_string();
                if station.url == current_url {
                    ui.set_is_station_favorited(false);
                }
                refresh_favorites(&ui, &ctx.favorites, &ctx.logos);
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
    use radiotrope_app::data::types::Favorite;
    use std::fs;

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
