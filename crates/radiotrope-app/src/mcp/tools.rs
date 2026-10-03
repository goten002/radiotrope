//! MCP tools
//!
//! Each tool is a method on [`RadioTools`]; the `#[tool]` attributes give
//! the name, title, description and behaviour hints clients show and use.
//! Tools that return data return it as structured JSON (with an output
//! schema) and as the same JSON in text, for clients that only read text.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::service::RequestContext;
use rmcp::{schemars, tool, tool_router, RoleServer};
use serde::{Deserialize, Deserializer, Serialize};

use radiotrope::audio::PlaybackState;
use radiotrope_app::config::ui::SEARCH_PAGE_SIZE;
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::recordings;
use radiotrope_app::data::schedule::{self, Action, ClockTime, Days, End, Entry, ScheduledStation};
use radiotrope_app::data::types::{url_to_id, Favorite, FavoriteSort, Station};
use radiotrope_app::providers::{CategoryType, ProviderRegistry, SearchOrder, StationFilter};

use super::presence::{NetworkAgent, Place, Presence};
use crate::app::state::{AppCommand, AppSnapshot};

/// Default number of results returned by search_stations
const DEFAULT_SEARCH_LIMIT: usize = 20;
/// How long play tools wait for the station to start, unless told otherwise
const DEFAULT_PLAY_WAIT: Duration = Duration::from_secs(10);
/// Longest wait a play tool accepts
const MAX_PLAY_WAIT_SECS: f64 = 30.0;
/// How long recording tools wait for the recorder to answer
const RECORDING_WAIT: Duration = Duration::from_secs(3);
/// How often waiting tools look at the player's state
const POLL: Duration = Duration::from_millis(100);
/// Default and largest number of categories list_categories returns
const DEFAULT_CATEGORY_LIMIT: usize = 50;
const MAX_CATEGORY_LIMIT: usize = 500;
/// Longest station name or country an agent may give, in characters
const MAX_NAME_CHARS: usize = 512;
/// Longest stream or logo URL an agent may give, in characters
const MAX_URL_CHARS: usize = 2048;
/// add_favorite stops adding at this many favorites
const MAX_FAVORITES: usize = 1000;
/// How long schedule tools wait for the player to answer
const SCHEDULE_WAIT: Duration = Duration::from_secs(3);
/// What a tool says when the player's command queue is full
const PLAYER_BUSY: &str = "The player is busy; try again in a moment";

/// Everything the tools reach into: the controller's command channel and
/// the state and favorites shared with the GUI
#[derive(Clone)]
pub struct RadioTools {
    cmd_tx: Sender<AppCommand>,
    state: Arc<Mutex<AppSnapshot>>,
    favorites: Arc<Mutex<FavoritesManager>>,
    /// Built on first search; shares its HTTP client and server list
    providers: Arc<Mutex<Option<Arc<ProviderRegistry>>>>,
    /// Where favorites are saved; `None` is the usual data folder
    favorites_file: Option<std::path::PathBuf>,
    /// The last change an agent made, shared by every agent's session
    last_change: Arc<Mutex<Option<LastChange>>>,
    /// The agents using the player, for the menu bar's agents chip
    presence: Presence,
    /// Set on a local agent's own copy; network agents are told apart by
    /// their address, which comes with each request
    place: Option<Place>,
    pub(super) tool_router: ToolRouter<Self>,
}

