//! Configuration constants for radiotrope app services

/// Application metadata
pub mod app {
    /// Application name (used for config directory, etc.)
    pub const NAME: &str = "radiotrope";
}

/// Provider-related configuration
pub mod providers {
    use std::time::Duration;

    /// Radio Browser API servers to start from. More are learned from the
    /// server list of the first one that answers. `all.api` points (by DNS)
    /// at whichever servers radio-browser runs, in case the others are gone.
    /// The first is also the name cached responses are stored under.
    pub const RADIO_BROWSER_SERVERS: &[&str] = &[
        "https://de1.api.radio-browser.info",
        "https://all.api.radio-browser.info",
    ];

    /// How long the server list learned from radio-browser is used before
    /// it is asked for again
    pub const RADIO_BROWSER_SERVER_LIST_TTL: Duration = Duration::from_secs(24 * 3600);

    /// How long a server gets to answer the quick check made before it is
    /// picked, and the server list request
    pub const RADIO_BROWSER_SERVER_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
