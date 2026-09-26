//! Configuration constants for radiotrope app services

/// Application metadata
pub mod app {
    /// Application name (used for config directory, etc.)
    pub const NAME: &str = "radiotrope";
}

/// Provider-related configuration
pub mod providers {
    use std::time::Duration;

    /// Default Radio Browser API server
    pub const RADIO_BROWSER_DEFAULT_SERVER: &str = "https://de1.api.radio-browser.info";

    /// Default search result limit
    pub const DEFAULT_SEARCH_LIMIT: usize = 100;

    /// How long cached genre, country and language lists stay fresh.
    /// Only their station counts change, and slowly.
    pub const CATEGORY_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

    /// How long cached station lists (top, search, category pages) stay fresh.
    /// Click counts, new stations and broken-stream checks change daily.
    pub const STATION_CACHE_TTL: Duration = Duration::from_secs(12 * 3600);

    /// Cached responses older than this are deleted at startup. Until then
    /// they are still used when the directory cannot be reached.
    pub const API_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
}

/// UI-related configuration
pub mod ui {
    /// Search results page size
    pub const SEARCH_PAGE_SIZE: usize = 100;
}