/// Who changed the player last, and what they did
#[derive(Debug, Clone)]
struct LastChange {
    by: String,
    action: String,
    at: Instant,
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlayUrlArgs {
    /// Station stream URL (http or https), e.g. from search_stations
    pub url: String,
    /// Display name for the station
    #[serde(default)]
    pub name: Option<String>,
    /// Seconds to wait for the station to start playing (0-30, default 10;
    /// 0 returns at once)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "wait_schema")]
    pub wait_seconds: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlayStationArgs {
    /// Station id from search_stations
    pub id: String,
    /// Seconds to wait for the station to start playing (0-30, default 10;
    /// 0 returns at once)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "wait_schema")]
    pub wait_seconds: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlayFavoriteArgs {
    /// Favorite id, from list_favorites
    pub id: String,
    /// Seconds to wait for the station to start playing (0-30, default 10;
    /// 0 returns at once)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "wait_schema")]
    pub wait_seconds: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetMutedArgs {
    /// true to mute, false to unmute
    pub muted: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetVolumeArgs {
    /// Volume from 0 (silent) to 100 (full)
    #[serde(deserialize_with = "lenient_number")]
    #[schemars(schema_with = "volume_schema")]
    pub volume: f64,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Words to match against station names
    #[serde(default)]
    pub query: Option<String>,
    /// Genre tag, e.g. "jazz" (list_categories lists them)
    #[serde(default)]
    pub genre: Option<String>,
    /// Country name ("Greece") or two-letter code ("GR")
    #[serde(default)]
    pub country: Option<String>,
    /// Language, e.g. "greek"
    #[serde(default)]
    pub language: Option<String>,
    /// Codec, e.g. "MP3", "AAC", "OGG"
    #[serde(default)]
    pub codec: Option<String>,
    /// Lowest bitrate in kbps
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "bitrate_schema")]
    pub min_bitrate: Option<f64>,
    /// Order of the results (default: popular)
    #[serde(default)]
    pub order: Option<OrderArg>,
    /// Maximum number of stations to return (1-100, default 20)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "limit_schema")]
    pub limit: Option<f64>,
    /// Stations to skip, for the next page
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "offset_schema")]
    pub offset: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OrderArg {
    /// Most played first
    Popular,
    /// Most voted first
    Votes,
    /// Rising fastest first
    Trending,
    /// Highest bitrate first
    Bitrate,
    /// A to Z
    Name,
}

impl From<OrderArg> for SearchOrder {
    fn from(order: OrderArg) -> Self {
        match order {
            OrderArg::Popular => SearchOrder::Popular,
            OrderArg::Votes => SearchOrder::Votes,
            OrderArg::Trending => SearchOrder::Trending,
            OrderArg::Bitrate => SearchOrder::Bitrate,
            OrderArg::Name => SearchOrder::Name,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CategoryKind {
    Genre,
    Country,
    Language,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListCategoriesArgs {
    /// Which list: genres, countries or languages
    pub kind: CategoryKind,
    /// Only names containing this text
    #[serde(default)]
    pub filter: Option<String>,
    /// Maximum number to return, largest first (1-500, default 50)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "category_limit_schema")]
    pub limit: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddFavoriteArgs {
    /// Station stream URL
    pub url: String,
    /// Station display name
    pub name: String,
    /// Country name or code
    #[serde(default)]
    pub country: Option<String>,
    /// Station logo URL
    #[serde(default)]
    pub logo_url: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RemoveFavoriteArgs {
    /// Favorite id, from list_favorites (give this or url)
    #[serde(default)]
    pub id: Option<String>,
    /// Station stream URL (give this or id)
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SleepTimerArgs {
    /// Stop playback this many minutes from now (1-1440); 0 turns the
    /// timer off
    #[serde(deserialize_with = "lenient_number")]
    #[schemars(schema_with = "sleep_minutes_schema")]
    pub minutes: f64,
    /// Lower the volume over the last minute (default true)
    #[serde(default)]
    pub fade: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActionArg {
    /// Play the station (an alarm)
    Play,
    /// Play the station and record it (needs an end)
    Record,
    /// Stop whatever plays (bedtime)
    Stop,
}

impl From<ActionArg> for Action {
    fn from(action: ActionArg) -> Self {
        match action {
            ActionArg::Play => Action::Play,
            ActionArg::Record => Action::Record,
            ActionArg::Stop => Action::Stop,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddScheduleArgs {
    /// play, record or stop
    pub action: ActionArg,
    /// Start time, 24-hour "HH:MM" in the player's local time
    pub start: String,
    /// Days it repeats: "mon".."sun", or "daily", "weekdays", "weekends".
    /// None: once, at the next time `start` comes round (or on `date`).
    #[serde(default)]
    pub days: Vec<String>,
    /// For a one-off: the day, "YYYY-MM-DD"
    #[serde(default)]
    pub date: Option<String>,
    /// Station to play or record: a favorite id from list_favorites (give
    /// this or url)
    #[serde(default)]
    pub favorite_id: Option<String>,
    /// Station stream URL (give this or favorite_id)
    #[serde(default)]
    pub url: Option<String>,
    /// Station display name, with url
    #[serde(default)]
    pub name: Option<String>,
    /// End after this many minutes (1-1440)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "length_schema")]
    pub end_after_minutes: Option<f64>,
    /// End at this time, "HH:MM" (one before the start is the next day)
    #[serde(default)]
    pub end_at: Option<String>,
    /// Volume to play at, 0-100; leave out to keep the current one
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "optional_volume_schema")]
    pub volume: Option<f64>,
    /// play: rise from silence over 30 s; stop: fade out over a minute
    #[serde(default)]
    pub fade: bool,
    /// play: beep instead if the station hasn't started after 30 s (it is
    /// down or there is no internet). On unless set to false.
    #[serde(default)]
    pub fallback: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScheduleIdArgs {
    /// Entry id, from list_schedule
    #[serde(deserialize_with = "lenient_number")]
    #[schemars(schema_with = "entry_id_schema")]
    pub id: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EnableScheduleArgs {
    /// Entry id, from list_schedule
    #[serde(deserialize_with = "lenient_number")]
    #[schemars(schema_with = "entry_id_schema")]
    pub id: f64,
    /// true to switch it on, false to switch it off
    pub enabled: bool,
}

fn sleep_minutes_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 0,
        "maximum": schedule::MAX_MINUTES,
        "description": "Minutes until playback stops (1-1440); 0 turns the timer off"
    })
}

fn length_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 1,
        "maximum": schedule::MAX_MINUTES,
        "description": "End after this many minutes (1-1440)"
    })
}

fn optional_volume_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 0,
        "maximum": 100,
        "description": "Volume to play at, 0-100; leave out to keep the current one"
    })
}

fn entry_id_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 1,
        "description": "Entry id, from list_schedule"
    })
}

fn volume_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 0,
        "maximum": 100,
        "description": "Volume from 0 (silent) to 100 (full)"
    })
}

fn wait_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 0,
        "maximum": 30,
        "description": "Seconds to wait for the station to start playing (0-30, default 10; 0 returns at once)"
    })
}

fn bitrate_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 0,
        "description": "Lowest bitrate in kbps"
    })
}

fn offset_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 0,
        "description": "Stations to skip, for the next page"
    })
}

fn category_limit_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_CATEGORY_LIMIT,
        "description": "Maximum number to return, largest first (1-500, default 50)"
    })
}

fn limit_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 1,
        "maximum": SEARCH_PAGE_SIZE,
        "description": "Maximum number of stations to return (1-100, default 20)"
    })
}

/// Clients often send numbers as strings (`"45"`), so accept both. `"NaN"`
/// and `"inf"` parse as floats and are refused.
fn lenient_number<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrText {
        Number(f64),
        Text(String),
    }
    let value = match NumberOrText::deserialize(d)? {
        NumberOrText::Number(n) => Some(n),
        NumberOrText::Text(s) => s.trim().parse::<f64>().ok(),
    };
    value
        .filter(|v| v.is_finite())
        .ok_or_else(|| serde::de::Error::custom("expected a number"))
}

