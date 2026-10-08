//! Favorites management
//!
//! In-memory management of favorite stations. Another radiotrope process
//! (an agent's own player) may save the same file: what it saved is taken
//! in before each change and save here (see
//! [`FavoritesManager::reload_if_changed`]).
//!
//! Plays and listening time are kept in stats.json next to favorites.json
//! ([`StationStats`]), so favorites.json changes only when a favorite does.

use crate::data::storage::{self, FileStamp};
use crate::data::types::{
    url_to_id, Favorite, FavoriteFilter, FavoriteSort, FavoriteUpdate, StationStats,
};
use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Favorites data file name
const FAVORITES_FILE: &str = "favorites.json";

/// Favorites file format version for migrations. 2: plays and listening
/// time moved to [`STATS_FILE`]
const FAVORITES_VERSION: u32 = 2;

/// Plays and listening time, in the favorites file's folder
pub const STATS_FILE: &str = "stats.json";

/// Stats file format version
const STATS_VERSION: u32 = 1;

/// How long the stats of a station that is no longer a favorite are kept
/// after it was last played, so adding it back brings them back
const ORPHAN_STATS_SECS: u64 = 90 * 24 * 60 * 60;

/// Metadata describing a station to play (see [`FavoritesManager::resolve_play`])
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayMetadata {
    pub url: String,
    pub name: Option<String>,
    pub logo_url: Option<String>,
    pub country: Option<String>,
    /// Provider-specific station ID (e.g. radio-browser's stationuuid)
    pub provider_id: Option<String>,
}

/// The favorites as they were before the last list taken in, in the
/// favorites file's folder, for [`FavoritesManager::undo_import`]
pub const BEFORE_IMPORT_FILE: &str = "favorites.before-import.json";

/// How a list of favorites from elsewhere (another player, a file) is
/// taken in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Add the stations not here yet, at the end; the ones here stay as
    /// they are
    Add,
    /// Become the list: its stations, details and order
    Replace,
}

/// What taking in a list did, or would do
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ImportOutcome {
    /// Stations new here
    pub added: usize,
    /// Stations here already whose details change (Replace)
    pub updated: usize,
    /// Stations no longer favorites (Replace)
    pub removed: usize,
    /// Stations here already and left as they are
    pub kept: usize,
    /// Stations not added because the favorites are full (Add)
    pub over_cap: usize,
    /// Whether the stations both lists have change order (Replace)
    pub reordered: bool,
}

impl ImportOutcome {
    /// Whether taking the list in changes anything
    pub fn changes(&self) -> bool {
        self.added > 0 || self.updated > 0 || self.removed > 0 || self.reordered
    }
}

/// Favorites file structure
#[derive(Debug, Serialize, Deserialize)]
struct FavoritesFile {
    version: u32,
    favorites: Vec<Favorite>,
}

/// [`FavoritesFile`] as read back: each favorite is parsed on its own, so
/// one damaged entry doesn't cost all the others
#[derive(Deserialize)]
struct StoredFavoritesFile {
    favorites: Vec<serde_json::Value>,
}

/// The stats file: plays and listening time by favorite id
#[derive(Debug, Serialize, Deserialize)]
struct StatsFile {
    version: u32,
    #[serde(default)]
    stations: HashMap<String, StationStats>,
}

impl Default for FavoritesFile {
    fn default() -> Self {
        Self {
            version: FAVORITES_VERSION,
            favorites: Vec::new(),
        }
    }
}

/// Manages favorites in memory
///
/// Uses URL hash as ID, so lookups by URL are O(1).
pub struct FavoritesManager {
    /// All favorites by ID (which is derived from URL hash)
    favorites: HashMap<String, Favorite>,
    /// Whether there are unsaved changes
    dirty: bool,
    /// Monotonically increasing generation counter, bumped on every mutation
    generation: u64,
    /// The file the favorites were loaded from or last saved to
    path: Option<PathBuf>,
    /// That file's stamp when this manager last read or wrote it
    seen: Option<FileStamp>,
    /// The favorites as that read or write left them: what differs from
    /// them now are the changes made here
    base: HashMap<String, Favorite>,
    /// Plays and listening time by favorite id, kept for a while after a
    /// favorite is removed
    stats: HashMap<String, StationStats>,
    /// Whether the stats have unsaved changes
    stats_dirty: bool,
    /// The stats file's stamp when this manager last read or wrote it
    stats_seen: Option<FileStamp>,
    /// The stats as that read or write left them
    stats_base: HashMap<String, StationStats>,
}

impl FavoritesManager {
    /// Create a new empty manager
    pub fn new() -> Self {
        Self {
            favorites: HashMap::new(),
            dirty: false,
            generation: 0,
            path: None,
            seen: None,
            base: HashMap::new(),
            stats: HashMap::new(),
            stats_dirty: false,
            stats_seen: None,
            stats_base: HashMap::new(),
        }
    }

    /// Current generation counter (incremented on every mutation)
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Load favorites from default storage location
    pub fn load() -> Result<Self> {
        let path = storage::data_path(FAVORITES_FILE)?;
        Self::load_from(&path)
    }

    /// Load favorites from a specific path
    pub fn load_from(path: &Path) -> Result<Self> {
        let mut manager = Self::new();
        // Taken before the read, so a save in between shows as a change
        let seen = storage::stamp(path);

        let mut old_stats = HashMap::new();
        if let Some(file) = storage::load_from::<StoredFavoritesFile>(path)? {
            (manager.favorites, old_stats) = read_entries(file, path);
        }

        manager.path = Some(path.to_path_buf());
        manager.seen = seen;
        manager.base = manager.favorites.clone();
        manager.dirty = false;

        let stats_path = stats_path_for(path);
        manager.stats_seen = storage::stamp(&stats_path);
        match storage::load_from::<StatsFile>(&stats_path) {
            Ok(Some(file)) => manager.stats = file.stations,
            // First run since the stats moved out of favorites.json: they
            // are written to the stats file with the next save
            Ok(None) => {
                manager.stats = old_stats;
                manager.stats_dirty = !manager.stats.is_empty();
            }
            Err(e) => eprintln!("Stats: {e}"),
        }
        if !manager.stats_dirty {
            manager.stats_base = manager.stats.clone();
        }
        Ok(manager)
    }

    /// Where the stats are kept, next to the favorites file
    fn stats_path(&self) -> Option<PathBuf> {
        self.path.as_deref().map(stats_path_for)
    }

    /// Take in what another radiotrope process saved to the file since this
    /// manager last read or wrote it. A favorite changed here since then
    /// keeps the change made here (it is saved next); every other favorite
    /// becomes the other process's, removals included. Returns whether the
    /// favorites changed.
    ///
    /// A missing or damaged file is no news: the next save writes it anew.
    pub fn reload_if_changed(&mut self) -> bool {
        let stats = self.reload_stats_if_changed();
        self.reload_favorites_if_changed() || stats
    }

    fn reload_favorites_if_changed(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let stamp = storage::stamp(&path);
        if stamp.is_none() || stamp == self.seen {
            return false;
        }
        // Before the read, as at load
        self.seen = stamp;
        let theirs = match storage::parse_file::<StoredFavoritesFile>(&path) {
            Ok(Some(file)) => read_entries(file, &path).0,
            Ok(None) => return false,
            Err(e) => {
                eprintln!("Favorites: {e}");
                return false;
            }
        };

        // The changes made here, on top of what the other process saved
        let mut merged = theirs.clone();
        for id in self.favorites.keys().chain(self.base.keys()) {
            let ours = self.favorites.get(id);
            if ours != self.base.get(id) {
                match ours {
                    Some(favorite) => merged.insert(id.clone(), favorite.clone()),
                    None => merged.remove(id),
                };
            }
        }
        self.base = theirs;
        if merged == self.favorites {
            return false;
        }
        self.favorites = merged;
        self.generation += 1;
        true
    }

