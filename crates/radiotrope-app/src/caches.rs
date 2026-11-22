//! Upkeep of the disk caches: logo files named by old ids, and the daily trim

use std::sync::{Arc, Mutex};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::types::{FavoriteSort, Station};
use radiotrope_app::network::browse_logos::BrowseLogos;
use radiotrope_app::network::logo::LogoService;

use crate::app::state::AppSnapshot;
use crate::context::lock;

/// Logos cached by builds before the stable favorite ids (FNV-1a) are named
/// by the old id. Give them the current name before anything looks them up,
/// and before the startup cleanup takes them for orphans and deletes them.
/// A rename per logo, once; later starts find nothing to move.
pub(crate) fn migrate_logo_ids(
    favorites: &Mutex<FavoritesManager>,
    last_station: Option<&Station>,
    logo_service: &LogoService,
) {
    use radiotrope_app::data::types::{legacy_url_to_id, url_to_id};
    let urls: Vec<String> = {
        let favs = lock(favorites);
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
pub(crate) fn prune_caches_daily(
    favorites: Arc<Mutex<FavoritesManager>>,
    favorites_loaded: bool,
    shared_state: Arc<Mutex<AppSnapshot>>,
    logo_service: Arc<LogoService>,
    browse_logos: Arc<BrowseLogos>,
    logos_refreshed: impl Fn() + Send + 'static,
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
        let favs: Vec<radiotrope_app::data::types::Favorite> =
            lock(&favorites).all().into_iter().cloned().collect();
        // Download again the favorite logos that are out of date (a
        // month old, or from another URL); a failed one keeps the old
        let stale: Vec<_> = favs
            .iter()
            .filter(|f| logo_service.is_stale(*f))
            .cloned()
            .collect();
        if logo_service.prefetch(&stale) > 0 {
            logos_refreshed();
        }
        let mut valid: std::collections::HashSet<String> = favs.iter().map(|f| f.id()).collect();
        // The station in the player keeps its logo even when it isn't a
        // favorite
        let playing = lock(&shared_state).station_url.clone();
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
