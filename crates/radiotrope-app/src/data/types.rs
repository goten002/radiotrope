//! Common data types for persistence
//!
//! Shared types used across the data module.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

// =============================================================================
// HasLogo - Trait for types with cacheable logos
// =============================================================================

/// Trait for types that have a cacheable logo
///
/// This provides a uniform interface for caching logos across different
/// types (Station, Favorite, search results, etc.)
pub trait HasLogo {
    /// Get the cache key for this item's logo (typically the station ID)
    fn logo_cache_key(&self) -> String;

    /// Get the URL to fetch the logo from (if available)
    fn logo_url(&self) -> Option<&str>;
}

// =============================================================================
// Helper functions
// =============================================================================

/// Generate a deterministic ID from a URL
///
/// Using URL hash as ID provides:
/// - Deterministic: same URL always produces same ID
/// - Fast deduplication: check if ID exists without scanning
/// - Stable: ID doesn't change across sessions
pub fn url_to_id(url: &str) -> String {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// =============================================================================
// Station - Base type for radio station info
// =============================================================================

/// A radio station with its metadata
///
/// This is the base type used throughout the application for representing
/// a station. It can be used for search results, last played station,
/// and as the core data in Favorite.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Station {
    // === Basic Info ===
    /// Display name
    pub name: String,
    /// Stream URL
    pub url: String,
    /// Logo/favicon URL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,

    // === Metadata ===
    /// Country code (ISO 3166-1 alpha-2)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Primary language
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Genre tags from provider
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub genres: HashSet<String>,
    /// Audio codec (e.g., "MP3", "AAC", "OGG")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Bitrate in kbps
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
    /// Station homepage URL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,

    // === Provider Info ===
    /// Provider name (e.g., "radio-browser", "manual")
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Provider-specific station ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
}

fn default_provider() -> String {
    "manual".to_string()
}

impl Station {
    /// Create a new station with minimal info
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            logo_url: None,
            country: None,
            language: None,
            genres: HashSet::new(),
            codec: None,
            bitrate: None,
            homepage: None,
            provider: "manual".to_string(),
            provider_id: None,
        }
    }

    /// Get the deterministic ID for this station (based on URL hash)
    pub fn id(&self) -> String {
        url_to_id(&self.url)
    }

    /// Create with logo URL
    pub fn with_logo(mut self, logo_url: impl Into<String>) -> Self {
        self.logo_url = Some(logo_url.into());
        self
    }

    /// Set provider info
    pub fn with_provider(mut self, provider: impl Into<String>, provider_id: Option<String>) -> Self {
        self.provider = provider.into();
        self.provider_id = provider_id;
        self
    }

    /// Set metadata
    pub fn with_metadata(
        mut self,
        country: Option<String>,
        language: Option<String>,
        genres: HashSet<String>,
    ) -> Self {
        self.country = country;
        self.language = language;
        self.genres = genres;
        self
    }

    /// Set audio info
    pub fn with_audio_info(mut self, codec: Option<String>, bitrate: Option<u32>) -> Self {
        self.codec = codec;
        self.bitrate = bitrate;
        self
    }

    /// Set homepage
    pub fn with_homepage(mut self, homepage: impl Into<String>) -> Self {
        self.homepage = Some(homepage.into());
        self
    }

    /// Set logo URL from an Option (no-op if None)
    pub fn with_logo_opt(mut self, logo_url: Option<String>) -> Self {
        self.logo_url = logo_url;
        self
    }

    /// Set homepage from an Option (no-op if None)
    pub fn with_homepage_opt(mut self, homepage: Option<String>) -> Self {
        self.homepage = homepage;
        self
    }
}

impl HasLogo for Station {
    fn logo_cache_key(&self) -> String {
        self.id()
    }

    fn logo_url(&self) -> Option<&str> {
        self.logo_url.as_deref()
    }
}

// =============================================================================
// Favorite - A saved station with user-specific data
// =============================================================================

