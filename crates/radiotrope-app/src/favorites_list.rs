//! The favorites list on screen: its rows, their logos and their listening
//! figures

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::types::FavoriteSort;
use radiotrope_app::network::logo::LogoService;
use radiotrope_app::text;

use crate::browse_ui::mark_browse_favorites;
use crate::context::lock;
use crate::favorites_ui::session_listen_secs;
use crate::player_ui::logo_image;
use crate::{App, FavoriteStation};

pub(crate) fn favorite_to_slint(
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

/// Refresh the stats text of every favorites row in place, without
/// rebuilding the list (which would interrupt a drag)
pub(crate) fn update_favorite_stats(ui: &App, favs: &FavoritesManager) {
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
pub(crate) fn format_listen_time(secs: u64) -> String {
    text::minutes(secs)
}

/// When a favorite was last played, relative to now: "Just now",
/// "12 min ago", "3 h ago", "Yesterday", "5 days ago", "2 weeks ago",
/// "4 months ago", or empty when never played
pub(crate) fn format_last_played(timestamp: Option<u64>) -> String {
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

/// A favorite's decoded logo, and when the file it was decoded from was
/// written: a newer file (a logo downloaded again) is decoded again
pub(crate) type DecodedLogo = (slint::Image, Option<std::time::SystemTime>);

// In-memory cache of decoded slint::Image keyed by favorite ID.
// All callers run on the Slint event-loop thread, so thread_local is safe and avoids
// threading slint::Image (which is !Send) through closures.
thread_local! {
    pub(crate) static LOGO_IMAGE_CACHE: std::cell::RefCell<HashMap<String, DecodedLogo>> =
        std::cell::RefCell::new(HashMap::new());
}

thread_local! {
    /// The favorites generation the list on screen shows. A change made
    /// elsewhere (an agent) shows as a newer one, which the poll redraws.
    pub(crate) static FAVORITES_SHOWN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The favorites on screen are up to date with `favs`
pub(crate) fn note_favorites_shown(favs: &FavoritesManager) {
    FAVORITES_SHOWN.set(favs.generation());
}

/// Remove a cached logo image, forcing re-decode on next refresh.
pub(crate) fn invalidate_logo_image(id: &str) {
    LOGO_IMAGE_CACHE.with(|cache| {
        cache.borrow_mut().remove(id);
    });
}

/// Redraw the favorites list, and start fetching the logos it lacks
pub(crate) fn refresh_favorites(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_time_says_min() {
        assert_eq!(format_listen_time(40 * 60), "40 min");
        assert_eq!(format_listen_time(3 * 3600 + 25 * 60), "3 h 25 min");
    }
}