    /// [`reload_if_changed`](Self::reload_if_changed) for the stats: the
    /// stations whose stats changed here keep them, the others take what
    /// the other process saved
    fn reload_stats_if_changed(&mut self) -> bool {
        let Some(path) = self.stats_path() else {
            return false;
        };
        let stamp = storage::stamp(&path);
        if stamp.is_none() || stamp == self.stats_seen {
            return false;
        }
        self.stats_seen = stamp;
        let theirs = match storage::parse_file::<StatsFile>(&path) {
            Ok(Some(file)) => file.stations,
            Ok(None) => return false,
            Err(e) => {
                eprintln!("Stats: {e}");
                return false;
            }
        };
        let mut merged = theirs.clone();
        for id in self.stats.keys().chain(self.stats_base.keys()) {
            let ours = self.stats.get(id);
            if ours != self.stats_base.get(id) {
                match ours {
                    Some(stats) => merged.insert(id.clone(), *stats),
                    None => merged.remove(id),
                };
            }
        }
        self.stats_base = theirs;
        if merged == self.stats {
            return false;
        }
        self.stats = merged;
        true
    }

    /// Save favorites to default storage location
    pub fn save(&mut self) -> Result<()> {
        let path = storage::data_path(FAVORITES_FILE)?;
        self.save_to(&path)
    }

    /// Save favorites to a specific path
    ///
    /// What another process saved there since is kept too (see
    /// [`reload_if_changed`](Self::reload_if_changed)).
    pub fn save_to(&mut self, path: &Path) -> Result<()> {
        if !self.dirty && !self.stats_dirty {
            return Ok(());
        }
        if self.path.as_deref() != Some(path) {
            // A file this manager hasn't read: the favorites already in it
            // stay, next to the ones here (stats too)
            self.path = Some(path.to_path_buf());
            self.seen = None;
            self.base.clear();
            self.stats_seen = None;
            self.stats_base.clear();
        }
        self.reload_if_changed();

        if self.dirty {
            let file = FavoritesFile {
                version: FAVORITES_VERSION,
                favorites: self.favorites.values().cloned().collect(),
            };
            self.seen = Some(storage::save_to_stamped(path, &file)?);
            self.base = self.favorites.clone();
            self.dirty = false;
        }

        if self.stats_dirty {
            self.drop_old_orphan_stats();
            let file = StatsFile {
                version: STATS_VERSION,
                stations: self.stats.clone(),
            };
            self.stats_seen = Some(storage::save_to_stamped(&stats_path_for(path), &file)?);
            self.stats_base = self.stats.clone();
            self.stats_dirty = false;
        }
        Ok(())
    }

    /// Forget the stats of stations that are no longer favorites and
    /// weren't played for [`ORPHAN_STATS_SECS`]
    fn drop_old_orphan_stats(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let favorites = &self.favorites;
        self.stats.retain(|id, stats| {
            favorites.contains_key(id)
                || stats
                    .last_played
                    .is_some_and(|t| now.saturating_sub(t) < ORPHAN_STATS_SECS)
        });
    }

    /// Force save to default location (ignore dirty flag)
    pub fn force_save(&mut self) -> Result<()> {
        self.dirty = true;
        self.save()
    }

    /// Force save to a specific path (ignore dirty flag)
    pub fn force_save_to(&mut self, path: &Path) -> Result<()> {
        self.dirty = true;
        self.save_to(path)
    }

    /// Check if there are unsaved changes
    pub fn is_dirty(&self) -> bool {
        self.dirty || self.stats_dirty
    }

    /// The plays and listening time of the favorite with this id (all zero
    /// when it was never played)
    pub fn stats(&self, id: &str) -> StationStats {
        self.stats.get(id).copied().unwrap_or_default()
    }

    /// Add a new favorite
    pub fn add(&mut self, favorite: Favorite) -> Result<()> {
        self.reload_if_changed();
        let id = favorite.id();
        // Check for duplicate (ID is derived from URL, so same URL = same ID)
        if self.favorites.contains_key(&id) {
            return Err(AppError::Config(format!(
                "A favorite with URL '{}' already exists",
                favorite.url()
            )));
        }

        // Place at end of list: sort_order = max + 1
        let max_order = self
            .favorites
            .values()
            .map(|f| f.sort_order)
            .max()
            .unwrap_or(-1);
        let mut favorite = favorite;
        favorite.sort_order = max_order + 1;

        self.favorites.insert(id, favorite);
        self.dirty = true;
        self.generation += 1;
        Ok(())
    }

    /// Remove a favorite by ID
    pub fn remove(&mut self, id: &str) -> Result<Favorite> {
        self.reload_if_changed();
        let favorite = self
            .favorites
            .remove(id)
            .ok_or_else(|| AppError::Config(format!("Favorite with ID '{}' not found", id)))?;

        self.dirty = true;
        self.generation += 1;
        Ok(favorite)
    }

    /// Remove a favorite by URL
    pub fn remove_by_url(&mut self, url: &str) -> Result<Favorite> {
        let id = url_to_id(url);
        self.remove(&id)
    }

    /// Get a favorite by ID
    pub fn get(&self, id: &str) -> Option<&Favorite> {
        self.favorites.get(id)
    }

    /// Get a mutable favorite by ID
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Favorite> {
        self.reload_if_changed();
        self.dirty = true; // Assume modification
        self.generation += 1;
        self.favorites.get_mut(id)
    }

    /// Get a favorite by URL (O(1) - just compute hash)
    pub fn get_by_url(&self, url: &str) -> Option<&Favorite> {
        let id = url_to_id(url);
        self.favorites.get(&id)
    }

    /// Find the favorite for a station about to be played
    ///
    /// Matches the stream URL first, then the provider's station ID, since
    /// providers may return a different (e.g. resolved) URL for the same
    /// station than the one saved in favorites.
    pub fn find_match(&self, url: &str, provider_id: Option<&str>) -> Option<&Favorite> {
        self.get_by_url(url).or_else(|| {
            let provider_id = provider_id.filter(|id| !id.is_empty())?;
            self.favorites
                .values()
                .find(|f| f.station.provider_id.as_deref() == Some(provider_id))
        })
    }

    /// Resolve the metadata for playing a station, favorites taking priority
    ///
    /// When the station is a favorite (see [`find_match`](Self::find_match)),
    /// its URL, name, logo and country are used, and the given values only
    /// fill in what the favorite lacks.
    pub fn resolve_play(&self, request: PlayMetadata) -> PlayMetadata {
        let Some(fav) = self.find_match(&request.url, request.provider_id.as_deref()) else {
            return request;
        };
        PlayMetadata {
            url: fav.url().to_string(),
            name: Some(fav.name().to_string()),
            logo_url: fav.station.logo_url.clone().or(request.logo_url),
            country: fav.station.country.clone().or(request.country),
            provider_id: fav.station.provider_id.clone().or(request.provider_id),
        }
    }

    /// Check if a URL is favorited (O(1))
    pub fn is_favorite(&self, url: &str) -> bool {
        let id = url_to_id(url);
        self.favorites.contains_key(&id)
    }

    /// Get ID for a URL (deterministic - just computes hash)
    pub fn get_id_for_url(&self, url: &str) -> String {
        url_to_id(url)
    }