/// A favorite radio station with user-specific metadata
///
/// Extends Station with statistics and display preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Favorite {
    /// The station data
    #[serde(flatten)]
    pub station: Station,

    // === Statistics ===
    /// Number of times played
    #[serde(default)]
    pub play_count: u32,
    /// Total listening time in seconds
    #[serde(default)]
    pub total_listen_time_secs: u64,
    /// When the favorite was added (Unix timestamp)
    pub added_at: u64,
    /// Last played timestamp
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<u64>,

    // === Display ===
    /// Sort order (lower = higher priority)
    #[serde(default)]
    pub sort_order: i32,
}

impl Favorite {
    /// Create a new favorite from a station
    pub fn from_station(station: Station) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            station,
            play_count: 0,
            total_listen_time_secs: 0,
            added_at: now,
            last_played: None,
            sort_order: 0,
        }
    }

    /// Create a new favorite with minimal info
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self::from_station(Station::new(name, url))
    }

    /// Get the deterministic ID for this favorite (based on URL hash)
    pub fn id(&self) -> String {
        self.station.id()
    }

    /// Get the URL
    pub fn url(&self) -> &str {
        &self.station.url
    }

    /// Get the name
    pub fn name(&self) -> &str {
        &self.station.name
    }

    /// Create with logo URL
    pub fn with_logo(mut self, logo_url: impl Into<String>) -> Self {
        self.station.logo_url = Some(logo_url.into());
        self
    }

    /// Set provider info
    pub fn with_provider(mut self, provider: impl Into<String>, provider_id: Option<String>) -> Self {
        self.station = self.station.with_provider(provider, provider_id);
        self
    }

    /// Set metadata
    pub fn with_metadata(
        mut self,
        country: Option<String>,
        language: Option<String>,
        genres: HashSet<String>,
    ) -> Self {
        self.station = self.station.with_metadata(country, language, genres);
        self
    }

    /// Set audio info
    pub fn with_audio_info(mut self, codec: Option<String>, bitrate: Option<u32>) -> Self {
        self.station = self.station.with_audio_info(codec, bitrate);
        self
    }

    /// Record a play session
    pub fn record_play(&mut self, duration_secs: u64) {
        self.play_count += 1;
        self.total_listen_time_secs += duration_secs;
        self.last_played = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    }
}

impl HasLogo for Favorite {
    fn logo_cache_key(&self) -> String {
        self.id()
    }

    fn logo_url(&self) -> Option<&str> {
        self.station.logo_url.as_deref()
    }
}

// =============================================================================
// FavoriteUpdate - Partial update for favorites
// =============================================================================

/// Partial update for a favorite (only specified fields are updated)
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FavoriteUpdate {
    pub name: Option<String>,
    pub url: Option<String>,
    pub logo_url: Option<Option<String>>, // Option<Option> to allow clearing
    pub country: Option<Option<String>>,
    pub language: Option<Option<String>>,
    pub genres: Option<HashSet<String>>,
    pub codec: Option<Option<String>>,
    pub bitrate: Option<Option<u32>>,
    pub homepage: Option<Option<String>>,
    pub sort_order: Option<i32>,
}

impl FavoriteUpdate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    pub fn logo_url(mut self, logo_url: Option<String>) -> Self {
        self.logo_url = Some(logo_url);
        self
    }

    pub fn sort_order(mut self, order: i32) -> Self {
        self.sort_order = Some(order);
        self
    }

    /// Apply this update to a favorite
    pub fn apply_to(self, favorite: &mut Favorite) {
        if let Some(name) = self.name {
            favorite.station.name = name;
        }
        if let Some(url) = self.url {
            favorite.station.url = url;
        }
        if let Some(logo_url) = self.logo_url {
            favorite.station.logo_url = logo_url;
        }
        if let Some(country) = self.country {
            favorite.station.country = country;
        }
        if let Some(language) = self.language {
            favorite.station.language = language;
        }
        if let Some(genres) = self.genres {
            favorite.station.genres = genres;
        }
        if let Some(codec) = self.codec {
            favorite.station.codec = codec;
        }
        if let Some(bitrate) = self.bitrate {
            favorite.station.bitrate = bitrate;
        }
        if let Some(homepage) = self.homepage {
            favorite.station.homepage = homepage;
        }
        if let Some(sort_order) = self.sort_order {
            favorite.sort_order = sort_order;
        }
    }
}

