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

    /// Cached responses older than this are deleted (at startup and once a
    /// day). Until then they are still used when the directory cannot be
    /// reached.
    pub const API_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
}

/// The disk caches (API responses, logos)
pub mod caches {
    use std::time::Duration;

    /// How often the caches are trimmed while the app runs (they are also
    /// trimmed at startup)
    pub const PRUNE_EVERY: Duration = Duration::from_secs(24 * 3600);

    /// A temp file a write left behind (a crash before its rename) is
    /// deleted once it is this old. A write in progress never takes this
    /// long.
    pub const TEMP_FILE_MAX_AGE: Duration = Duration::from_secs(10 * 60);

    /// The cached logo of a station that isn't a favorite is kept at least
    /// this long: it may be the one playing, or about to be starred
    pub const ORPHAN_LOGO_MIN_AGE: Duration = Duration::from_secs(3600);
}

/// Station logos
pub mod logos {
    use std::time::Duration;

    /// Largest logo download. Logo URLs are user-submitted, and some point
    /// at streams, web pages or huge images.
    pub const MAX_BYTES: u64 = 2 * 1024 * 1024;

    /// Largest width or height of an image decoded as a logo
    pub const MAX_DIMENSION: u32 = 4096;

    /// Most memory one logo's decoding may take (a 4096 x 4096 RGBA image)
    pub const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;

    /// A logo that failed for a passing reason (no network, a timeout, a
    /// server error) is tried again after this long. One the server
    /// refused, or that isn't a usable image, isn't tried again this
    /// session.
    pub const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
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

    /// Listening time added to a favorite is saved this long after, on a
    /// thread of its own
    pub const FAVORITES_SAVE_DELAY: Duration = Duration::from_secs(2);

    /// The most listening time one tick of the UI's poll can add. The poll
    /// runs every 200 ms; a longer gap is the computer asleep (on Windows
    /// the clock keeps counting through sleep) or a stalled UI.
    pub const LISTEN_TICK_MAX: Duration = Duration::from_secs(5);

    /// How long closing the window waits for room to tell the controller
    /// to shut down. A controller that doesn't take it in time is stuck
    /// (e.g. on an audio device that doesn't answer).
    pub const SHUTDOWN_SEND_TIMEOUT: Duration = Duration::from_secs(1);

    /// The longest wait at exit for the controller to finish: the recorder
    /// may take its whole stop timeout to finish a file on a slow drive,
    /// and the engine then shuts down
    pub const SHUTDOWN_GRACE: Duration =
        radiotrope::audio::recording::STOP_TIMEOUT.saturating_add(Duration::from_secs(2));
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

    /// A session that has neither opened its event stream nor called a tool
    /// ends this long after its last request, so clients that only say
    /// hello don't hold a place for [`SESSION_IDLE`]
    pub const UNUSED_SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

    /// Most network sessions at once; one more is refused until one ends
    pub const MAX_SESSIONS: usize = 32;

    /// Most network agents the agents chip lists; the one seen longest ago
    /// makes way for a new one
    pub const MAX_NETWORK_AGENTS: usize = 32;

    /// Most connections the network server serves at once; more wait until
    /// one closes. An agent's event stream holds one.
    pub const MAX_CONNECTIONS: usize = 64;

    /// How long a connection has to send a request's headers, which is
    /// also how long an idle one stays open between requests
    pub const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// How often the network server checks that it still listens on the
    /// picked interface's address, or tries again after it could not
    /// listen (the network may not have been up yet)
    pub const NETWORK_RECHECK: std::time::Duration = std::time::Duration::from_secs(20);
}