    /// Update a favorite
    ///
    /// Note: Changing the URL will change the ID, effectively creating
    /// a new favorite. Use with caution.
    pub fn update(&mut self, id: &str, update: FavoriteUpdate) -> Result<()> {
        self.reload_if_changed();
        // If URL is changing, we need to re-key the favorite
        if let Some(ref new_url) = update.url {
            let new_id = url_to_id(new_url);

            // Check new URL doesn't conflict (unless it's the same)
            if new_id != id && self.favorites.contains_key(&new_id) {
                return Err(AppError::Config(format!(
                    "A favorite with URL '{}' already exists",
                    new_url
                )));
            }

            // Remove old, update, insert with new key
            let mut favorite = self
                .favorites
                .remove(id)
                .ok_or_else(|| AppError::Config(format!("Favorite with ID '{}' not found", id)))?;

            update.apply_to(&mut favorite);
            // ID is computed from URL, so after applying update with new URL,
            // favorite.id() will return the new_id
            self.favorites.insert(favorite.id(), favorite);
            // The stats follow the favorite to its new id
            if new_id != id {
                if let Some(stats) = self.stats.remove(id) {
                    self.stats.insert(new_id, stats);
                    self.stats_dirty = true;
                }
            }
        } else {
            // No URL change, just update in place
            let favorite = self
                .favorites
                .get_mut(id)
                .ok_or_else(|| AppError::Config(format!("Favorite with ID '{}' not found", id)))?;
            update.apply_to(favorite);
        }

        self.dirty = true;
        self.generation += 1;
        Ok(())
    }

    /// Toggle favorite status for a URL
    /// Returns Some(id) if added, None if removed
    pub fn toggle(
        &mut self,
        name: &str,
        url: &str,
        logo_url: Option<&str>,
    ) -> Result<Option<String>> {
        self.reload_if_changed();
        let id = url_to_id(url);

        if self.favorites.contains_key(&id) {
            self.remove(&id)?;
            Ok(None)
        } else {
            let mut favorite = Favorite::new(name, url);
            if let Some(logo) = logo_url {
                favorite = favorite.with_logo(logo);
            }
            let id = favorite.id();
            self.add(favorite)?;
            Ok(Some(id))
        }
    }

    /// Get all favorites
    pub fn all(&self) -> Vec<&Favorite> {
        self.favorites.values().collect()
    }

    /// Get all favorites sorted
    pub fn sorted(&self, sort: FavoriteSort) -> Vec<&Favorite> {
        let mut favorites: Vec<_> = self.favorites.values().collect();

        match sort {
            FavoriteSort::Manual => {
                favorites.sort_by_key(|f| f.sort_order);
            }
            FavoriteSort::Name => {
                favorites.sort_by_key(|f| f.name().to_lowercase());
            }
            FavoriteSort::RecentlyAdded => {
                favorites.sort_by_key(|f| std::cmp::Reverse(f.added_at));
            }
            FavoriteSort::RecentlyPlayed => {
                favorites.sort_by_key(|f| std::cmp::Reverse(self.stats(&f.id()).last_played));
            }
            FavoriteSort::MostPlayed => {
                favorites.sort_by_key(|f| std::cmp::Reverse(self.stats(&f.id()).play_count));
            }
            FavoriteSort::MostListened => {
                favorites
                    .sort_by_key(|f| std::cmp::Reverse(self.stats(&f.id()).total_listen_time_secs));
            }
        }

        favorites
    }

    /// Get filtered favorites
    pub fn filtered(&self, filter: &FavoriteFilter) -> Vec<&Favorite> {
        self.favorites
            .values()
            .filter(|f| filter.matches(f))
            .collect()
    }

    /// Get filtered and sorted favorites
    pub fn query(&self, filter: &FavoriteFilter, sort: FavoriteSort) -> Vec<&Favorite> {
        let mut favorites: Vec<_> = self.filtered(filter);

        match sort {
            FavoriteSort::Manual => {
                favorites.sort_by_key(|f| f.sort_order);
            }
            FavoriteSort::Name => {
                favorites.sort_by_key(|f| f.name().to_lowercase());
            }
            FavoriteSort::RecentlyAdded => {
                favorites.sort_by_key(|f| std::cmp::Reverse(f.added_at));
            }
            FavoriteSort::RecentlyPlayed => {
                favorites.sort_by_key(|f| std::cmp::Reverse(self.stats(&f.id()).last_played));
            }
            FavoriteSort::MostPlayed => {
                favorites.sort_by_key(|f| std::cmp::Reverse(self.stats(&f.id()).play_count));
            }
            FavoriteSort::MostListened => {
                favorites
                    .sort_by_key(|f| std::cmp::Reverse(self.stats(&f.id()).total_listen_time_secs));
            }
        }

        favorites
    }

    /// Get number of favorites
    pub fn count(&self) -> usize {
        self.favorites.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.favorites.is_empty()
    }

    /// Get all unique genres across all favorites
    pub fn all_genres(&self) -> Vec<String> {
        let mut genres: Vec<_> = self
            .favorites
            .values()
            .flat_map(|f| f.station.genres.iter().cloned())
            .collect();
        genres.sort();
        genres.dedup();
        genres
    }

    /// Get all unique providers
    pub fn all_providers(&self) -> Vec<String> {
        let mut providers: Vec<_> = self
            .favorites
            .values()
            .map(|f| f.station.provider.clone())
            .collect();
        providers.sort();
        providers.dedup();
        providers
    }

    /// Get all unique countries
    pub fn all_countries(&self) -> Vec<String> {
        let mut countries: Vec<_> = self
            .favorites
            .values()
            .filter_map(|f| f.station.country.clone())
            .collect();
        countries.sort();
        countries.dedup();
        countries
    }

    /// Record a play session for a favorite
    pub fn record_play(&mut self, id: &str, duration_secs: u64) -> Result<()> {
        self.reload_if_changed();
        if !self.favorites.contains_key(id) {
            return Err(AppError::Config(format!(
                "Favorite with ID '{}' not found",
                id
            )));
        }
        self.stats
            .entry(id.to_string())
            .or_default()
            .record_play(duration_secs);
        self.stats_dirty = true;
        self.generation += 1;
        Ok(())
    }

    /// Record a play session by URL
    pub fn record_play_by_url(&mut self, url: &str, duration_secs: u64) -> Result<()> {
        self.reload_if_changed();
        let id = url_to_id(url);
        if self.favorites.contains_key(&id) {
            self.record_play(&id, duration_secs)
        } else {
            // Not a favorite, ignore
            Ok(())
        }
    }

    /// Add listening time to the favorite playing from `url`, if any
    ///
    /// Unlike [`record_play`](Self::record_play) this does not bump the
    /// generation: stats change every minute while playing, and a full
    /// favorites refresh would interrupt a drag in progress. Returns the
    /// updated favorite so the caller can refresh its row.
    pub fn add_listening(&mut self, url: &str, secs: u64, new_play: bool) -> Option<&Favorite> {
        self.reload_if_changed();
        let id = self.find_match(url, None)?.id();
        self.stats
            .entry(id.clone())
            .or_default()
            .add_listening(secs, new_play);
        self.stats_dirty = true;
        self.favorites.get(&id)
    }

    /// Clear the stats of a favorite
    pub fn reset_stats(&mut self, id: &str) -> Result<()> {
        self.reload_if_changed();
        if !self.favorites.contains_key(id) {
            return Err(AppError::Config(format!(
                "Favorite with ID '{}' not found",
                id
            )));
        }
        if self.stats.remove(id).is_some() {
            self.stats_dirty = true;
        }
        self.generation += 1;
        Ok(())
    }

