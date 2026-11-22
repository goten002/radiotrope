//! Configuration for station directory and favorites services.

/// Application metadata
pub mod app {
    /// Application name (used for config directory, etc.)
    pub const NAME: &str = "radiotrope";
}

/// Provider-related configuration
pub mod providers {
    use std::time::Duration;

    /// The Radiotrope station directory (see `providers::radiotrope`)
    pub const RADIOTROPE_API_URL: &str = "https://api.radiotrope.app/v1";

    /// Default search result limit
    pub const DEFAULT_SEARCH_LIMIT: usize = 100;

    /// How long cached genre, country and language lists stay fresh.
    /// They change only as stations are added or retired.
    pub const CATEGORY_CACHE_TTL: Duration = Duration::from_secs(24 * 3600);

    /// How long cached station lists (top, search, category pages) stay fresh.
    /// The directory publishes new station data every 15 minutes.
    pub const STATION_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

    /// Cached responses older than this are deleted (at startup and once a
    /// day). Until then they are still used when the directory cannot be
    /// reached.
    pub const API_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
}

pub mod caches {
    use std::time::Duration;

    /// A temp file a write left behind (a crash before its rename) is
    /// deleted once it is this old. A write in progress never takes this
    /// long.
    pub const TEMP_FILE_MAX_AGE: Duration = Duration::from_secs(10 * 60);
}
