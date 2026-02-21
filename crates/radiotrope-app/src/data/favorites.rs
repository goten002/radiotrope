//! Favorites management
//!
//! In-memory management of favorite stations.

use crate::data::storage;
use crate::data::types::{url_to_id, Favorite, FavoriteFilter, FavoriteSort, FavoriteUpdate};
use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Favorites data file name
const FAVORITES_FILE: &str = "favorites2.json";

/// Favorites file format version for migrations
const FAVORITES_VERSION: u32 = 1;

/// Favorites file structure
#[derive(Debug, Serialize, Deserialize)]
struct FavoritesFile {
    version: u32,
    favorites: Vec<Favorite>,
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
}

impl FavoritesManager {
    /// Create a new empty manager
    pub fn new() -> Self {
        Self {
            favorites: HashMap::new(),
            dirty: false,
        }
    }

    /// Load favorites from default storage location
    pub fn load() -> Result<Self> {
        let path = storage::data_path(FAVORITES_FILE)?;
        Self::load_from(&path)
    }

    /// Load favorites from a specific path
    pub fn load_from(path: &Path) -> Result<Self> {
        let mut manager = Self::new();

        if let Some(file) = storage::load_from::<FavoritesFile>(path)? {
            // TODO: Handle version migrations when FAVORITES_VERSION increases
            for favorite in file.favorites {
                manager.favorites.insert(favorite.id(), favorite);
            }
        }

        manager.dirty = false;
        Ok(manager)
    }

    /// Save favorites to default storage location
    pub fn save(&mut self) -> Result<()> {
        let path = storage::data_path(FAVORITES_FILE)?;
        self.save_to(&path)
    }

    /// Save favorites to a specific path
    pub fn save_to(&mut self, path: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }

        let file = FavoritesFile {
            version: FAVORITES_VERSION,
            favorites: self.favorites.values().cloned().collect(),
        };