    /// Take in a list of favorites from elsewhere, in its order. Stations
    /// match by stream URL, and only the first of a URL listed twice
    /// counts. Plays and listening time stay as they are here.
    ///
    /// With `preview` nothing changes; the outcome says what would. A list
    /// that changes something keeps the favorites as they were first, for
    /// [`undo_import`](Self::undo_import). Replace with more than `cap`
    /// stations is refused; Add stops at `cap` favorites.
    pub fn import_list(
        &mut self,
        mode: ImportMode,
        list: &[Favorite],
        cap: usize,
        preview: bool,
    ) -> Result<ImportOutcome> {
        self.reload_if_changed();
        let mut seen = std::collections::HashSet::new();
        let list: Vec<&Favorite> = list.iter().filter(|f| seen.insert(f.id())).collect();
        let mut outcome = ImportOutcome::default();
        let mut next = self.favorites.clone();

        match mode {
            ImportMode::Add => {
                let mut order = self
                    .favorites
                    .values()
                    .map(|f| f.sort_order)
                    .max()
                    .unwrap_or(-1);
                for fav in list {
                    let id = fav.id();
                    if next.contains_key(&id) {
                        outcome.kept += 1;
                    } else if next.len() >= cap {
                        outcome.over_cap += 1;
                    } else {
                        order += 1;
                        let mut fav = fav.clone();
                        fav.sort_order = order;
                        next.insert(id, fav);
                        outcome.added += 1;
                    }
                }
            }
            ImportMode::Replace => {
                if list.len() > cap {
                    return Err(AppError::Config(format!(
                        "The list has {} stations; at most {cap} can be favorites",
                        list.len()
                    )));
                }
                next.clear();
                for (i, fav) in list.iter().enumerate() {
                    let id = fav.id();
                    let mut fav = (*fav).clone();
                    fav.sort_order = i as i32;
                    match self.favorites.get(&id) {
                        Some(here) => {
                            // Added here when it was added here
                            fav.added_at = here.added_at;
                            if here.station == fav.station {
                                outcome.kept += 1;
                            } else {
                                outcome.updated += 1;
                            }
                        }
                        None => outcome.added += 1,
                    }
                    next.insert(id, fav);
                }
                outcome.removed = self
                    .favorites
                    .keys()
                    .filter(|id| !next.contains_key(*id))
                    .count();
                let order_of = |map: &HashMap<String, Favorite>| {
                    let mut both: Vec<(&i32, &String)> = map
                        .iter()
                        .filter(|(id, _)| {
                            self.favorites.contains_key(*id) && next.contains_key(*id)
                        })
                        .map(|(id, f)| (&f.sort_order, id))
                        .collect();
                    both.sort();
                    both.into_iter()
                        .map(|(_, id)| id.clone())
                        .collect::<Vec<_>>()
                };
                outcome.reordered = order_of(&self.favorites) != order_of(&next);
            }
        }

        if preview || !outcome.changes() {
            return Ok(outcome);
        }
        if let Some(path) = self.before_import_path() {
            let file = FavoritesFile {
                version: FAVORITES_VERSION,
                favorites: self.favorites.values().cloned().collect(),
            };
            storage::save_to(&path, &file)?;
        }
        self.favorites = next;
        self.dirty = true;
        self.generation += 1;
        Ok(outcome)
    }

    /// Where the favorites from before the last list taken in are kept
    fn before_import_path(&self) -> Option<PathBuf> {
        self.path
            .as_deref()
            .map(|p| p.with_file_name(BEFORE_IMPORT_FILE))
    }

    /// Whether [`undo_import`](Self::undo_import) has a list to put back
    pub fn can_undo_import(&self) -> bool {
        self.before_import_path().is_some_and(|p| p.is_file())
    }

    /// Put back the favorites as they were before the last list taken in
    /// (changes made since are lost). Returns how many there are now.
    pub fn undo_import(&mut self) -> Result<usize> {
        self.reload_if_changed();
        let path = self
            .before_import_path()
            .filter(|p| p.is_file())
            .ok_or_else(|| AppError::Config("There is no import to undo".into()))?;
        let file = storage::parse_file::<StoredFavoritesFile>(&path)?
            .ok_or_else(|| AppError::Config("There is no import to undo".into()))?;
        self.favorites = read_entries(file, &path).0;
        self.dirty = true;
        self.generation += 1;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(storage::backup_path(&path));
        Ok(self.favorites.len())
    }

    /// Move a favorite to the start or the end of the manual order
    pub fn move_to_edge(&mut self, id: &str, to_top: bool) -> Result<()> {
        self.reload_if_changed();
        let mut ids: Vec<String> = self
            .sorted(FavoriteSort::Manual)
            .iter()
            .map(|f| f.id())
            .filter(|i| i != id)
            .collect();
        if !self.favorites.contains_key(id) {
            return Err(AppError::Config(format!(
                "Favorite with ID '{}' not found",
                id
            )));
        }
        if to_top {
            ids.insert(0, id.to_string());
        } else {
            ids.push(id.to_string());
        }
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.reorder(&refs)
    }

    /// Reorder favorites (set sort_order based on provided ID order)
    pub fn reorder(&mut self, ids: &[&str]) -> Result<()> {
        self.reload_if_changed();
        for (i, id) in ids.iter().enumerate() {
            if let Some(favorite) = self.favorites.get_mut(*id) {
                favorite.sort_order = i as i32;
            }
        }
        self.dirty = true;
        self.generation += 1;
        Ok(())
    }

    /// Import favorites from another manager (merge)
    pub fn import(&mut self, other: &FavoritesManager) -> (usize, usize) {
        self.reload_if_changed();
        let mut added = 0;
        let mut skipped = 0;

        for favorite in other.favorites.values() {
            let id = favorite.id();
            // ID is derived from URL, so same URL = same ID
            use std::collections::hash_map::Entry;
            match self.favorites.entry(id) {
                Entry::Occupied(_) => {
                    skipped += 1;
                }
                Entry::Vacant(entry) => {
                    entry.insert(favorite.clone());
                    added += 1;
                }
            }
        }

        if added > 0 {
            self.dirty = true;
            self.generation += 1;
        }

        (added, skipped)
    }
}

impl Default for FavoritesManager {
    fn default() -> Self {
        Self::new()
    }
}

/// The stats file that goes with the favorites file at `path`
fn stats_path_for(path: &Path) -> PathBuf {
    path.with_file_name(STATS_FILE)
}

