//! Common data types for persistence
//!
//! Shared types used across the data module.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
/// - Stable: ID doesn't change across sessions or Rust versions (MCP clients
///   and the logo cache keep these ids, so `DefaultHasher` won't do)
pub fn url_to_id(url: &str) -> String {
    format!("{:016x}", fnv1a(url.as_bytes()))
}

/// The id builds before FNV-1a gave a URL (`DefaultHasher`, which the
/// standard library may change). Only for moving files that older builds
/// named by it, such as cached logos; never for new data.
pub fn legacy_url_to_id(url: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// A name for a stream that has none: its host (`stream.example.com`), or
/// the URL itself when it has no host
pub fn name_from_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// 64-bit FNV-1a, stable across Rust versions (unlike `DefaultHasher`)
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

// =============================================================================
// Station - Base type for radio station info
// =============================================================================

/// One of a station's streams: the same programme at another address,
/// codec or bitrate
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stream {
    pub url: String,
    /// Audio codec (e.g., "MP3", "AAC", "OGG"), when known
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Bitrate in kbps, when known
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
    /// An HLS stream (a playlist of segments)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hls: bool,
    /// How the directory last found it: "online", "suspect", "down" or
    /// "offline"; none for a stream the user added
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl Stream {
    /// A stream nothing is known about but its address
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }

    /// With its codec and bitrate
    pub fn with_audio_info(mut self, codec: Option<String>, bitrate: Option<u32>) -> Self {
        self.codec = codec;
        self.bitrate = bitrate;
        self
    }

    /// What it sounds like, for a list of streams: "AAC · 128 kbps", "MP3",
    /// "HLS · 96 kbps", or empty when nothing is known
    pub fn label(&self) -> String {
        let codec = self
            .codec
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("unknown"))
            .map(str::to_string)
            .or_else(|| self.hls.then(|| "HLS".to_string()));
        let bitrate = self.bitrate.filter(|b| *b > 0).map(|b| format!("{b} kbps"));
        [codec, bitrate]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(crate::text::SEPARATOR)
    }
}

/// A radio station with its metadata
///
/// This is the base type used throughout the application for representing
/// a station. It can be used for search results, last played station,
/// and as the core data in Favorite.
///
/// A station has one or more [`streams`](Self::streams), in order, and one
/// of them is its default: the one played, whose address is `url` (which
/// also gives the station its id) and whose `codec` and `bitrate` the
/// station shows. The others are tried in order when the default doesn't
/// connect. Change them with [`set_streams`](Self::set_streams) and
/// [`set_default`](Self::set_default), which keep these in step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(remote = "Self")]
pub struct Station {
    // === Basic Info ===
    /// Display name
    pub name: String,
    /// The default stream's URL
    pub url: String,
    /// Every stream of the station, in order, the default among them.
    /// Never empty: a station read without a list gets its `url` as one.
    #[serde(default)]
    pub streams: Vec<Stream>,
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
    /// Provider name (e.g., "radiotrope", "manual")
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Provider-specific station ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
}

fn default_provider() -> String {
    "manual".to_string()
}

impl Serialize for Station {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Station::serialize(self, serializer)
    }
}

/// A station is read with its streams in step (see [`Station::normalize`]):
/// one saved before stations had several gets its URL as its only stream
impl<'de> Deserialize<'de> for Station {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut station = Station::deserialize(deserializer)?;
        station.normalize();
        Ok(station)
    }
}

