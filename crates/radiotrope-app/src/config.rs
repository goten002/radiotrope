//! Configuration constants for radiotrope app services

/// Application metadata
pub mod app {
    /// Application name (used for config directory, etc.)
    pub const NAME: &str = "radiotrope";
}

/// Provider-related configuration
pub mod providers {
    /// Default Radio Browser API server
    pub const RADIO_BROWSER_DEFAULT_SERVER: &str = "https://de1.api.radio-browser.info";

    /// Default search result limit
    pub const DEFAULT_SEARCH_LIMIT: usize = 100;
}

/// UI-related configuration
pub mod ui {
    /// Search results page size
    pub const SEARCH_PAGE_SIZE: usize = 100;
}