// =============================================================================
// Sorting and Filtering
// =============================================================================

/// Sort criteria for favorites
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FavoriteSort {
    /// Manual sort order
    #[default]
    Manual,
    /// Alphabetical by name
    Name,
    /// Most recently added first
    RecentlyAdded,
    /// Most recently played first
    RecentlyPlayed,
    /// Most played first
    MostPlayed,
    /// Most listened time first
    MostListened,
}

/// Filter criteria for favorites
#[derive(Debug, Default, Clone)]
pub struct FavoriteFilter {
    /// Search in name
    pub search: Option<String>,
    /// Filter by genre
    pub genre: Option<String>,
    /// Filter by provider
    pub provider: Option<String>,
    /// Filter by country
    pub country: Option<String>,
}

impl FavoriteFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn search(mut self, search: impl Into<String>) -> Self {
        self.search = Some(search.into());
        self
    }

    pub fn genre(mut self, genre: impl Into<String>) -> Self {
        self.genre = Some(genre.into());
        self
    }

    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn country(mut self, country: impl Into<String>) -> Self {
        self.country = Some(country.into());
        self
    }

    /// Check if a favorite matches this filter
    pub fn matches(&self, favorite: &Favorite) -> bool {
        // Check search
        if let Some(ref search) = self.search {
            let search_lower = search.to_lowercase();
            if !favorite.station.name.to_lowercase().contains(&search_lower) {
                return false;
            }
        }

        // Check genre
        if let Some(ref genre) = self.genre {
            if !favorite.station.genres.contains(genre) {
                return false;
            }
        }

        // Check provider
        if let Some(ref provider) = self.provider {
            if &favorite.station.provider != provider {
                return false;
            }
        }

        // Check country
        if let Some(ref country) = self.country {
            if favorite.station.country.as_ref() != Some(country) {
                return false;
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_station_creation() {
        let station = Station::new("Test Radio", "http://example.com/stream");
        assert_eq!(station.name, "Test Radio");
        assert_eq!(station.url, "http://example.com/stream");
        assert_eq!(station.provider, "manual");
        assert!(!station.id().is_empty());
    }

    #[test]
    fn test_station_id_deterministic() {
        let station1 = Station::new("Radio 1", "http://example.com/stream");
        let station2 = Station::new("Radio 2", "http://example.com/stream");
        // Same URL = same ID, regardless of name
        assert_eq!(station1.id(), station2.id());
    }

    #[test]
    fn test_station_builder() {
        let station = Station::new("Test", "http://test.com")
            .with_logo("http://test.com/logo.png")
            .with_provider("radio-browser", Some("abc123".to_string()))
            .with_metadata(
                Some("US".to_string()),
                Some("English".to_string()),
                HashSet::from(["Rock".to_string(), "Pop".to_string()]),
            )
            .with_audio_info(Some("MP3".to_string()), Some(128))
            .with_homepage("http://test.com");

        assert_eq!(station.logo_url, Some("http://test.com/logo.png".to_string()));
        assert_eq!(station.provider, "radio-browser");
        assert_eq!(station.provider_id, Some("abc123".to_string()));
        assert_eq!(station.country, Some("US".to_string()));
        assert_eq!(station.genres.len(), 2);
        assert_eq!(station.codec, Some("MP3".to_string()));
        assert_eq!(station.bitrate, Some(128));
        assert_eq!(station.homepage, Some("http://test.com".to_string()));
    }

    #[test]
    fn test_favorite_from_station() {
        let station = Station::new("Test Radio", "http://example.com/stream")
            .with_logo("http://example.com/logo.png");

        let favorite = Favorite::from_station(station.clone());

        assert_eq!(favorite.station.name, "Test Radio");
        assert_eq!(favorite.station.url, "http://example.com/stream");
        assert_eq!(favorite.station.logo_url, Some("http://example.com/logo.png".to_string()));
        assert_eq!(favorite.play_count, 0);
        assert!(favorite.added_at > 0);
    }

    #[test]
    fn test_favorite_creation() {
        let fav = Favorite::new("Test Radio", "http://example.com/stream");
        assert_eq!(fav.name(), "Test Radio");
        assert_eq!(fav.url(), "http://example.com/stream");
        assert_eq!(fav.station.provider, "manual");
        assert!(!fav.id().is_empty());
    }

    #[test]
    fn test_favorite_builder() {
        let fav = Favorite::new("Test", "http://test.com")
            .with_logo("http://test.com/logo.png")
            .with_provider("radio-browser", Some("abc123".to_string()))
            .with_metadata(
                Some("US".to_string()),
                Some("English".to_string()),
                HashSet::from(["Rock".to_string(), "Pop".to_string()]),
            );

        assert_eq!(fav.station.logo_url, Some("http://test.com/logo.png".to_string()));
        assert_eq!(fav.station.provider, "radio-browser");
        assert_eq!(fav.station.provider_id, Some("abc123".to_string()));
        assert_eq!(fav.station.country, Some("US".to_string()));
        assert_eq!(fav.station.genres.len(), 2);
    }

    #[test]
    fn test_favorite_update() {
        let mut fav = Favorite::new("Old Name", "http://old.url");

        let update = FavoriteUpdate::new()
            .name("New Name")
            .sort_order(5);

        update.apply_to(&mut fav);

        assert_eq!(fav.station.name, "New Name");
        assert_eq!(fav.station.url, "http://old.url"); // unchanged
        assert_eq!(fav.sort_order, 5);
    }

    #[test]
    fn test_filter() {
        let fav = Favorite::new("Rock Radio", "http://rock.fm")
            .with_provider("radio-browser", None)
            .with_metadata(None, None, HashSet::from(["rock".to_string()]));

        let filter = FavoriteFilter::new().search("rock");
        assert!(filter.matches(&fav));

        let filter = FavoriteFilter::new().search("jazz");
        assert!(!filter.matches(&fav));

        let filter = FavoriteFilter::new().genre("rock");
        assert!(filter.matches(&fav));

        let filter = FavoriteFilter::new().genre("jazz");
        assert!(!filter.matches(&fav));

        let filter = FavoriteFilter::new().provider("radio-browser");
        assert!(filter.matches(&fav));

        let filter = FavoriteFilter::new().provider("manual");
        assert!(!filter.matches(&fav));
    }

    #[test]
    fn test_record_play() {
        let mut fav = Favorite::new("Test", "http://test.com");
        assert_eq!(fav.play_count, 0);
        assert_eq!(fav.total_listen_time_secs, 0);

        fav.record_play(300); // 5 minutes
        assert_eq!(fav.play_count, 1);
        assert_eq!(fav.total_listen_time_secs, 300);
        assert!(fav.last_played.is_some());

        fav.record_play(600); // 10 more minutes
        assert_eq!(fav.play_count, 2);
        assert_eq!(fav.total_listen_time_secs, 900);
    }

    #[test]
    fn test_station_with_logo_opt() {
        let station = Station::new("Test", "http://test.com")
            .with_logo_opt(Some("http://logo.png".to_string()));
        assert_eq!(station.logo_url, Some("http://logo.png".to_string()));

        let station = Station::new("Test", "http://test.com").with_logo_opt(None);
        assert_eq!(station.logo_url, None);
    }

    #[test]
    fn test_station_with_homepage_opt() {
        let station = Station::new("Test", "http://test.com")
            .with_homepage_opt(Some("http://homepage.com".to_string()));
        assert_eq!(station.homepage, Some("http://homepage.com".to_string()));

        let station = Station::new("Test", "http://test.com").with_homepage_opt(None);
        assert_eq!(station.homepage, None);
    }

    #[test]
    fn test_url_to_id_deterministic() {
        let url = "http://example.com/stream";
        let id1 = url_to_id(url);
        let id2 = url_to_id(url);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16); // 16 hex characters
    }
}