impl Station {
    /// Create a new station with minimal info
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            name: name.into(),
            streams: vec![Stream::new(url.clone())],
            url,
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
    pub fn with_provider(
        mut self,
        provider: impl Into<String>,
        provider_id: Option<String>,
    ) -> Self {
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

    /// Set the default stream's audio info
    pub fn with_audio_info(mut self, codec: Option<String>, bitrate: Option<u32>) -> Self {
        self.codec = codec;
        self.bitrate = bitrate;
        self.sync_default();
        self
    }

    /// The default stream gets the station's codec and bitrate
    fn sync_default(&mut self) {
        let at = self.default_index();
        if let Some(stream) = self.streams.get_mut(at) {
            stream.codec = self.codec.clone();
            stream.bitrate = self.bitrate;
        }
    }

    /// Give the station these streams, the first as its default; with
    /// none it keeps what it has
    pub fn with_streams(mut self, streams: Vec<Stream>) -> Self {
        if let Some(first) = streams.first().map(|s| s.url.clone()) {
            self.set_streams(streams, &first);
        }
        self
    }

    /// Where the default stream is in [`streams`](Self::streams)
    pub fn default_index(&self) -> usize {
        self.streams
            .iter()
            .position(|s| s.url == self.url)
            .unwrap_or(0)
    }

    /// The default stream
    pub fn default_stream(&self) -> Stream {
        self.streams
            .get(self.default_index())
            .cloned()
            .unwrap_or_else(|| Stream::new(self.url.clone()))
    }

    /// The streams tried, in order, when the default doesn't connect:
    /// the others, as they are listed
    pub fn backups(&self) -> Vec<String> {
        self.streams
            .iter()
            .filter(|s| s.url != self.url)
            .map(|s| s.url.clone())
            .collect()
    }

    /// Replace the streams, with the one at `default_url` as the default
    /// (the first when none has that address). Addresses are trimmed, and
    /// empty ones and repeats are left out. Returns false, changing
    /// nothing, when no stream is left.
    ///
    /// The station's id follows its default stream: see
    /// [`FavoritesManager::update`](crate::data::favorites::FavoritesManager::update)
    /// for changing a favorite's.
    pub fn set_streams(&mut self, streams: Vec<Stream>, default_url: &str) -> bool {
        let mut kept: Vec<Stream> = Vec::with_capacity(streams.len());
        for mut stream in streams {
            stream.url = stream.url.trim().to_string();
            if !stream.url.is_empty() && !kept.iter().any(|s| s.url == stream.url) {
                kept.push(stream);
            }
        }
        let default_url = default_url.trim();
        let Some(default) = kept
            .iter()
            .find(|s| s.url == default_url)
            .or(kept.first())
            .cloned()
        else {
            return false;
        };
        self.url = default.url;
        self.codec = default.codec;
        self.bitrate = default.bitrate;
        self.streams = kept;
        true
    }

    /// Make the stream at `url` the default. False when the station has
    /// no such stream.
    pub fn set_default(&mut self, url: &str) -> bool {
        if !self.streams.iter().any(|s| s.url == url) {
            return false;
        }
        let streams = std::mem::take(&mut self.streams);
        self.set_streams(streams, url)
    }

    /// Bring `streams` in step with `url`, `codec` and `bitrate` after the
    /// fields were set one by one (reading a file does this): a station
    /// without a list gets its URL as its only stream, a default missing
    /// from the list goes first in it, and the default's codec and bitrate
    /// are the station's.
    pub fn normalize(&mut self) {
        if !self.streams.iter().any(|s| s.url == self.url) {
            let default =
                Stream::new(self.url.clone()).with_audio_info(self.codec.clone(), self.bitrate);
            self.streams.insert(0, default);
        }
        let mut seen = HashSet::new();
        self.streams
            .retain(|s| s.url == self.url || !s.url.trim().is_empty());
        self.streams.retain(|s| seen.insert(s.url.clone()));
        let at = self.default_index();
        let default = &mut self.streams[at];
        // What the station says wins: an old file has it only there
        if self.codec.is_some() {
            default.codec = self.codec.clone();
        } else {
            self.codec = default.codec.clone();
        }
        if self.bitrate.is_some() {
            default.bitrate = self.bitrate;
        } else {
            self.bitrate = default.bitrate;
        }
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
/// Its plays and listening time are kept apart, in [`StationStats`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Favorite {
    /// The station data
    #[serde(flatten)]
    pub station: Station,

    /// When the favorite was added (Unix timestamp)
    pub added_at: u64,

    // === Display ===
    /// Sort order (lower = higher priority)
    #[serde(default)]
    pub sort_order: i32,
}

/// How much a station was listened to on this computer
///
/// Kept in its own file next to the favorites, so the favorites file
/// changes only when a favorite does, not every minute while music plays.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StationStats {
    /// Number of times played
    #[serde(default)]
    pub play_count: u32,
    /// Total listening time in seconds
    #[serde(default)]
    pub total_listen_time_secs: u64,
    /// Last played timestamp
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<u64>,
}

impl StationStats {
    /// Never played (or reset)
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Record a play session
    pub fn record_play(&mut self, duration_secs: u64) {
        self.add_listening(duration_secs, true);
    }

