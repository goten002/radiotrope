//! Upkeep of the disk caches: logo files named by old ids, and the daily trim

use std::sync::{Arc, Mutex};

use radiotrope_app::data::cache::trim_dir_to_size;
use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::types::{FavoriteSort, Station};
use radiotrope_app::network::browse_logos::BrowseLogos;
use radiotrope_app::network::logo::LogoService;

use crate::app::state::AppSnapshot;
use crate::context::lock;

/// The file in the logo cache folder that says the logos have their
/// current names (see [`migrate_logo_ids`])
const IDS_MIGRATED: &str = "ids-migrated";

/// The most the cached API responses may take on disk: past it, the
/// oldest are removed
const API_CACHE_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// Logos cached by builds before the stable favorite ids (FNV-1a) are named
/// by the old id. Give them the current name before anything looks them up,
/// and before the startup cleanup takes them for orphans and deletes them.
/// A rename per logo, once: a note in the cache folder then says it was
/// done, and later starts only look for that note (the search for old
/// names is several file checks per favorite). A cache that is cleared
/// loses the note with the logos, and has nothing to rename.
pub(crate) fn migrate_logo_ids(
    favorites: &Mutex<FavoritesManager>,
    last_station: Option<&Station>,
    logo_service: &LogoService,
) {
    use radiotrope_app::data::types::{legacy_url_to_id, url_to_id};
    let done = logo_service.cache().dir().join(IDS_MIGRATED);
    if done.exists() {
        return;
    }
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
    // With no favorites nothing was learned: they may not have loaded
    // (a damaged file, recovered later)
    if !urls.is_empty() {
        let _ = std::fs::write(done, "");
    }
}

/// Trim the disk caches on a thread of their own, now and then once a day:
/// API responses older than a month, browser logos not shown for 90 days,
/// the oldest of either past a size (50 MB of responses, 150 MB of
/// logos), and the logos of stations that are neither favorites nor the
/// one in the player. Those are kept when the favorites couldn't be read:
/// they may still be needed once the file is recovered.
///
/// From the second day on, what is remembered of the favorites' logo
/// files is read again first, in case another program changed them.
pub(crate) fn prune_caches_daily(
    favorites: Arc<Mutex<FavoritesManager>>,
    favorites_loaded: bool,
    shared_state: Arc<Mutex<AppSnapshot>>,
    logo_service: Arc<LogoService>,
    browse_logos: Arc<BrowseLogos>,
    logos_refreshed: impl Fn() + Send + 'static,
) {
    use radiotrope_app::config::caches::{ORPHAN_LOGO_MIN_AGE, PRUNE_EVERY};
    let prune = move |first: bool| {
        if let Some(cache) = radiotrope_app::network::ApiCache::open_default() {
            cache.prune(radiotrope_app::config::providers::API_CACHE_MAX_AGE);
            trim_dir_to_size(cache.dir(), API_CACHE_MAX_BYTES);
        }
        browse_logos.remove_unused();
        browse_logos.trim_to_size();
        if !favorites_loaded {
            return;
        }
        let favs: Vec<radiotrope_app::data::types::Favorite> =
            lock(&favorites).all().into_iter().cloned().collect();
        if !first {
            let ids = favs.iter().map(|f| f.id()).collect();
            logo_service.cache().reindex(&ids);
        }
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
        .spawn(move || {
            let mut first = true;
            loop {
                prune(first);
                first = false;
                std::thread::sleep(PRUNE_EVERY);
            }
        });
    if let Err(e) = spawned {
        eprintln!("Failed to start the cache prune thread: {e}");
    }
}