fn lenient_optional_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    lenient_number(d).map(Some)
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// The player's state, as get_status returns it
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Status {
    /// "stopped", "resolving", "playing" or "paused"
    pub playback: String,
    /// The station playing or starting, if any
    pub station: Option<StationRef>,
    /// Song title from the stream, if the station sends one
    pub title: Option<String>,
    /// Artist from the stream, if the station sends one
    pub artist: Option<String>,
    /// Volume from 0 to 100
    pub volume: u8,
    pub muted: bool,
    /// Codec and format of the stream playing
    pub stream: Option<StreamInfo>,
    /// The recording in progress, if any
    pub recording: Option<RecordingInfo>,
    /// When the sleep timer stops playback, "HH:MM" local time
    pub sleep_timer_until: Option<String>,
    /// The scheduled entry playing or recording now, e.g. "Play Jazz FM"
    pub scheduled_now: Option<String>,
    /// The next scheduled entry, e.g. "Tomorrow 07:00: Play Jazz FM"
    pub next_scheduled: Option<String>,
    /// The last error, e.g. why a station failed to start
    pub last_error: Option<String>,
    /// The last change an agent made (changes made in the window are not
    /// listed). Agents share the player, so another one may have acted.
    pub last_agent_change: Option<AgentChange>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AgentChange {
    /// The agent's client name
    pub by: String,
    /// What it did, e.g. "play Jazz FM"
    pub action: String,
    pub seconds_ago: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StationRef {
    pub name: Option<String>,
    pub url: Option<String>,
    /// Favorite id when the station is a favorite
    pub favorite_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StreamInfo {
    pub codec: String,
    /// e.g. "ICY", "HLS"
    #[serde(rename = "type")]
    pub stream_type: String,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordingInfo {
    pub path: String,
    pub seconds: u64,
    pub bytes: u64,
}

/// A station found by search_stations
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FoundStation {
    /// Id for play_station
    pub id: Option<String>,
    pub name: String,
    /// Stream URL to pass to play_url or add_favorite
    pub url: String,
    pub country: Option<String>,
    pub language: Option<String>,
    /// Genre tags
    pub genres: Vec<String>,
    pub codec: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub homepage: Option<String>,
    pub logo_url: Option<String>,
    /// Favorite id when the station is already a favorite
    pub favorite_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchResult {
    pub count: usize,
    /// More may follow: search again with offset + count
    pub has_more: bool,
    pub stations: Vec<FoundStation>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CategoryItem {
    /// Pass this as genre, country or language to search_stations
    pub name: String,
    /// Two-letter country code, for countries
    pub code: Option<String>,
    pub stations: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CategoryList {
    pub kind: CategoryKind,
    pub categories: Vec<CategoryItem>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FavoriteItem {
    /// Id for play_favorite and remove_favorite; stays the same for a URL
    pub id: String,
    pub name: String,
    pub url: String,
    pub country: Option<String>,
    pub genres: Vec<String>,
    pub play_count: u32,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FavoritesList {
    /// In the user's own order
    pub favorites: Vec<FavoriteItem>,
}

/// One scheduled entry, as list_schedule returns it
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ScheduleItem {
    /// Id for remove_schedule_entry and set_schedule_entry_enabled
    pub id: u64,
    pub enabled: bool,
    /// "play", "record" or "stop"
    pub action: String,
    /// The station played or recorded
    pub station: Option<String>,
    pub station_url: Option<String>,
    /// "HH:MM"
    pub start: String,
    /// Days it repeats ("mon".."sun"); empty for a one-off
    pub days: Vec<String>,
    /// A one-off's day, "YYYY-MM-DD"
    pub date: Option<String>,
    /// e.g. "Weekdays · for 1 h · fade in"
    pub summary: String,
    /// When it next comes round, "YYYY-MM-DD HH:MM" local time
    pub next: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ScheduleList {
    /// Ordered by start time
    pub entries: Vec<ScheduleItem>,
    /// When the sleep timer stops playback, "HH:MM" local time
    pub sleep_timer_until: Option<String>,
}

fn schedule_item(entry: &Entry, now: &chrono::DateTime<chrono::Local>) -> ScheduleItem {
    const IDS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    ScheduleItem {
        id: entry.id,
        enabled: entry.enabled,
        action: entry.action.id().into(),
        station: entry.station.as_ref().map(|s| s.name.clone()),
        station_url: entry.station.as_ref().map(|s| s.url.clone()),
        start: entry.start.to_string(),
        days: entry
            .days
            .flags()
            .iter()
            .zip(IDS)
            .filter(|(on, _)| **on)
            .map(|(_, id)| id.to_string())
            .collect(),
        date: entry
            .date
            .filter(|_| entry.days.is_once())
            .map(|d| d.format("%Y-%m-%d").to_string()),
        summary: entry.details(),
        next: entry
            .next_start(now)
            .map(|at| at.format("%Y-%m-%d %H:%M").to_string()),
    }
}

/// "mon".."sun" (or the full names), "daily", "weekdays", "weekends"
fn parse_days(days: &[String]) -> Result<Days, String> {
    let mut flags = [false; 7];
    for day in days {
        let day = day.trim().to_lowercase();
        match day.as_str() {
            "daily" | "every day" | "everyday" | "all" => flags = [true; 7],
            "weekdays" => flags[..5].fill(true),
            "weekends" => flags[5..].fill(true),
            _ => {
                let i = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
                    .iter()
                    .position(|d| day.starts_with(d))
                    .ok_or_else(|| format!("Not a day: {day}"))?;
                flags[i] = true;
            }
        }
    }
    Ok(Days::from_flags(flags))
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn sorted_genres(genres: &HashSet<String>) -> Vec<String> {
    let mut genres: Vec<String> = genres.iter().cloned().collect();
    genres.sort();
    genres
}

fn playback_name(s: &AppSnapshot) -> &'static str {
    if s.is_resolving {
        return "resolving";
    }
    match s.playback {
        PlaybackState::Stopped => "stopped",
        PlaybackState::Playing => "playing",
        PlaybackState::Paused => "paused",
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router]
impl RadioTools {
    pub fn new(
        cmd_tx: Sender<AppCommand>,
        state: Arc<Mutex<AppSnapshot>>,
        favorites: Arc<Mutex<FavoritesManager>>,
    ) -> Self {
        Self {
            cmd_tx,
            state,
            favorites,
            providers: Arc::new(Mutex::new(None)),
            favorites_file: None,
            last_change: Arc::new(Mutex::new(None)),
            presence: Presence::default(),
            place: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Search these providers and save favorites to this file (for tests)
    #[cfg(test)]
    pub fn with_test_setup(
        mut self,
        providers: ProviderRegistry,
        favorites_file: std::path::PathBuf,
    ) -> Self {
        self.providers = Arc::new(Mutex::new(Some(Arc::new(providers))));
        self.favorites_file = Some(favorites_file);
        self
    }

    #[tool(
        title = "Play a stream URL",
        description = "Play a radio station by its stream URL, and wait until it plays or \
                       fails (wait_seconds, default 10)",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn play_url(
        &self,
        Parameters(args): Parameters<PlayUrlArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let url = args.url.trim().to_string();
        if url.is_empty() {
            return Err("url must not be empty".into());
        }
        check_length("url", &url, MAX_URL_CHARS)?;
        if let Some(name) = &args.name {
            check_length("name", name, MAX_NAME_CHARS)?;
        }
        // Enrich from favorites, as the GUI does
        let request = PlayMetadata {
            url: url.clone(),
            name: args.name.clone(),
            ..Default::default()
        };
        let known = self
            .with_favorites(move |_, favorites| favorites.resolve_play(request))
            .await?;
        self.play(
            &ctx,
            url,
            known.name,
            known.logo_url,
            known.country,
            play_wait(args.wait_seconds),
        )
        .await
    }

    #[tool(
        title = "Play a station",
        description = "Play a station found by search_stations, by its id, and wait until it \
                       plays or fails (wait_seconds, default 10)",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn play_station(
        &self,
        Parameters(args): Parameters<PlayStationArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let id = args.id.trim().to_string();
        if id.is_empty() {
            return Err("id must not be empty".into());
        }
        let tools = self.clone();
        let station = tokio::task::spawn_blocking(move || tools.find_station(&id))
            .await
            .map_err(|e| format!("Lookup failed: {e}"))??;
        // Count the play in the directory, as the GUI does; nobody waits on it
        let tools = self.clone();
        let clicked = station.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(providers) = tools.providers() {
                if let Some(p) = providers.get(&clicked.provider) {
                    let _ = p.report_click(&clicked);
                }
            }
        });
        self.play(
            &ctx,
            station.url.clone(),
            Some(station.name.clone()),
            station.logo_url.clone(),
            station.country.clone(),
            play_wait(args.wait_seconds),
        )
        .await
    }

    #[tool(
        title = "Play a favorite",
        description = "Play a favorite station by its id (ids come from list_favorites), and \
                       wait until it plays or fails (wait_seconds, default 10)",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn play_favorite(
        &self,
        Parameters(args): Parameters<PlayFavoriteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let id = args.id.trim().to_string();
        let (url, name, logo, country) = self
            .with_favorites(move |_, favorites| {
                let fav = favorites.get(&id).ok_or_else(|| {
                    format!("No favorite with id {id}; list_favorites gives the ids")
                })?;
                Ok::<_, String>((
                    fav.url().to_string(),
                    fav.name().to_string(),
                    fav.station.logo_url.clone(),
                    fav.station.country.clone(),
                ))
            })
            .await??;
        self.play(
            &ctx,
            url,
            Some(name),
            logo,
            country,
            play_wait(args.wait_seconds),
        )
        .await
    }

    #[tool(
        title = "Stop",
        description = "Stop playback",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn stop(&self, ctx: RequestContext<RoleServer>) -> Result<String, String> {
        self.send(AppCommand::Stop)?;
        self.note_change(&ctx, "stop");
        Ok("Playback stopped".into())
    }

    #[tool(
        title = "Set volume",
        description = "Set the volume from 0 to 100. Unmutes when above 0.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_volume(
        &self,
        Parameters(args): Parameters<SetVolumeArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let volume = (args.volume as f32).clamp(0.0, 100.0) / 100.0;
        let was_muted = self.snapshot().is_muted;
        self.send(AppCommand::SetVolume(volume))?;
        {
            // Set it in the shared state now so the GUI shows it at once
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.volume = volume;
            if volume > 0.0 {
                s.is_muted = false;
            }
        }
        let percent = (volume * 100.0).round() as u8;
        self.note_change(&ctx, format!("set volume {percent}"));
        Ok(if was_muted && volume > 0.0 {
            format!("Volume set to {percent}% (unmuted)")
        } else {
            format!("Volume set to {percent}%")
        })
    }

    #[tool(
        title = "Mute or unmute",
        description = "Mute or unmute the sound, keeping the volume setting",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_muted(
        &self,
        Parameters(args): Parameters<SetMutedArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        if args.muted {
            self.send(AppCommand::Mute)?;
            self.note_change(&ctx, "mute");
            Ok("Muted".into())
        } else {
            self.send(AppCommand::Unmute)?;
            self.note_change(&ctx, "unmute");
            Ok("Unmuted".into())
        }
    }

    #[tool(
        title = "Player status",
        description = "What is playing: playback state, station, song, volume, stream format, \
                       recording, the last error and the last change an agent made",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_status(&self) -> Json<Status> {
        let s = self.snapshot();
        let favorite_id = match s.station_url.clone() {
            Some(url) => self
                .with_favorites(move |_, favorites| {
                    favorites.is_favorite(&url).then(|| url_to_id(&url))
                })
                .await
                .unwrap_or_default(),
            None => None,
        };
        let station = (s.station_name.is_some() || s.station_url.is_some()).then(|| StationRef {
            name: s.station_name.clone(),
            url: s.station_url.clone(),
            favorite_id,
        });
        let stream =
            (s.playback != PlaybackState::Stopped && !s.codec_name.is_empty()).then(|| {
                StreamInfo {
                    codec: s.codec_name.clone(),
                    stream_type: s.stream_type.clone(),
                    bitrate_kbps: s.bitrate,
                    sample_rate: s.sample_rate,
                    channels: s.channels,
                }
            });
        let last_agent_change = self
            .last_change
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .map(|c| AgentChange {
                by: c.by,
                action: c.action,
                seconds_ago: c.at.elapsed().as_secs(),
            });
        Json(Status {
            playback: playback_name(&s).into(),
            station,
            title: non_empty(&s.title),
            artist: non_empty(&s.artist),
            volume: (s.volume * 100.0).round() as u8,
            muted: s.is_muted,
            stream,
            recording: s.recording.as_ref().map(|r| RecordingInfo {
                path: r.path.display().to_string(),
                seconds: r.duration.as_secs(),
                bytes: r.bytes,
            }),
            sleep_timer_until: s
                .sleep
                .as_ref()
                .map(|t| t.until.format("%H:%M").to_string()),
            scheduled_now: s.scheduled.as_ref().map(|a| a.title.clone()),
            next_scheduled: s.next_run.as_ref().map(|n| {
                format!(
                    "{}: {}",
                    schedule::when_label(&n.at, &chrono::Local::now()),
                    n.title
                )
            }),
            last_error: s.last_error.clone(),
            last_agent_change,
        })
    }

    #[tool(
        title = "Search stations",
        description = "Search the radio-browser.info directory. Combine a name query with \
                       genre, country, language, codec and minimum bitrate; with nothing \
                       given it lists the most popular stations. Station names and tags come \
                       from that public directory.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn search_stations(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<Json<SearchResult>, String> {
        let limit = args
            .limit
            .map(|v| (v as usize).clamp(1, SEARCH_PAGE_SIZE))
            .unwrap_or(DEFAULT_SEARCH_LIMIT);
        let offset = args.offset.map(|v| v.max(0.0) as usize).unwrap_or(0);
        let text = |v: Option<String>| v.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let filter = StationFilter {
            name: text(args.query),
            genre: text(args.genre),
            country: text(args.country),
            language: text(args.language),
            codec: text(args.codec),
            min_bitrate: args.min_bitrate.map(|b| b.max(0.0) as u32),
            order: args.order.map(Into::into).unwrap_or_default(),
        };
        // Blocking HTTP (reqwest's blocking client, which must not even be
        // built inside the async runtime): off it, so other calls keep flowing
        let tools = self.clone();
        let results = tokio::task::spawn_blocking(move || {
            let providers = tools.providers()?;
            let mut stations = Vec::new();
            let mut has_more = false;
            let mut failed = None;
            for id in providers.list_ids() {
                let Some(provider) = providers.get(id) else {
                    continue;
                };
                match provider.search_filtered(&filter, limit, offset) {
                    Ok(r) => {
                        has_more |= r.has_more;
                        stations.extend(r.stations);
                    }
                    Err(e) => failed = Some(e),
                }
            }
            if let Some(e) = failed.filter(|_| stations.is_empty()) {
                return Err(format!("Search failed: {e}"));
            }
            let favorites = tools.favorites();
            let stations: Vec<FoundStation> = stations
                .into_iter()
                .map(|s: Station| FoundStation {
                    id: s.provider_id.clone().filter(|id| !id.is_empty()),
                    favorite_id: favorites.is_favorite(&s.url).then(|| url_to_id(&s.url)),
                    genres: sorted_genres(&s.genres),
                    name: s.name,
                    url: s.url,
                    country: s.country,
                    language: s.language,
                    codec: s.codec,
                    bitrate_kbps: s.bitrate.filter(|b| *b > 0),
                    homepage: s.homepage,
                    logo_url: s.logo_url,
                })
                .collect();
            Ok((stations, has_more))
        })
        .await
        .map_err(|e| format!("Search failed: {e}"))?;
        let (stations, has_more) = results?;
        Ok(Json(SearchResult {
            count: stations.len(),
            has_more,
            stations,
        }))
    }

    #[tool(
        title = "List genres, countries or languages",
        description = "List the genres, countries or languages of the radio-browser.info \
                       directory, largest first, to use as search_stations filters",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_categories(
        &self,
        Parameters(args): Parameters<ListCategoriesArgs>,
    ) -> Result<Json<CategoryList>, String> {
        let kind = args.kind;
        let wanted = match kind {
            CategoryKind::Genre => CategoryType::Genre,
            CategoryKind::Country => CategoryType::Country,
            CategoryKind::Language => CategoryType::Language,
        };
        let limit = args
            .limit
            .map(|v| (v as usize).clamp(1, MAX_CATEGORY_LIMIT))
            .unwrap_or(DEFAULT_CATEGORY_LIMIT);
        let filter = args
            .filter
            .map(|f| f.trim().to_lowercase())
            .filter(|f| !f.is_empty());
        let tools = self.clone();
        let categories = tokio::task::spawn_blocking(move || {
            let providers = tools.providers()?;
            let mut all = Vec::new();
            let mut failed = None;
            for id in providers.list_ids() {
                match providers.get(id).map(|p| p.browse_categories()) {
                    Some(Ok(c)) => all.extend(c),
                    Some(Err(e)) => failed = Some(e),
                    None => {}
                }
            }
            match failed {
                Some(e) if all.is_empty() => Err(format!("Listing failed: {e}")),
                _ => Ok(all),
            }
        })
        .await
        .map_err(|e| format!("Listing failed: {e}"))??;

        let mut categories: Vec<CategoryItem> = categories
            .into_iter()
            .filter(|c| c.category_type == wanted)
            .filter(|c| {
                filter.as_deref().is_none_or(|f| {
                    c.name.to_lowercase().contains(f)
                        || c.code
                            .as_deref()
                            .is_some_and(|code| code.eq_ignore_ascii_case(f))
                })
            })
            .map(|c| CategoryItem {
                name: c.name,
                code: c.code.filter(|code| !code.is_empty()),
                stations: c.station_count,
            })
            .collect();
        categories.sort_by_key(|c| std::cmp::Reverse(c.stations));
        categories.truncate(limit);
        Ok(Json(CategoryList { kind, categories }))
    }

    #[tool(
        title = "List favorites",
        description = "List the saved favorite stations, in the user's order, with their ids",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_favorites(&self) -> Result<Json<FavoritesList>, String> {
        let favorites = self
            .with_favorites(|_, favorites| {
                favorites
                    .sorted(FavoriteSort::Manual)
                    .into_iter()
                    .map(|fav| FavoriteItem {
                        id: fav.id(),
                        name: fav.name().to_string(),
                        url: fav.url().to_string(),
                        country: fav.station.country.clone(),
                        genres: sorted_genres(&fav.station.genres),
                        play_count: fav.play_count,
                    })
                    .collect()
            })
            .await?;
        Ok(Json(FavoritesList { favorites }))
    }

    #[tool(
        title = "Add a favorite",
        description = "Save a station to the favorites",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn add_favorite(
        &self,
        Parameters(args): Parameters<AddFavoriteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let url = args.url.trim().to_string();
        let name = args.name.trim().to_string();
        if url.is_empty() || name.is_empty() {
            return Err("url and name must not be empty".into());
        }
        check_length("url", &url, MAX_URL_CHARS)?;
        check_length("name", &name, MAX_NAME_CHARS)?;
        if let Some(logo) = &args.logo_url {
            check_length("logo_url", logo, MAX_URL_CHARS)?;
        }
        if let Some(country) = &args.country {
            check_length("country", country, MAX_NAME_CHARS)?;
        }
        let mut fav = Favorite::new(&name, &url);
        if let Some(logo) = args.logo_url.filter(|s| !s.is_empty()) {
            fav = fav.with_logo(logo);
        }
        if args.country.is_some() {
            fav = fav.with_metadata(args.country, None, HashSet::new());
        }
        let id = fav.id();
        self.with_favorites(move |tools, favorites| {
            if favorites.count() >= MAX_FAVORITES && !favorites.is_favorite(fav.url()) {
                return Err(format!(
                    "There are {MAX_FAVORITES} favorites already, the most an agent can add; \
                     remove some first"
                ));
            }
            favorites.add(fav).map_err(|e| e.to_string())?;
            tools
                .save_favorites(favorites)
                .map_err(|e| format!("Failed to save: {e}"))
        })
        .await??;
        self.note_change(&ctx, format!("add favorite {name}"));
        Ok(format!("Added \"{name}\" to favorites (id {id})"))
    }

    #[tool(
        title = "Remove a favorite",
        description = "Remove a station from the favorites, by id or URL",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn remove_favorite(
        &self,
        Parameters(args): Parameters<RemoveFavoriteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let id = match (args.id.as_deref(), args.url.as_deref()) {
            (Some(id), _) if !id.trim().is_empty() => id.trim().to_string(),
            (_, Some(url)) if !url.trim().is_empty() => url_to_id(url.trim()),
            _ => return Err("Give the favorite's id or url".into()),
        };
        let removed = self
            .with_favorites(move |tools, favorites| {
                let removed = favorites.remove(&id).map_err(|e| e.to_string())?;
                tools
                    .save_favorites(favorites)
                    .map_err(|e| format!("Removed but failed to save: {e}"))?;
                Ok::<_, String>(removed)
            })
            .await??;
        self.note_change(&ctx, format!("remove favorite {}", removed.name()));
        Ok(format!("Removed \"{}\" from favorites", removed.name()))
    }

    #[tool(
        title = "Start recording",
        description = "Record the station playing to a file, in the format and folder set in \
                       the player's recording settings",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn start_recording(&self, ctx: RequestContext<RoleServer>) -> Result<String, String> {
        let s = self.snapshot();
        if let Some(r) = &s.recording {
            return Ok(format!("Already recording to {}", r.path.display()));
        }
        if s.playback != PlaybackState::Playing {
            return Err("Nothing is playing; start a station first".into());
        }
        let notice_before = s.recording_notice.as_ref().map(|n| n.seq);
        // The settings as the window has them, not as the file may be
        // half-way through a save
        let setup = &s.recording_setup;
        self.send(AppCommand::StartRecording {
            folder: recordings::folder(setup.dir.as_deref()),
            format: setup.format,
            bitrate: setup.bitrate,
            // As in the window: the switch only counts with the EQ on
            with_eq: setup.with_eq && s.eq_enabled,
            cover: None,
        })?;
        self.note_change(&ctx, "start recording");
        let deadline = Instant::now() + RECORDING_WAIT;
        while Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let s = self.snapshot();
            if let Some(r) = &s.recording {
                return Ok(format!("Recording to {}", r.path.display()));
            }
            if let Some(n) = s.recording_notice.filter(|n| Some(n.seq) != notice_before) {
                if n.is_error {
                    return Err(n.text);
                }
            }
        }
        Ok("Recording is starting; get_status shows its file".into())
    }

    #[tool(
        title = "Stop recording",
        description = "Stop the recording in progress and save the file",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn stop_recording(&self, ctx: RequestContext<RoleServer>) -> Result<String, String> {
        let s = self.snapshot();
        let Some(recording) = s.recording else {
            return Ok("Not recording".into());
        };
        let notice_before = s.recording_notice.as_ref().map(|n| n.seq);
        self.send(AppCommand::StopRecording)?;
        self.note_change(&ctx, "stop recording");
        let deadline = Instant::now() + RECORDING_WAIT;
        while Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let s = self.snapshot();
            if let Some(n) = s.recording_notice.filter(|n| Some(n.seq) != notice_before) {
                return if n.is_error { Err(n.text) } else { Ok(n.text) };
            }
            if s.recording.is_none() {
                break;
            }
        }
        Ok(format!("Recording stopped: {}", recording.path.display()))
    }

    #[tool(
        title = "Timer",
        description = "Stop playback after some minutes, fading out over the last minute. \
                       0 minutes turns the timer off.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_sleep_timer(
        &self,
        Parameters(args): Parameters<SleepTimerArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let minutes = args.minutes.round();
        if !(0.0..=f64::from(schedule::MAX_MINUTES)).contains(&minutes) {
            return Err(format!(
                "minutes must be from 0 to {}",
                schedule::MAX_MINUTES
            ));
        }
        let minutes = minutes as u32;
        if minutes == 0 {
            self.send(AppCommand::SetSleepTimer {
                minutes: None,
                fade: true,
            })?;
            self.note_change(&ctx, "sleep timer off");
            return Ok("Sleep timer off".into());
        }
        self.send(AppCommand::SetSleepTimer {
            minutes: Some(minutes),
            fade: args.fade.unwrap_or(true),
        })?;
        self.note_change(&ctx, format!("sleep timer {minutes} min"));
        let until = chrono::Local::now() + chrono::TimeDelta::minutes(minutes.into());
        Ok(format!(
            "Playback stops in {} (at {})",
            schedule::duration_label(minutes),
            until.format("%H:%M")
        ))
    }

    #[tool(
        title = "List the Scheduler entries",
        description = "The scheduled alarms, recordings and bedtime stops, with when each \
                       comes round next, and the sleep timer",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_schedule(&self) -> Json<ScheduleList> {
        let s = self.snapshot();
        let now = chrono::Local::now();
        let mut entries = s.schedule.clone();
        entries.sort_by_key(|e| (e.start, e.id));
        Json(ScheduleList {
            entries: entries.iter().map(|e| schedule_item(e, &now)).collect(),
            sleep_timer_until: s
                .sleep
                .as_ref()
                .map(|t| t.until.format("%H:%M").to_string()),
        })
    }

    #[tool(
        title = "Add to the Scheduler",
        description = "Schedule an alarm (play), a recording (record, needs end_at or \
                       end_after_minutes) or a bedtime stop (stop) at a local time, once or on \
                       chosen days. Recordings use the player's recording settings. Only one \
                       recording can run at a time.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn add_schedule_entry(
        &self,
        Parameters(args): Parameters<AddScheduleArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let action: Action = args.action.into();
        let start = ClockTime::parse(&args.start)
            .ok_or_else(|| format!("start must be a time like 07:30, not {:?}", args.start))?;
        let days = parse_days(&args.days)?;
        let date = match args
            .date
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(text) => Some(
                chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                    .map_err(|_| format!("date must be YYYY-MM-DD, not {text:?}"))?,
            ),
            None => None,
        };
        let end = match (args.end_after_minutes, args.end_at.as_deref()) {
            (Some(_), Some(_)) => return Err("Give end_after_minutes or end_at, not both".into()),
            (Some(minutes), None) => {
                let minutes = minutes.round();
                if !(1.0..=f64::from(schedule::MAX_MINUTES)).contains(&minutes) {
                    return Err(format!(
                        "end_after_minutes must be from 1 to {}",
                        schedule::MAX_MINUTES
                    ));
                }
                End::After {
                    minutes: minutes as u32,
                }
            }
            (None, Some(text)) => End::At {
                time: ClockTime::parse(text)
                    .ok_or_else(|| format!("end_at must be a time like 22:00, not {text:?}"))?,
            },
            (None, None) => End::Never,
        };
        let volume = match args.volume {
            Some(v) if !(0.0..=100.0).contains(&v) => {
                return Err("volume must be from 0 to 100".into())
            }
            Some(v) => Some((v / 100.0) as f32),
            None => None,
        };
        let station = if action == Action::Stop {
            None
        } else {
            Some(self.schedule_station(&args).await?)
        };
        let entry = Entry {
            id: 0,
            enabled: true,
            action,
            station,
            start,
            days,
            date,
            end,
            volume,
            fade: args.fade,
            fallback: args.fallback.unwrap_or(true),
            armed_from: 0,
        };
        let title = entry.title();
        let id = self
            .ask_schedule(|reply| AppCommand::SaveScheduleEntry {
                entry,
                reply: Some(reply),
            })
            .await?;
        self.note_change(&ctx, format!("schedule {title} at {start}"));
        let now = chrono::Local::now();
        let saved = self.snapshot().schedule.into_iter().find(|e| e.id == id);
        Ok(match saved {
            Some(e) => {
                let next = e
                    .next_start(&now)
                    .map(|at| format!(", next {}", schedule::when_label(&at, &now)))
                    .unwrap_or_default();
                format!(
                    "Scheduled {title} at {start} ({}){next}. Id {id}",
                    e.details()
                )
            }
            None => format!("Scheduled {title} at {start}. Id {id}"),
        })
    }

    #[tool(
        title = "Remove from the Scheduler",
        description = "Remove a scheduled entry by its id (ids come from list_schedule)",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn remove_schedule_entry(
        &self,
        Parameters(args): Parameters<ScheduleIdArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let id = entry_id(args.id)?;
        let title = self.entry_title(id)?;
        self.ask_schedule(|reply| AppCommand::RemoveScheduleEntry {
            id,
            reply: Some(reply),
        })
        .await?;
        self.note_change(&ctx, format!("unschedule {title}"));
        Ok(format!("Removed {title} from the schedule"))
    }

    #[tool(
        title = "Switch a Scheduler entry on or off",
        description = "Switch a scheduled entry on or off without removing it",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_schedule_entry_enabled(
        &self,
        Parameters(args): Parameters<EnableScheduleArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let id = entry_id(args.id)?;
        let title = self.entry_title(id)?;
        let enabled = args.enabled;
        self.ask_schedule(|reply| AppCommand::SetScheduleEntryEnabled {
            id,
            enabled,
            reply: Some(reply),
        })
        .await?;
        let word = if enabled { "on" } else { "off" };
        self.note_change(&ctx, format!("switch {word} {title}"));
        Ok(format!("Switched {word}: {title}"))
    }
}

/// A whole, positive entry id
fn entry_id(id: f64) -> Result<u64, String> {
    if id >= 1.0 && id.fract() == 0.0 && id <= u64::MAX as f64 {
        Ok(id as u64)
    } else {
        Err("id must be an entry id from list_schedule".into())
    }
}

/// Refuse a text longer than `max` characters: it would be saved and drawn
fn check_length(what: &str, text: &str, max: usize) -> Result<(), String> {
    if text.chars().count() > max {
        return Err(format!("{what} is too long (at most {max} characters)"));
    }
    Ok(())
}

/// How long a play tool waits, from its wait_seconds argument
fn play_wait(seconds: Option<f64>) -> Duration {
    seconds
        .map(|s| Duration::from_secs_f64(s.clamp(0.0, MAX_PLAY_WAIT_SECS)))
        .unwrap_or(DEFAULT_PLAY_WAIT)
}

impl RadioTools {
    fn providers(&self) -> Result<Arc<ProviderRegistry>, String> {
        let mut providers = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = providers.as_ref() {
            return Ok(p.clone());
        }
        let registry = Arc::new(
            ProviderRegistry::with_defaults()
                .map_err(|e| format!("Failed to initialize provider: {e}"))?,
        );
        *providers = Some(registry.clone());
        Ok(registry)
    }

    /// A station by its directory id (blocking)
    fn find_station(&self, id: &str) -> Result<Station, String> {
        let providers = self.providers()?;
        let mut failed = None;
        for provider_id in providers.list_ids() {
            let Some(provider) = providers.get(provider_id) else {
                continue;
            };
            match provider.get_station(id) {
                Ok(Some(station)) => return Ok(station),
                Ok(None) => {}
                Err(e) => failed = Some(e),
            }
        }
        Err(match failed {
            Some(e) => format!("Lookup failed: {e}"),
            None => format!("No station with id {id}; search_stations gives the ids"),
        })
    }

    fn save_favorites(
        &self,
        favorites: &mut FavoritesManager,
    ) -> radiotrope_app::error::Result<()> {
        match &self.favorites_file {
            Some(path) => favorites.save_to(path),
            None => favorites.save(),
        }
    }

    /// The favorites, locked (blocking: the window may hold the lock while
    /// it draws them, and taking in another player's save reads the file)
    fn favorites(&self) -> std::sync::MutexGuard<'_, FavoritesManager> {
        let mut favorites = self.favorites.lock().unwrap_or_else(|e| e.into_inner());
        // What another player saved since: with `--mcp --standalone` the
        // agent's own player and the usual one share the file
        favorites.reload_if_changed();
        favorites
    }

    /// Run `work` on the favorites off the async thread, which every local
    /// agent shares: waiting for the lock or the disk there would hold them
    /// all up
    async fn with_favorites<R, F>(&self, work: F) -> Result<R, String>
    where
        F: FnOnce(&RadioTools, &mut FavoritesManager) -> R + Send + 'static,
        R: Send + 'static,
    {
        let tools = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut favorites = tools.favorites();
            work(&tools, &mut favorites)
        })
        .await
        .map_err(|e| format!("Favorites unavailable: {e}"))
    }

    /// The station a schedule entry is for: a favorite, or a URL
    async fn schedule_station(&self, args: &AddScheduleArgs) -> Result<ScheduledStation, String> {
        let favorite_id = args
            .favorite_id
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty());
        let url = args.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
        match (favorite_id, url) {
            (Some(_), Some(_)) => Err("Give favorite_id or url, not both".into()),
            (None, None) => Err("Give the station: favorite_id or url".into()),
            (Some(id), None) => {
                let id = id.to_string();
                self.with_favorites(move |_, favorites| {
                    let fav = favorites.get(&id).ok_or_else(|| {
                        format!("No favorite with id {id}; list_favorites gives the ids")
                    })?;
                    Ok(ScheduledStation {
                        name: fav.name().to_string(),
                        url: fav.url().to_string(),
                        logo_url: fav.station.logo_url.clone(),
                        country: fav.station.country.clone(),
                    })
                })
                .await?
            }
            (None, Some(url)) => {
                check_length("url", url, MAX_URL_CHARS)?;
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err("url must start with http:// or https://".into());
                }
                let name = args.name.as_deref().map(str::trim).unwrap_or_default();
                check_length("name", name, MAX_NAME_CHARS)?;
                let url = url.to_string();
                let name = if name.is_empty() {
                    radiotrope_app::data::types::name_from_url(&url)
                } else {
                    name.to_string()
                };
                // A favorite's logo and country come along
                let lookup = url.clone();
                let (logo_url, country) = self
                    .with_favorites(move |_, favorites| {
                        favorites
                            .get_by_url(&lookup)
                            .map(|f| (f.station.logo_url.clone(), f.station.country.clone()))
                            .unwrap_or_default()
                    })
                    .await
                    .unwrap_or_default();
                Ok(ScheduledStation {
                    name,
                    url,
                    logo_url,
                    country,
                })
            }
        }
    }

    /// The title of entry `id`, or why there is none
    fn entry_title(&self, id: u64) -> Result<String, String> {
        self.snapshot()
            .schedule
            .iter()
            .find(|e| e.id == id)
            .map(Entry::title)
            .ok_or_else(|| format!("No schedule entry with id {id}; list_schedule gives the ids"))
    }

    /// Send a schedule change and wait for the player's answer
    async fn ask_schedule<T>(
        &self,
        command: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> AppCommand,
    ) -> Result<T, String> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(command(reply))?;
        match tokio::time::timeout(SCHEDULE_WAIT, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("The player has stopped".into()),
            Err(_) => Err(PLAYER_BUSY.into()),
        }
    }

    fn snapshot(&self) -> AppSnapshot {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Hand `cmd` to the player without waiting: a player whose queue is
    /// full (a recording finishing, a stuck audio device) says so instead
    /// of holding up every agent
    fn send(&self, cmd: AppCommand) -> Result<(), String> {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(PLAYER_BUSY.into()),
            Err(TrySendError::Disconnected(_)) => Err("The player has stopped".into()),
        }
    }

    /// The agents using the player, shared by every session
    pub fn presence(&self) -> Presence {
        self.presence.clone()
    }

    /// A copy for one local agent's session
    pub fn for_local(&self, place: Place) -> Self {
        Self {
            place: Some(place),
            ..self.clone()
        }
    }

    /// Put the agent's name on the agents chip's list
    pub(super) fn note_agent(&self, name: Option<String>, extensions: &rmcp::model::Extensions) {
        let place = self.place.clone().or_else(|| {
            extensions
                .get::<http::request::Parts>()
                .and_then(|parts| parts.extensions.get::<NetworkAgent>())
                .map(|agent| Place::Network(agent.0.clone()))
        });
        if let (Some(place), Some(name)) = (place, name) {
            self.presence.set_name(&place, &name);
        }
    }

    /// Remember which agent changed the player, for get_status
    fn note_change(&self, ctx: &RequestContext<RoleServer>, action: impl Into<String>) {
        // 2026-07-28 clients name themselves on every request; older ones
        // once, in initialize
        let name = ctx
            .meta
            .client_info()
            .map(|info| info.name)
            .or_else(|| ctx.peer.peer_info().map(|p| p.client_info.name.clone()))
            .filter(|name| !name.is_empty());
        self.note_agent(name.clone(), &ctx.extensions);
        let by = name.unwrap_or_else(|| "an agent".into());
        *self.last_change.lock().unwrap_or_else(|e| e.into_inner()) = Some(LastChange {
            by,
            action: action.into(),
            at: Instant::now(),
        });
    }

    /// Start a station and wait up to `wait` for it to play or fail
    async fn play(
        &self,
        ctx: &RequestContext<RoleServer>,
        url: String,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
        wait: Duration,
    ) -> Result<String, String> {
        let label = name.clone().unwrap_or_else(|| url.clone());
        // The player says which station number ours became, so a station
        // started by the window or another agent just before or after
        // isn't taken for ours
        let (taken, ours) = tokio::sync::oneshot::channel();
        // The logo and country go with it, so the header shows them as it
        // does for a station picked in the UI
        self.send(AppCommand::Play {
            url: url.clone(),
            name,
            logo_url,
            country,
            taken: Some(taken),
        })?;
        self.note_change(ctx, format!("play {label}"));
        if wait.is_zero() {
            return Ok(format!(
                "Starting {label}; call get_status to see whether it plays"
            ));
        }

        let deadline = tokio::time::Instant::now() + wait;
        let still_connecting = || {
            Ok(format!(
                "{label} is still connecting; call get_status to follow it"
            ))
        };
        let seq = match tokio::time::timeout_at(deadline, ours).await {
            Ok(Ok(seq)) => seq,
            // The player went away without taking it
            Ok(Err(_)) => return Err(format!("{label} was not started: the player has stopped")),
            Err(_) => return still_connecting(),
        };
        loop {
            let s = self.snapshot();
            if s.play_seq != seq {
                return Ok(format!(
                    "Started {label}, but another station has been started since"
                ));
            }
            if !s.is_resolving {
                if s.playback == PlaybackState::Playing {
                    return Ok(playing_text(&label, &s));
                }
                if let Some(e) = &s.last_error {
                    return Err(format!("{label} did not start: {e}"));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return still_connecting();
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

/// "Playing Jazz FM (MP3, 128 kbps). Now: Artist - Title"
fn playing_text(label: &str, s: &AppSnapshot) -> String {
    let mut text = format!("Playing {label}");
    let format = match (s.codec_name.as_str(), s.bitrate) {
        ("", _) => None,
        (codec, Some(kbps)) => Some(format!("{codec}, {kbps} kbps")),
        (codec, None) => Some(codec.to_string()),
    };
    if let Some(format) = format {
        text.push_str(&format!(" ({format})"));
    }
    match (s.artist.as_str(), s.title.as_str()) {
        (_, "") => {}
        ("", title) => text.push_str(&format!(". Now: {title}")),
        (artist, title) => text.push_str(&format!(". Now: {artist} - {title}")),
    }
    text
}