    /// Add listening time, counting a new play when `new_play` is set, and
    /// mark the station as played now. Long sessions are credited in steps,
    /// so only their first step counts as a play.
    pub fn add_listening(&mut self, secs: u64, new_play: bool) {
        // Listening after a stats reset counts as a play too
        if new_play || self.play_count == 0 {
            self.play_count += 1;
        }
        self.total_listen_time_secs += secs;
        self.last_played = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    }
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
            added_at: now,
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
    pub fn with_provider(
        mut self,
        provider: impl Into<String>,
        provider_id: Option<String>,
    ) -> Self {
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
    /// A new address for the default stream, in its place in the list
    pub url: Option<String>,
    /// The whole list of streams and the address of its default; `url`
    /// is ignored with it
    pub streams: Option<(Vec<Stream>, String)>,
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

    /// Replace the streams; `default_url` is the one to play
    pub fn streams(mut self, streams: Vec<Stream>, default_url: impl Into<String>) -> Self {
        self.streams = Some((streams, default_url.into()));
        self
    }

    /// The default stream's address once this is applied to `station`;
    /// `None` when the update leaves it alone
    pub fn new_url(&self, station: &Station) -> Option<String> {
        match (&self.streams, &self.url) {
            (Some((streams, default)), _) => {
                let mut after = station.clone();
                after
                    .set_streams(streams.clone(), default)
                    .then_some(after.url)
            }
            (None, Some(url)) => Some(url.clone()),
            (None, None) => None,
        }
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
        if let Some((streams, default)) = self.streams {
            favorite.station.set_streams(streams, &default);
        } else if let Some(url) = self.url {
            // The default stream moves to the new address, where it is
            // in the list
            let station = &mut favorite.station;
            let at = station.default_index();
            let mut streams = std::mem::take(&mut station.streams);
            if let Some(stream) = streams.get_mut(at) {
                if stream.url != url {
                    *stream = Stream::new(url.clone());
                }
            }
            if !station.set_streams(streams, &url) {
                station.streams = vec![Stream::new(station.url.clone())];
            }
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
        favorite.station.sync_default();
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
    fn a_stream_without_a_name_is_named_by_its_host() {
        assert_eq!(
            name_from_url("http://stream.example.com:8000/live.mp3"),
            "stream.example.com"
        );
        assert_eq!(name_from_url("https://[::1]/radio"), "[::1]");
        assert_eq!(name_from_url("not a url"), "not a url");
    }

    #[test]
    fn test_station_creation() {
        let station = Station::new("Test Radio", "http://example.com/stream");
        assert_eq!(station.name, "Test Radio");
        assert_eq!(station.url, "http://example.com/stream");
        assert_eq!(station.provider, "manual");
        assert!(!station.id().is_empty());
    }

    fn stream(url: &str, codec: &str, bitrate: u32) -> Stream {
        Stream::new(url).with_audio_info(Some(codec.into()), Some(bitrate))
    }

    fn three_streams() -> Station {
        Station::new("Jazz FM", "http://a.test").with_streams(vec![
            stream("http://a.test", "AAC", 128),
            stream("http://b.test", "MP3", 64),
            stream("http://c.test", "MP3", 32),
        ])
    }

    #[test]
    fn a_new_station_has_its_url_as_its_only_stream() {
        let station = Station::new("Test Radio", "http://example.com/stream");
        assert_eq!(
            station.streams,
            vec![Stream::new("http://example.com/stream")]
        );
        assert!(station.backups().is_empty());

        let station = station.with_audio_info(Some("MP3".into()), Some(128));
        assert_eq!(station.streams[0].codec.as_deref(), Some("MP3"));
        assert_eq!(station.streams[0].bitrate, Some(128));
    }

    #[test]
    fn the_first_of_the_streams_given_is_the_default() {
        let station = three_streams();
        assert_eq!(station.url, "http://a.test");
        assert_eq!(station.codec.as_deref(), Some("AAC"));
        assert_eq!(station.bitrate, Some(128));
        assert_eq!(station.default_index(), 0);
        assert_eq!(station.backups(), ["http://b.test", "http://c.test"]);
        // No streams given: it keeps the one it has
        let alone = Station::new("Alone", "http://alone.test").with_streams(Vec::new());
        assert_eq!(alone.streams, vec![Stream::new("http://alone.test")]);
    }

    #[test]
    fn picking_another_default_keeps_the_order_and_tries_the_others_in_it() {
        let mut station = three_streams();
        let id = station.id();
        assert!(station.set_default("http://b.test"));
        assert_eq!(station.url, "http://b.test");
        assert_eq!(station.codec.as_deref(), Some("MP3"));
        assert_eq!(station.bitrate, Some(64));
        assert_eq!(station.default_index(), 1);
        let urls: Vec<&str> = station.streams.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, ["http://a.test", "http://b.test", "http://c.test"]);
        assert_eq!(station.backups(), ["http://a.test", "http://c.test"]);
        // The id goes by the default stream
        assert_ne!(station.id(), id);

        // Not one of its streams: nothing changes
        assert!(!station.set_default("http://nowhere.test"));
        assert_eq!(station.url, "http://b.test");
    }

    #[test]
    fn streams_are_set_without_empty_or_repeated_addresses() {
        let mut station = three_streams();
        assert!(station.set_streams(
            vec![
                Stream::new("  http://x.test  "),
                Stream::new(""),
                stream("http://y.test", "OGG", 96),
                Stream::new("http://x.test"),
            ],
            "http://y.test",
        ));
        let urls: Vec<&str> = station.streams.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, ["http://x.test", "http://y.test"]);
        assert_eq!(station.url, "http://y.test");
        assert_eq!(station.codec.as_deref(), Some("OGG"));

        // A default that isn't there: the first; its codec isn't known
        assert!(station.set_streams(vec![Stream::new("http://z.test")], "http://gone.test"));
        assert_eq!(station.url, "http://z.test");
        assert_eq!((station.codec.clone(), station.bitrate), (None, None));

        // No stream left: refused, nothing changes
        assert!(!station.set_streams(vec![Stream::new("  ")], ""));
        assert_eq!(station.url, "http://z.test");
        assert_eq!(station.streams.len(), 1);
    }

    #[test]
    fn a_station_saved_with_one_url_reads_as_a_one_stream_list() {
        let old: Station = serde_json::from_str(
            r#"{"name":"Old FM","url":"http://old.test/live","codec":"MP3","bitrate":128}"#,
        )
        .unwrap();
        assert_eq!(
            old.streams,
            vec![stream("http://old.test/live", "MP3", 128)]
        );

        // And a favorite, which holds its station's fields beside its own
        let fav: Favorite = serde_json::from_str(
            r#"{"name":"Old FM","url":"http://old.test/live","added_at":5,"sort_order":2}"#,
        )
        .unwrap();
        assert_eq!(
            fav.station.streams,
            vec![Stream::new("http://old.test/live")]
        );
        assert_eq!(fav.sort_order, 2);
    }

    #[test]
    fn a_station_is_saved_with_all_its_streams_and_read_back_the_same() {
        let mut station = three_streams();
        station.set_default("http://c.test");
        let fav = Favorite::from_station(station);
        let text = serde_json::to_string(&fav).unwrap();
        assert!(text.contains(r#""url":"http://c.test""#), "{text}");
        assert!(text.contains("http://a.test") && text.contains("http://b.test"));
        let back: Favorite = serde_json::from_str(&text).unwrap();
        assert_eq!(back, fav);
        assert_eq!(back.station.default_index(), 2);
    }

    #[test]
    fn a_list_that_lacks_the_default_or_repeats_a_stream_is_put_right_when_read() {
        let station: Station = serde_json::from_str(
            r#"{"name":"Odd","url":"http://main.test","bitrate":96,"streams":[
                {"url":"http://b.test"},{"url":"http://b.test"},{"url":""}]}"#,
        )
        .unwrap();
        let urls: Vec<&str> = station.streams.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, ["http://main.test", "http://b.test"]);
        assert_eq!(station.streams[0].bitrate, Some(96));
    }

