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

/// UI-related configuration. Sizes, colours and delays the UI draws with
/// are in `ui/defaults.slint`.
pub mod ui {
    use std::time::Duration;

    /// Search results page size
    pub const SEARCH_PAGE_SIZE: usize = 100;

    /// How often the menu bar's agents chip checks who uses the player
    pub const AGENTS_REFRESH: Duration = Duration::from_secs(1);

    /// How often the favorites file is checked for a save by another
    /// player (one an agent started with `--mcp --standalone`)
    pub const FAVORITES_FOLLOW: Duration = Duration::from_secs(1);

    /// How long "Saved ..." and recording errors stay on screen
    pub const RECORDING_NOTICE_TIME: Duration = Duration::from_secs(6);

    /// Listening shorter than this does not count (tuning through stations)
    pub const MIN_LISTEN_SECS: u64 = 30;

    /// Listening time is saved in steps of this long while a station plays
    pub const LISTEN_CREDIT_SECS: u64 = 60;
}

/// Agents over MCP
pub mod mcp {
    /// Where the network server listens unless the user picks another
    /// address: this computer only, on a port of our choosing
    pub const DEFAULT_ADDRESS: &str = "127.0.0.1:8765";

    /// File in the config folder holding the network token, readable by the
    /// user only
    pub const TOKEN_FILE: &str = "mcp-token";

    /// A network agent without an open event stream counts as there (the
    /// menu bar's agents chip) until this long after its last request
    pub const AGENT_IDLE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

    /// A network agent whose event stream closed is gone unless it opens
    /// another or makes a request within this long (a reconnect after a
    /// network hiccup takes a few seconds)
    pub const STREAM_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    /// How long a network agent's session lasts without a request. rmcp
    /// ends it after 5 minutes by default, open event stream or not, and
    /// the agent's next call then fails with "session not found"; agents
    /// often sit idle much longer than that between uses. Agents that quit
    /// cleanly end their session themselves.
    pub const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
}