/// The favorites in a file read from `path`, by ID, and the stats a
/// version 1 file still kept in each favorite
fn read_entries(
    file: StoredFavoritesFile,
    path: &Path,
) -> (HashMap<String, Favorite>, HashMap<String, StationStats>) {
    let mut favorites = HashMap::new();
    let mut stats = HashMap::new();
    let mut skipped = 0;
    for entry in file.favorites {
        let old_stats = serde_json::from_value::<StationStats>(entry.clone()).unwrap_or_default();
        match serde_json::from_value::<Favorite>(entry) {
            Ok(favorite) => {
                if !old_stats.is_empty() {
                    stats.insert(favorite.id(), old_stats);
                }
                favorites.insert(favorite.id(), favorite);
            }
            Err(e) => {
                eprintln!("Favorites: skipping an entry that can't be read: {e}");
                skipped += 1;
            }
        }
    }
    // The next save drops the entries skipped here, so keep the
    // file as it was for recovery by hand
    if skipped > 0 {
        storage::keep_copy(path);
    }
    (favorites, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::env::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path() -> std::path::PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        // A folder each, since the stats file sits next to the favorites
        let dir = temp_dir().join(format!("radiotrope_fav_test_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(FAVORITES_FILE)
    }

    fn empty_manager() -> FavoritesManager {
        FavoritesManager::new()
    }

    #[test]
    fn test_reset_stats() {
        let mut manager = FavoritesManager::new();
        manager.add(Favorite::new("A", "http://a.test")).unwrap();
        manager.add_listening("http://a.test", 90, true).unwrap();
        let id = manager.get_by_url("http://a.test").unwrap().id();
        let generation = manager.generation();

        manager.reset_stats(&id).unwrap();
        assert!(manager.stats(&id).is_empty());
        assert!(manager.generation() > generation);
        assert!(manager.reset_stats("missing").is_err());

        // Still listening after the reset: counts as a play again
        manager.add_listening("http://a.test", 60, false).unwrap();
        assert_eq!(manager.stats(&id).play_count, 1);
    }

    #[test]
    fn test_add_listening_counts_one_play_per_session() {
        let mut manager = empty_manager();
        manager.add(Favorite::new("A", "http://a.test")).unwrap();
        let gen = manager.generation();

        manager.add_listening("http://a.test", 60, true).unwrap();
        manager.add_listening("http://a.test", 60, false).unwrap();
        let stats = manager.stats(&url_to_id("http://a.test"));
        assert_eq!(stats.play_count, 1);
        assert_eq!(stats.total_listen_time_secs, 120);
        assert!(stats.last_played.is_some());
        assert!(manager.is_dirty());
        // No full refresh while listening
        assert_eq!(manager.generation(), gen);

        assert!(manager
            .add_listening("http://other.test", 60, true)
            .is_none());
    }

    #[test]
    fn test_move_to_edge() {
        let mut manager = empty_manager();
        for (i, url) in ["http://a.test", "http://b.test", "http://c.test"]
            .iter()
            .enumerate()
        {
            let mut fav = Favorite::new(format!("S{i}"), *url);
            fav.sort_order = i as i32;
            manager.add(fav).unwrap();
        }
        let names = |m: &FavoritesManager| -> Vec<String> {
            m.sorted(FavoriteSort::Manual)
                .iter()
                .map(|f| f.name().to_string())
                .collect()
        };
        let c = url_to_id("http://c.test");
        manager.move_to_edge(&c, true).unwrap();
        assert_eq!(names(&manager), ["S2", "S0", "S1"]);
        manager.move_to_edge(&c, false).unwrap();
        assert_eq!(names(&manager), ["S0", "S1", "S2"]);
        assert!(manager.move_to_edge("missing", true).is_err());
    }

    #[test]
    fn test_url_to_id_deterministic() {
        let url = "http://test.com/stream";
        let id1 = url_to_id(url);
        let id2 = url_to_id(url);
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_add_and_get() {
        let mut manager = empty_manager();

        let fav = Favorite::new("Test Radio", "http://test.com/stream");
        let id = fav.id();

        manager.add(fav).unwrap();

        assert!(manager.get(&id).is_some());
        assert!(manager.is_favorite("http://test.com/stream"));
    }

    #[test]
    fn test_duplicate_url() {
        let mut manager = empty_manager();

        manager
            .add(Favorite::new("Test 1", "http://test.com"))
            .unwrap();
        let result = manager.add(Favorite::new("Test 2", "http://test.com"));

        assert!(result.is_err());
    }

    #[test]
    fn test_toggle() {
        let mut manager = empty_manager();

        // Toggle on
        let result = manager.toggle("Test", "http://test.com", None).unwrap();
        assert!(result.is_some());
        assert!(manager.is_favorite("http://test.com"));

        // Toggle off
        let result = manager.toggle("Test", "http://test.com", None).unwrap();
        assert!(result.is_none());
        assert!(!manager.is_favorite("http://test.com"));
    }

    #[test]
    fn test_get_by_url() {
        let mut manager = empty_manager();

        manager
            .add(Favorite::new("Test Radio", "http://test.com"))
            .unwrap();

        let fav = manager.get_by_url("http://test.com");
        assert!(fav.is_some());
        assert_eq!(fav.unwrap().name(), "Test Radio");

        let not_found = manager.get_by_url("http://other.com");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_update() {
        let mut manager = empty_manager();

        let fav = Favorite::new("Old Name", "http://test.com");
        let id = fav.id();
        manager.add(fav).unwrap();

        manager
            .update(&id, FavoriteUpdate::new().name("New Name"))
            .unwrap();

        assert_eq!(manager.get(&id).unwrap().name(), "New Name");
    }

    #[test]
    fn test_sorting() {
        let mut manager = empty_manager();

        manager
            .add(Favorite::new("Zebra Radio", "http://zebra.com"))
            .unwrap();
        manager
            .add(Favorite::new("Apple Radio", "http://apple.com"))
            .unwrap();
        for _ in 0..5 {
            manager.record_play_by_url("http://zebra.com", 60).unwrap();
        }
        for _ in 0..10 {
            manager.record_play_by_url("http://apple.com", 60).unwrap();
        }

        // By name
        let sorted = manager.sorted(FavoriteSort::Name);
        assert_eq!(sorted[0].name(), "Apple Radio");

        // By play count
        let sorted = manager.sorted(FavoriteSort::MostPlayed);
        assert_eq!(sorted[0].name(), "Apple Radio"); // 10 plays
    }

    #[test]
    fn test_filter() {
        let mut manager = empty_manager();

        let mut fav1 = Favorite::new("Rock Station", "http://rock.fm");
        fav1.station.genres.insert("rock".to_string());

        let fav2 = Favorite::new("Jazz Station", "http://jazz.fm");

        manager.add(fav1).unwrap();
        manager.add(fav2).unwrap();

        let filter = FavoriteFilter::new().genre("rock");
        let filtered = manager.filtered(&filter);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name(), "Rock Station");
    }

    #[test]
    fn test_dirty_flag() {
        let mut manager = empty_manager();
        assert!(!manager.is_dirty());

        manager
            .add(Favorite::new("Test", "http://test.com"))
            .unwrap();
        assert!(manager.is_dirty());
    }

    // =========================================================================
    // Persistence tests
    // =========================================================================

    #[test]
    fn test_save_and_load_roundtrip() {
        let path = temp_path();

        // Create and save
        {
            let mut manager = FavoritesManager::new();
            manager
                .add(Favorite::new("Station 1", "http://station1.com"))
                .unwrap();
            manager
                .add(Favorite::new("Station 2", "http://station2.com"))
                .unwrap();
            manager.save_to(&path).unwrap();
        }

        // Load and verify
        {
            let manager = FavoritesManager::load_from(&path).unwrap();
            assert_eq!(manager.count(), 2);
            assert!(manager.is_favorite("http://station1.com"));
            assert!(manager.is_favorite("http://station2.com"));
            assert_eq!(
                manager.get_by_url("http://station1.com").unwrap().name(),
                "Station 1"
            );
        }

        // Cleanup
        remove_files(&path);
    }

    #[test]
    fn one_damaged_favorite_does_not_cost_the_others() {
        let path = temp_path();
        // The second entry lacks `added_at`
        fs::write(
            &path,
            r#"{"version": 1, "favorites": [
                {"name": "Good", "url": "http://good.test", "added_at": 1},
                {"name": "Bad", "url": "http://bad.test"}
            ]}"#,
        )
        .unwrap();

        let manager = FavoritesManager::load_from(&path).unwrap();
        assert_eq!(manager.count(), 1);
        assert!(manager.is_favorite("http://good.test"));

        // The file as it was is kept, since the next save drops the bad entry
        let dir = path.parent().unwrap();
        let prefix = format!("{}.bad-", path.file_name().unwrap().to_string_lossy());
        let copies: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| e.path())
            .collect();
        assert_eq!(copies.len(), 1);
        assert!(fs::read_to_string(&copies[0])
            .unwrap()
            .contains("http://bad.test"));

        for copy in copies {
            let _ = fs::remove_file(copy);
        }
        remove_files(&path);
    }

    #[test]
    fn test_load_nonexistent_file() {
        let path = temp_path();
        let manager = FavoritesManager::load_from(&path).unwrap();
        assert!(manager.is_empty());
    }

    #[test]
    fn test_save_skips_when_not_dirty() {
        let path = temp_path();

        let mut manager = FavoritesManager::new();
        // Not dirty, should not create file
        manager.save_to(&path).unwrap();
        assert!(!path.exists());

        // Make dirty
        manager
            .add(Favorite::new("Test", "http://test.com"))
            .unwrap();
        manager.save_to(&path).unwrap();
        assert!(path.exists());

        remove_files(&path);
    }

    #[test]
    fn test_force_save() {
        let path = temp_path();

        let mut manager = FavoritesManager::new();
        manager
            .add(Favorite::new("Test", "http://test.com"))
            .unwrap();
        manager.save_to(&path).unwrap(); // Clear dirty flag

        assert!(!manager.is_dirty());

        // Modify file externally wouldn't be detected, but force_save should work
        manager.force_save_to(&path).unwrap();
        assert!(!manager.is_dirty());

        remove_files(&path);
    }

    #[test]
    fn test_persistence_preserves_all_fields() {
        let path = temp_path();

        let url = "http://test.com/stream";

        // Create with all fields populated
        {
            let mut manager = FavoritesManager::new();
            let mut fav = Favorite::new("Full Station", url)
                .with_logo("http://logo.com/img.png")
                .with_provider("radio-browser", Some("uuid-123".to_string()))
                .with_metadata(
                    Some("US".to_string()),
                    Some("English".to_string()),
                    std::collections::HashSet::from(["rock".to_string(), "pop".to_string()]),
                )
                .with_audio_info(Some("MP3".to_string()), Some(320));

            fav.station.homepage = Some("http://station.com".to_string());

            let id = fav.id();
            manager.add(fav).unwrap();
            manager.record_play(&id, 3600).unwrap();
            // Set sort_order after add (add() auto-assigns order)
            manager.get_mut(&id).unwrap().sort_order = 5;
            manager.save_to(&path).unwrap();
        }

        // Load and verify all fields
        {
            let manager = FavoritesManager::load_from(&path).unwrap();
            let fav = manager.get_by_url(url).unwrap();

            assert_eq!(fav.name(), "Full Station");
            assert_eq!(
                fav.station.logo_url,
                Some("http://logo.com/img.png".to_string())
            );
            assert_eq!(fav.station.provider, "radio-browser");
            assert_eq!(fav.station.provider_id, Some("uuid-123".to_string()));
            assert_eq!(fav.station.country, Some("US".to_string()));
            assert_eq!(fav.station.language, Some("English".to_string()));
            assert!(fav.station.genres.contains("rock"));
            assert!(fav.station.genres.contains("pop"));
            assert_eq!(fav.station.codec, Some("MP3".to_string()));
            assert_eq!(fav.station.bitrate, Some(320));
            assert_eq!(fav.station.homepage, Some("http://station.com".to_string()));
            let stats = manager.stats(&fav.id());
            assert_eq!(stats.play_count, 1);
            assert_eq!(stats.total_listen_time_secs, 3600);
            assert_eq!(fav.sort_order, 5);
        }

        remove_files(&path);
    }

    #[test]
    fn test_modify_and_save() {
        let path = temp_path();
        let url = "http://test.com";

        // Create initial
        {
            let mut manager = FavoritesManager::new();
            manager.add(Favorite::new("Original", url)).unwrap();
            manager.save_to(&path).unwrap();
        }

        // Load, modify, save
        {
            let mut manager = FavoritesManager::load_from(&path).unwrap();
            let id = manager.get_id_for_url(url);
            manager
                .update(&id, FavoriteUpdate::new().name("Modified"))
                .unwrap();
            manager.save_to(&path).unwrap();
        }

        // Verify modification persisted
        {
            let manager = FavoritesManager::load_from(&path).unwrap();
            assert_eq!(manager.get_by_url(url).unwrap().name(), "Modified");
        }

        remove_files(&path);
    }

    #[test]
    fn test_remove_and_save() {
        let path = temp_path();

        // Create with two favorites
        {
            let mut manager = FavoritesManager::new();
            manager
                .add(Favorite::new("Keep", "http://keep.com"))
                .unwrap();
            manager
                .add(Favorite::new("Remove", "http://remove.com"))
                .unwrap();
            manager.save_to(&path).unwrap();
        }

        // Load, remove one, save
        {
            let mut manager = FavoritesManager::load_from(&path).unwrap();
            manager.remove_by_url("http://remove.com").unwrap();
            manager.save_to(&path).unwrap();
        }

        // Verify removal persisted
        {
            let manager = FavoritesManager::load_from(&path).unwrap();
            assert_eq!(manager.count(), 1);
            assert!(manager.is_favorite("http://keep.com"));
            assert!(!manager.is_favorite("http://remove.com"));
        }

        remove_files(&path);
    }

    // =========================================================================
    // Play metadata resolution
    // =========================================================================

    fn manager_with_favorite() -> FavoritesManager {
        let mut manager = empty_manager();
        let fav = Favorite::new("My Station", "http://fav.example/stream")
            .with_provider("radio-browser", Some("uuid-1".to_string()))
            .with_logo("http://fav.example/logo.png")
            .with_metadata(Some("Greece".to_string()), None, HashSet::new());
        manager.add(fav).unwrap();
        manager
    }

    #[test]
    fn test_find_match_by_url() {
        let manager = manager_with_favorite();
        assert!(manager
            .find_match("http://fav.example/stream", None)
            .is_some());
    }

    #[test]
    fn test_find_match_by_provider_id() {
        let manager = manager_with_favorite();
        let fav = manager
            .find_match("http://resolved.example/other", Some("uuid-1"))
            .unwrap();
        assert_eq!(fav.url(), "http://fav.example/stream");
    }

    #[test]
    fn test_find_match_none() {
        let manager = manager_with_favorite();
        assert!(manager.find_match("http://other/stream", None).is_none());
        assert!(manager
            .find_match("http://other/stream", Some("uuid-2"))
            .is_none());
        assert!(manager
            .find_match("http://other/stream", Some(""))
            .is_none());
    }

    #[test]
    fn test_resolve_play_favorite_wins() {
        let manager = manager_with_favorite();
        let resolved = manager.resolve_play(PlayMetadata {
            url: "http://resolved.example/other".to_string(),
            name: Some("Search Name".to_string()),
            logo_url: Some("http://search.example/favicon.ico".to_string()),
            country: Some("The Hellenic Republic".to_string()),
            provider_id: Some("uuid-1".to_string()),
        });
        assert_eq!(resolved.url, "http://fav.example/stream");
        assert_eq!(resolved.name.as_deref(), Some("My Station"));
        assert_eq!(
            resolved.logo_url.as_deref(),
            Some("http://fav.example/logo.png")
        );
        assert_eq!(resolved.country.as_deref(), Some("Greece"));
    }

    #[test]
    fn test_resolve_play_fills_gaps_from_request() {
        let mut manager = empty_manager();
        manager
            .add(Favorite::new("Bare", "http://bare.example/stream"))
            .unwrap();
        let resolved = manager.resolve_play(PlayMetadata {
            url: "http://bare.example/stream".to_string(),
            logo_url: Some("http://search.example/logo.png".to_string()),
            country: Some("Germany".to_string()),
            ..Default::default()
        });
        assert_eq!(resolved.name.as_deref(), Some("Bare"));
        assert_eq!(
            resolved.logo_url.as_deref(),
            Some("http://search.example/logo.png")
        );
        assert_eq!(resolved.country.as_deref(), Some("Germany"));
    }

    #[test]
    fn test_resolve_play_not_a_favorite() {
        let manager = manager_with_favorite();
        let request = PlayMetadata {
            url: "http://other/stream".to_string(),
            name: Some("Other".to_string()),
            ..Default::default()
        };
        assert_eq!(manager.resolve_play(request.clone()), request);
    }

    // =========================================================================
    // Two players on one file (`--mcp --standalone`)
    // =========================================================================

    /// A file holding `urls`, and two managers that loaded it
    fn two_players(urls: &[&str]) -> (std::path::PathBuf, FavoritesManager, FavoritesManager) {
        let path = temp_path();
        let mut first = FavoritesManager::new();
        for url in urls {
            first.add(Favorite::new(*url, *url)).unwrap();
        }
        first.force_save_to(&path).unwrap();
        let gui = FavoritesManager::load_from(&path).unwrap();
        let agent = FavoritesManager::load_from(&path).unwrap();
        (path, gui, agent)
    }

    fn urls(manager: &FavoritesManager) -> Vec<String> {
        let mut urls: Vec<_> = manager.all().iter().map(|f| f.url().to_string()).collect();
        urls.sort();
        urls
    }

    fn saved_urls(path: &Path) -> Vec<String> {
        urls(&FavoritesManager::load_from(path).unwrap())
    }

    fn remove_files(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_save_keeps_a_favorite_another_player_added_meanwhile() {
        let (path, mut gui, mut agent) = two_players(&["http://a.test"]);
        // Both add one before either saves
        gui.add(Favorite::new("B", "http://b.test")).unwrap();
        agent.add(Favorite::new("C", "http://c.test")).unwrap();
        agent.save_to(&path).unwrap();
        gui.save_to(&path).unwrap();

        assert_eq!(
            saved_urls(&path),
            ["http://a.test", "http://b.test", "http://c.test"]
        );
        // The agent's player sees the GUI's at its next look
        assert!(agent.reload_if_changed());
        assert_eq!(urls(&agent), saved_urls(&path));
        remove_files(&path);
    }

    #[test]
    fn a_removal_by_another_player_is_not_undone() {
        let (path, mut gui, mut agent) = two_players(&["http://a.test", "http://b.test"]);
        // Unsaved listening time here while the agent removes the other one
        gui.add_listening("http://a.test", 60, true).unwrap();
        agent.remove_by_url("http://b.test").unwrap();
        agent.save_to(&path).unwrap();
        gui.save_to(&path).unwrap();

        let saved = FavoritesManager::load_from(&path).unwrap();
        assert_eq!(urls(&saved), ["http://a.test"]);
        let a = saved.stats(&url_to_id("http://a.test"));
        assert_eq!(a.total_listen_time_secs, 60);
        remove_files(&path);
    }

    #[test]
    fn listening_time_from_both_players_is_kept() {
        let (path, mut gui, mut agent) = two_players(&["http://a.test", "http://b.test"]);
        gui.add_listening("http://a.test", 60, true).unwrap();
        agent.add_listening("http://b.test", 120, true).unwrap();
        agent.save_to(&path).unwrap();
        gui.save_to(&path).unwrap();

        let saved = FavoritesManager::load_from(&path).unwrap();
        let secs = |url| saved.stats(&url_to_id(url)).total_listen_time_secs;
        assert_eq!((secs("http://a.test"), secs("http://b.test")), (60, 120));
        remove_files(&path);
    }

    #[test]
    fn a_change_made_here_wins_over_the_other_players_change_to_that_favorite() {
        let (path, mut gui, mut agent) = two_players(&["http://a.test"]);
        let id = url_to_id("http://a.test");
        gui.update(&id, FavoriteUpdate::new().name("Renamed here"))
            .unwrap();
        agent
            .update(&id, FavoriteUpdate::new().name("Renamed by the agent"))
            .unwrap();
        agent.save_to(&path).unwrap();
        gui.save_to(&path).unwrap();

        let saved = FavoritesManager::load_from(&path).unwrap();
        assert_eq!(saved.get(&id).unwrap().name(), "Renamed here");
        remove_files(&path);
    }

    #[test]
    fn a_change_starts_from_what_another_player_saved() {
        let (path, mut gui, mut agent) = two_players(&["http://a.test"]);
        agent.add(Favorite::new("B", "http://b.test")).unwrap();
        agent.save_to(&path).unwrap();
        // The GUI's next favorite goes after the agent's, and a star on
        // the agent's favorite removes it rather than adding it twice
        gui.add(Favorite::new("C", "http://c.test")).unwrap();
        let order = |url| gui.get_by_url(url).unwrap().sort_order;
        assert!(order("http://c.test") > order("http://b.test"));
        assert_eq!(gui.toggle("B", "http://b.test", None).unwrap(), None);
        gui.save_to(&path).unwrap();

        assert_eq!(saved_urls(&path), ["http://a.test", "http://c.test"]);
        remove_files(&path);
    }

    #[test]
    fn only_another_players_save_is_news() {
        let (path, mut gui, mut agent) = two_players(&["http://a.test"]);
        let generation = gui.generation();
        assert!(!gui.reload_if_changed());

        agent.add(Favorite::new("B", "http://b.test")).unwrap();
        agent.save_to(&path).unwrap();
        assert!(gui.reload_if_changed());
        assert!(gui.generation() > generation);
        // What it holds now is what is saved
        assert!(!gui.is_dirty());

        // Its own save isn't news to it
        gui.add(Favorite::new("C", "http://c.test")).unwrap();
        gui.save_to(&path).unwrap();
        let generation = gui.generation();
        assert!(!gui.reload_if_changed());
        assert_eq!(gui.generation(), generation);
        remove_files(&path);
    }

    #[test]
    fn a_missing_or_damaged_file_is_no_news() {
        let (path, mut gui, _agent) = two_players(&["http://a.test"]);
        fs::remove_file(&path).unwrap();
        assert!(!gui.reload_if_changed());
        assert_eq!(urls(&gui), ["http://a.test"]);

        fs::write(&path, r#"{"version": 1, "favorites": ["#).unwrap();
        assert!(!gui.reload_if_changed());
        assert_eq!(urls(&gui), ["http://a.test"]);

        // The next save writes the file anew
        gui.add(Favorite::new("B", "http://b.test")).unwrap();
        gui.save_to(&path).unwrap();
        assert_eq!(saved_urls(&path), ["http://a.test", "http://b.test"]);
        remove_files(&path);
    }

    #[test]
    fn a_first_save_to_a_file_keeps_what_is_in_it() {
        let (path, _gui, _agent) = two_players(&["http://a.test"]);
        // A list that started empty (the file couldn't be read at startup)
        let mut fresh = FavoritesManager::new();
        fresh.add(Favorite::new("B", "http://b.test")).unwrap();
        fresh.save_to(&path).unwrap();

        assert_eq!(saved_urls(&path), ["http://a.test", "http://b.test"]);
        remove_files(&path);
    }

    fn stats_file(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(stats_path_for(path)).unwrap()).unwrap()
    }

    #[test]
    fn stats_of_a_version_1_file_move_to_the_stats_file() {
        let path = temp_path();
        fs::write(
            &path,
            r#"{"version": 1, "favorites": [
                {"name": "A", "url": "http://a.test", "added_at": 1, "sort_order": 0,
                 "play_count": 4, "total_listen_time_secs": 700, "last_played": 99},
                {"name": "B", "url": "http://b.test", "added_at": 2, "sort_order": 1}
            ]}"#,
        )
        .unwrap();

        let mut manager = FavoritesManager::load_from(&path).unwrap();
        let a = url_to_id("http://a.test");
        assert_eq!(manager.stats(&a).play_count, 4);
        assert_eq!(manager.stats(&a).total_listen_time_secs, 700);
        assert!(manager.stats(&url_to_id("http://b.test")).is_empty());
        assert!(manager.is_dirty());

        manager.save_to(&path).unwrap();
        let stats = stats_file(&path);
        assert_eq!(stats["stations"][&a]["play_count"], 4);
        assert_eq!(stats["stations"][&a]["last_played"], 99);

        // Read back from the stats file, not the old favorites
        manager.add(Favorite::new("C", "http://c.test")).unwrap();
        manager.save_to(&path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("play_count"));
        assert!(saved.contains("\"version\": 2"));
        let again = FavoritesManager::load_from(&path).unwrap();
        assert_eq!(again.stats(&a).total_listen_time_secs, 700);
        remove_files(&path);
    }

    #[test]
    fn listening_saves_the_stats_file_only() {
        let (path, mut gui, _agent) = two_players(&["http://a.test"]);
        let before = fs::read_to_string(&path).unwrap();
        let stamp = storage::stamp(&path);

        gui.add_listening("http://a.test", 60, true).unwrap();
        gui.save_to(&path).unwrap();

        assert_eq!(storage::stamp(&path), stamp);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        let id = url_to_id("http://a.test");
        assert_eq!(
            stats_file(&path)["stations"][&id]["total_listen_time_secs"],
            60
        );
        remove_files(&path);
    }

    #[test]
    fn stats_follow_a_new_stream_url() {
        let mut manager = empty_manager();
        manager.add(Favorite::new("A", "http://a.test")).unwrap();
        manager.add_listening("http://a.test", 120, true).unwrap();
        let old = url_to_id("http://a.test");

        manager
            .update(&old, FavoriteUpdate::new().url("http://a2.test"))
            .unwrap();

        assert!(manager.stats(&old).is_empty());
        let new = manager.stats(&url_to_id("http://a2.test"));
        assert_eq!(new.total_listen_time_secs, 120);
    }

    #[test]
    fn stats_come_back_when_a_removed_favorite_is_added_again() {
        let path = temp_path();
        let mut manager = FavoritesManager::new();
        manager.add(Favorite::new("A", "http://a.test")).unwrap();
        manager.add(Favorite::new("B", "http://b.test")).unwrap();
        manager.add_listening("http://a.test", 300, true).unwrap();
        // Played long ago, then removed: forgotten at the next save
        manager.stats.insert(
            url_to_id("http://b.test"),
            StationStats {
                play_count: 1,
                total_listen_time_secs: 60,
                last_played: Some(1),
            },
        );
        manager.remove_by_url("http://a.test").unwrap();
        manager.remove_by_url("http://b.test").unwrap();
        manager.save_to(&path).unwrap();

        let mut manager = FavoritesManager::load_from(&path).unwrap();
        assert!(manager.stats(&url_to_id("http://b.test")).is_empty());
        manager.add(Favorite::new("A", "http://a.test")).unwrap();
        let a = manager.stats(&url_to_id("http://a.test"));
        assert_eq!(a.total_listen_time_secs, 300);
        remove_files(&path);
    }

    #[test]
    fn a_damaged_stats_file_costs_the_stats_only() {
        let (path, mut gui, _agent) = two_players(&["http://a.test"]);
        gui.add_listening("http://a.test", 60, true).unwrap();
        gui.save_to(&path).unwrap();
        fs::write(stats_path_for(&path), "{ not json").unwrap();
        let _ = fs::remove_file(storage::backup_path(&stats_path_for(&path)));

        let manager = FavoritesManager::load_from(&path).unwrap();
        assert!(manager.is_favorite("http://a.test"));
        assert!(manager.stats(&url_to_id("http://a.test")).is_empty());
        remove_files(&path);
    }

    fn manual_urls(manager: &FavoritesManager) -> Vec<String> {
        manager
            .sorted(FavoriteSort::Manual)
            .iter()
            .map(|f| f.url().to_string())
            .collect()
    }

    fn list(urls: &[&str]) -> Vec<Favorite> {
        urls.iter().map(|u| Favorite::new(*u, *u)).collect()
    }

    #[test]
    fn adding_a_list_keeps_what_is_here_and_adds_the_rest_at_the_end() {
        let path = temp_path();
        let mut manager = FavoritesManager::new();
        manager.add(Favorite::new("Mine", "http://b.test")).unwrap();
        manager.add(Favorite::new("A", "http://a.test")).unwrap();
        manager.save_to(&path).unwrap();
        let theirs = list(&[
            "http://c.test",
            "http://b.test",
            "http://d.test",
            "http://c.test",
        ]);

        let preview = manager
            .import_list(ImportMode::Add, &theirs, 1000, true)
            .unwrap();
        assert_eq!(manual_urls(&manager), ["http://b.test", "http://a.test"]);
        assert!(!manager.can_undo_import());

        let done = manager
            .import_list(ImportMode::Add, &theirs, 1000, false)
            .unwrap();
        assert_eq!(done, preview);
        assert_eq!((done.added, done.kept, done.removed), (2, 1, 0));
        assert_eq!(
            manual_urls(&manager),
            [
                "http://b.test",
                "http://a.test",
                "http://c.test",
                "http://d.test"
            ]
        );
        // The one here keeps its own name
        assert_eq!(manager.get_by_url("http://b.test").unwrap().name(), "Mine");
        assert!(manager.can_undo_import());
        remove_files(&path);
    }

    #[test]
    fn adding_stops_at_the_cap() {
        let mut manager = FavoritesManager::new();
        manager.add(Favorite::new("A", "http://a.test")).unwrap();
        let done = manager
            .import_list(
                ImportMode::Add,
                &list(&["http://b.test", "http://c.test"]),
                2,
                false,
            )
            .unwrap();
        assert_eq!((done.added, done.over_cap), (1, 1));
        assert_eq!(manager.count(), 2);
    }

    #[test]
    fn replacing_takes_the_list_its_details_and_order_and_keeps_the_stats() {
        let mut manager = FavoritesManager::new();
        for url in ["http://a.test", "http://b.test", "http://x.test"] {
            manager.add(Favorite::new(url, url)).unwrap();
        }
        manager.add_listening("http://a.test", 600, true).unwrap();
        let added_at = manager.get_by_url("http://a.test").unwrap().added_at;
        let mut theirs = list(&["http://c.test", "http://b.test", "http://a.test"]);
        theirs[2].station.name = "A renamed".into();
        theirs[2].added_at = 1;

        let done = manager
            .import_list(ImportMode::Replace, &theirs, 1000, false)
            .unwrap();
        assert_eq!(
            (
                done.added,
                done.updated,
                done.kept,
                done.removed,
                done.reordered
            ),
            (1, 1, 1, 1, true)
        );
        assert_eq!(
            manual_urls(&manager),
            ["http://c.test", "http://b.test", "http://a.test"]
        );
        let a = manager.get_by_url("http://a.test").unwrap();
        assert_eq!(a.name(), "A renamed");
        assert_eq!(a.added_at, added_at);
        assert_eq!(manager.stats(&a.id()).total_listen_time_secs, 600);
    }

    #[test]
    fn replacing_with_the_same_list_changes_nothing() {
        let mut manager = FavoritesManager::new();
        for url in ["http://a.test", "http://b.test"] {
            manager.add(Favorite::new(url, url)).unwrap();
        }
        let generation = manager.generation();
        let same: Vec<Favorite> = manager
            .sorted(FavoriteSort::Manual)
            .into_iter()
            .cloned()
            .collect();
        let done = manager
            .import_list(ImportMode::Replace, &same, 1000, false)
            .unwrap();
        assert!(!done.changes());
        assert_eq!(done.kept, 2);
        assert_eq!(manager.generation(), generation);
    }

    #[test]
    fn replacing_with_too_many_is_refused() {
        let mut manager = FavoritesManager::new();
        manager.add(Favorite::new("A", "http://a.test")).unwrap();
        assert!(manager
            .import_list(
                ImportMode::Replace,
                &list(&["http://b.test", "http://c.test"]),
                1,
                false
            )
            .is_err());
        assert_eq!(manual_urls(&manager), ["http://a.test"]);
    }

    #[test]
    fn an_import_can_be_undone_once() {
        let path = temp_path();
        let mut manager = FavoritesManager::new();
        for url in ["http://a.test", "http://b.test"] {
            manager.add(Favorite::new(url, url)).unwrap();
        }
        manager.save_to(&path).unwrap();
        assert!(manager.undo_import().is_err());

        manager
            .import_list(ImportMode::Replace, &list(&["http://c.test"]), 1000, false)
            .unwrap();
        manager.save_to(&path).unwrap();
        assert_eq!(saved_urls(&path), ["http://c.test"]);

        assert_eq!(manager.undo_import().unwrap(), 2);
        manager.save_to(&path).unwrap();
        assert_eq!(manual_urls(&manager), ["http://a.test", "http://b.test"]);
        assert_eq!(saved_urls(&path), ["http://a.test", "http://b.test"]);
        assert!(!manager.can_undo_import());
        assert!(manager.undo_import().is_err());
        remove_files(&path);
    }
}