    #[test]
    fn a_stream_is_labelled_by_what_is_known_of_it() {
        assert_eq!(stream("u", "AAC", 128).label(), "AAC \u{b7} 128 kbps");
        assert_eq!(
            Stream::new("u")
                .with_audio_info(Some("MP3".into()), None)
                .label(),
            "MP3"
        );
        assert_eq!(
            Stream::new("u").with_audio_info(None, Some(64)).label(),
            "64 kbps"
        );
        let hls = Stream {
            hls: true,
            bitrate: Some(96),
            ..Stream::new("u")
        };
        assert_eq!(hls.label(), "HLS \u{b7} 96 kbps");
        assert_eq!(Stream::new("u").label(), "");
        assert_eq!(
            Stream::new("u")
                .with_audio_info(Some("unknown".into()), Some(0))
                .label(),
            ""
        );
    }

    #[test]
    fn an_update_replaces_the_streams_or_moves_the_default_one() {
        let mut fav = Favorite::from_station(three_streams());
        // Another default, one stream removed and one added
        let update = FavoriteUpdate::new().streams(
            vec![
                stream("http://a.test", "AAC", 128),
                stream("http://c.test", "MP3", 32),
                Stream::new("http://new.test"),
            ],
            "http://c.test",
        );
        assert_eq!(
            update.new_url(&fav.station).as_deref(),
            Some("http://c.test")
        );
        update.apply_to(&mut fav);
        assert_eq!(fav.url(), "http://c.test");
        assert_eq!(fav.station.bitrate, Some(32));
        assert_eq!(fav.station.backups(), ["http://a.test", "http://new.test"]);

        // A new address for the default stream keeps its place in the list
        FavoriteUpdate::new()
            .url("http://moved.test")
            .apply_to(&mut fav);
        let urls: Vec<&str> = fav.station.streams.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            urls,
            ["http://a.test", "http://moved.test", "http://new.test"]
        );
        assert_eq!(fav.url(), "http://moved.test");
        // What was known of the old address isn't true of the new one
        assert_eq!(
            (fav.station.codec.clone(), fav.station.bitrate),
            (None, None)
        );