        storage::save_to(path, &file)?;
        self.dirty = false;
        Ok(())
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
        self.dirty
    }

    /// Add a new favorite
    pub fn add(&mut self, favorite: Favorite) -> Result<()> {
        let id = favorite.id();
        // Check for duplicate (ID is derived from URL, so same URL = same ID)
        if self.favorites.contains_key(&id) {
            return Err(AppError::Config(format!(
                "A favorite with URL '{}' already exists",
                favorite.url()
            )));
        }

        self.favorites.insert(id, favorite);
        self.dirty = true;
        Ok(())
    }

    /// Remove a favorite by ID
    pub fn remove(&mut self, id: &str) -> Result<Favorite> {
        let favorite = self.favorites.remove(id).ok_or_else(|| {
            AppError::Config(format!("Favorite with ID '{}' not found", id))
        })?;

        self.dirty = true;
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
        self.dirty = true; // Assume modification
        self.favorites.get_mut(id)
    }

    /// Get a favorite by URL (O(1) - just compute hash)
    pub fn get_by_url(&self, url: &str) -> Option<&Favorite> {
        let id = url_to_id(url);
        self.favorites.get(&id)
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
            let mut favorite = self.favorites.remove(id).ok_or_else(|| {
                AppError::Config(format!("Favorite with ID '{}' not found", id))
            })?;

            update.apply_to(&mut favorite);
            // ID is computed from URL, so after applying update with new URL,
            // favorite.id() will return the new_id
            self.favorites.insert(favorite.id(), favorite);
        } else {
            // No URL change, just update in place
            let favorite = self.favorites.get_mut(id).ok_or_else(|| {
                AppError::Config(format!("Favorite with ID '{}' not found", id))
            })?;
            update.apply_to(favorite);
        }

        self.dirty = true;
        Ok(())
    }

    /// Toggle favorite status for a URL
    /// Returns Some(id) if added, None if removed
    pub fn toggle(&mut self, name: &str, url: &str, logo_url: Option<&str>) -> Result<Option<String>> {
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
                favorites.sort_by(|a, b| b.added_at.cmp(&a.added_at));
            }
            FavoriteSort::RecentlyPlayed => {
                favorites.sort_by(|a, b| {
                    let a_time = a.last_played.unwrap_or(0);
                    let b_time = b.last_played.unwrap_or(0);
                    b_time.cmp(&a_time)
                });
            }
            FavoriteSort::MostPlayed => {
                favorites.sort_by(|a, b| b.play_count.cmp(&a.play_count));
            }
            FavoriteSort::MostListened => {
                favorites.sort_by(|a, b| b.total_listen_time_secs.cmp(&a.total_listen_time_secs));
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
                favorites.sort_by(|a, b| b.added_at.cmp(&a.added_at));
            }
            FavoriteSort::RecentlyPlayed => {
                favorites.sort_by(|a, b| {
                    let a_time = a.last_played.unwrap_or(0);
                    let b_time = b.last_played.unwrap_or(0);
                    b_time.cmp(&a_time)
                });
            }
            FavoriteSort::MostPlayed => {
                favorites.sort_by(|a, b| b.play_count.cmp(&a.play_count));
            }
            FavoriteSort::MostListened => {
                favorites.sort_by(|a, b| b.total_listen_time_secs.cmp(&a.total_listen_time_secs));
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
        let favorite = self.favorites.get_mut(id).ok_or_else(|| {
            AppError::Config(format!("Favorite with ID '{}' not found", id))
        })?;
        favorite.record_play(duration_secs);
        self.dirty = true;
        Ok(())
    }

    /// Record a play session by URL
    pub fn record_play_by_url(&mut self, url: &str, duration_secs: u64) -> Result<()> {
        let id = url_to_id(url);
        if self.favorites.contains_key(&id) {
            self.record_play(&id, duration_secs)
        } else {
            // Not a favorite, ignore
            Ok(())
        }
    }

    /// Reorder favorites (set sort_order based on provided ID order)
    pub fn reorder(&mut self, ids: &[&str]) -> Result<()> {
        for (i, id) in ids.iter().enumerate() {
            if let Some(favorite) = self.favorites.get_mut(*id) {
                favorite.sort_order = i as i32;
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// Import favorites from another manager (merge)
    pub fn import(&mut self, other: &FavoritesManager) -> (usize, usize) {
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
        }

        (added, skipped)
    }
}

impl Default for FavoritesManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path() -> std::path::PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        temp_dir().join(format!("radiotrope_fav_test_{}.json", id))
    }

    fn empty_manager() -> FavoritesManager {
        FavoritesManager::new()
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

        manager.add(Favorite::new("Test 1", "http://test.com")).unwrap();
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

        manager.add(Favorite::new("Test Radio", "http://test.com")).unwrap();

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

        manager.update(&id, FavoriteUpdate::new().name("New Name")).unwrap();

        assert_eq!(manager.get(&id).unwrap().name(), "New Name");
    }

    #[test]
    fn test_sorting() {
        let mut manager = empty_manager();

        let mut fav1 = Favorite::new("Zebra Radio", "http://zebra.com");
        fav1.play_count = 5;

        let mut fav2 = Favorite::new("Apple Radio", "http://apple.com");
        fav2.play_count = 10;

        manager.add(fav1).unwrap();
        manager.add(fav2).unwrap();

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

        manager.add(Favorite::new("Test", "http://test.com")).unwrap();
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
            manager.add(Favorite::new("Station 1", "http://station1.com")).unwrap();
            manager.add(Favorite::new("Station 2", "http://station2.com")).unwrap();
            manager.save_to(&path).unwrap();
        }

        // Load and verify
        {
            let manager = FavoritesManager::load_from(&path).unwrap();
            assert_eq!(manager.count(), 2);
            assert!(manager.is_favorite("http://station1.com"));
            assert!(manager.is_favorite("http://station2.com"));
            assert_eq!(manager.get_by_url("http://station1.com").unwrap().name(), "Station 1");
        }

        // Cleanup
        let _ = fs::remove_file(&path);
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
        manager.add(Favorite::new("Test", "http://test.com")).unwrap();
        manager.save_to(&path).unwrap();
        assert!(path.exists());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_force_save() {
        let path = temp_path();

        let mut manager = FavoritesManager::new();
        manager.add(Favorite::new("Test", "http://test.com")).unwrap();
        manager.save_to(&path).unwrap(); // Clear dirty flag

        assert!(!manager.is_dirty());

        // Modify file externally wouldn't be detected, but force_save should work
        manager.force_save_to(&path).unwrap();
        assert!(!manager.is_dirty());

        let _ = fs::remove_file(&path);
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
            fav.play_count = 42;
            fav.total_listen_time_secs = 3600;
            fav.sort_order = 5;

            manager.add(fav).unwrap();
            manager.save_to(&path).unwrap();
        }

        // Load and verify all fields
        {
            let manager = FavoritesManager::load_from(&path).unwrap();
            let fav = manager.get_by_url(url).unwrap();

            assert_eq!(fav.name(), "Full Station");
            assert_eq!(fav.station.logo_url, Some("http://logo.com/img.png".to_string()));
            assert_eq!(fav.station.provider, "radio-browser");
            assert_eq!(fav.station.provider_id, Some("uuid-123".to_string()));
            assert_eq!(fav.station.country, Some("US".to_string()));
            assert_eq!(fav.station.language, Some("English".to_string()));
            assert!(fav.station.genres.contains("rock"));
            assert!(fav.station.genres.contains("pop"));
            assert_eq!(fav.station.codec, Some("MP3".to_string()));
            assert_eq!(fav.station.bitrate, Some(320));
            assert_eq!(fav.station.homepage, Some("http://station.com".to_string()));
            assert_eq!(fav.play_count, 42);
            assert_eq!(fav.total_listen_time_secs, 3600);
            assert_eq!(fav.sort_order, 5);
        }

        let _ = fs::remove_file(&path);
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
            manager.update(&id, FavoriteUpdate::new().name("Modified")).unwrap();
            manager.save_to(&path).unwrap();
        }

        // Verify modification persisted
        {
            let manager = FavoritesManager::load_from(&path).unwrap();
            assert_eq!(manager.get_by_url(url).unwrap().name(), "Modified");
        }

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_remove_and_save() {
        let path = temp_path();

        // Create with two favorites
        {
            let mut manager = FavoritesManager::new();
            manager.add(Favorite::new("Keep", "http://keep.com")).unwrap();
            manager.add(Favorite::new("Remove", "http://remove.com")).unwrap();
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

        let _ = fs::remove_file(&path);
    }
}