        // The same address changes nothing
        let before = fav.clone();
        FavoriteUpdate::new()
            .url("http://moved.test")
            .apply_to(&mut fav);
        assert_eq!(fav, before);
        // And nor does a name alone
        assert_eq!(FavoriteUpdate::new().name("N").new_url(&fav.station), None);
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

        assert_eq!(
            station.logo_url,
            Some("http://test.com/logo.png".to_string())
        );
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
        assert_eq!(
            favorite.station.logo_url,
            Some("http://example.com/logo.png".to_string())
        );
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

        assert_eq!(
            fav.station.logo_url,
            Some("http://test.com/logo.png".to_string())
        );
        assert_eq!(fav.station.provider, "radio-browser");
        assert_eq!(fav.station.provider_id, Some("abc123".to_string()));
        assert_eq!(fav.station.country, Some("US".to_string()));
        assert_eq!(fav.station.genres.len(), 2);
    }

    #[test]
    fn test_favorite_update() {
        let mut fav = Favorite::new("Old Name", "http://old.url");

        let update = FavoriteUpdate::new().name("New Name").sort_order(5);

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
        let mut stats = StationStats::default();
        assert!(stats.is_empty());

        stats.record_play(300); // 5 minutes
        assert_eq!(stats.play_count, 1);
        assert_eq!(stats.total_listen_time_secs, 300);
        assert!(stats.last_played.is_some());

        stats.record_play(600); // 10 more minutes
        assert_eq!(stats.play_count, 2);
        assert_eq!(stats.total_listen_time_secs, 900);
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
    fn legacy_ids_match_what_older_builds_wrote() {
        // Pinned from the `DefaultHasher` ids older builds used. If this
        // fails, the standard library changed its hasher, and logos cached
        // by those builds can no longer be renamed at startup.
        assert_eq!(
            legacy_url_to_id("http://stream.example/live.mp3"),
            "28acedf85b0b8994"
        );
        assert_ne!(
            legacy_url_to_id("http://stream.example/live.mp3"),
            url_to_id("http://stream.example/live.mp3")
        );
    }

    #[test]
    fn test_url_to_id_deterministic() {
        let url = "http://example.com/stream";
        let id1 = url_to_id(url);
        let id2 = url_to_id(url);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16); // 16 hex characters
    }

    #[test]
    fn url_to_id_is_fixed_across_builds() {
        // Known FNV-1a values: ids saved by clients must never change
        assert_eq!(url_to_id(""), "cbf29ce484222325");
        assert_eq!(url_to_id("a"), "af63dc4c8601ec8c");
    }
}
