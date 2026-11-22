//! MCP tools
//!
//! Each tool is a method on [`RadioTools`]; the `#[tool]` attributes give
//! the name, title, description and behaviour hints clients show and use.
//! Tools that return data return it as structured JSON (with an output
//! schema) and as the same JSON in text, for clients that only read text.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::service::RequestContext;
use rmcp::{schemars, tool, tool_router, RoleServer};
use serde::{Deserialize, Deserializer, Serialize};

use radiotrope::audio::PlaybackState;
use radiotrope_app::config::ui::SEARCH_PAGE_SIZE;
use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::flags::code_for_country;
use radiotrope_app::data::recordings;
use radiotrope_app::data::schedule::{self, Action, Entry};
use radiotrope_app::data::types::{url_to_id, Favorite};
#[cfg(test)]
use radiotrope_app::providers::ProviderRegistry;
use radiotrope_app::providers::{Category, CategoryType, SearchOrder, StationFilter};

use super::presence::{NetworkAgent, Place, Presence};
use crate::app::state::{AppCommand, AppSnapshot};
use crate::control::{
    self, check_length, sorted_genres, Control, EntryRequest, Found, Played, RecordingStart,
    RecordingStop, MAX_FAVORITES, MAX_NAME_CHARS, MAX_URL_CHARS,
};

/// Default number of results returned by search_stations
const DEFAULT_SEARCH_LIMIT: usize = 20;
/// How long play tools wait for the station to start, unless told otherwise
const DEFAULT_PLAY_WAIT: Duration = Duration::from_secs(10);
/// Longest wait a play tool accepts
const MAX_PLAY_WAIT_SECS: f64 = 30.0;
/// Default and largest number of entries the list_genres, list_countries,
/// list_languages and list_categories tools return
const DEFAULT_CATEGORY_LIMIT: usize = 50;
const MAX_CATEGORY_LIMIT: usize = 500;
/// What a search that found nothing says, so an agent checks its filters
const NOTHING_FOUND: &str = "No station matches. Filter values must be ones the directory has:                              list_genres, list_countries, list_languages and list_categories                              list them.";
/// Where the user's consent to report plays is given
const REPORT_PLAYS_NOTE: &str = "Whether plays of directory stations are counted in the                                  directory. This is the user's consent: only the user can                                  change it, in the window (Stations > Report Plays). No tool                                  can.";

/// Everything the tools reach into: the player, through the control layer
/// the Remote API shares, and what agents alone keep
#[derive(Clone)]
pub struct RadioTools {
    control: Control,
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
    /// Words to match against station names (typo tolerant)
    #[serde(default)]
    pub query: Option<String>,
    /// Genre name ("Drum and Bass") or slug ("drum-and-bass"), from
    /// list_genres. Several separated by commas ("jazz, blues") find
    /// stations with any of them.
    #[serde(default)]
    pub genre: Option<String>,
    /// Country name ("Greece") or two-letter ISO code ("GR"), from
    /// list_countries
    #[serde(default)]
    pub country: Option<String>,
    /// Language name ("Greek") or ISO 639-3 code ("ell"), from
    /// list_languages
    #[serde(default)]
    pub language: Option<String>,
    /// Programme type: its name ("Jazz music") or code (1-31), from
    /// list_categories. Broader than a genre: news, sport, culture ...
    #[serde(default, deserialize_with = "lenient_optional_text")]
    #[schemars(schema_with = "category_schema")]
    pub category: Option<String>,
    /// Codec, e.g. "MP3", "AAC", "OGG"
    #[serde(default)]
    pub codec: Option<String>,
    /// Lowest bitrate in kbps
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "bitrate_schema")]
    pub min_bitrate: Option<f64>,
    /// Order of the results. Default: relevance when a query is given,
    /// else popular.
    #[serde(default, alias = "order")]
    pub sort: Option<SortArg>,
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
pub enum SortArg {
    /// Most played first
    #[serde(alias = "popularity")]
    Popular,
    /// A to Z
    Name,
    /// Best match for the query first (most played first without a query)
    Relevance,
}

impl From<SortArg> for SearchOrder {
    fn from(sort: SortArg) -> Self {
        match sort {
            SortArg::Popular => SearchOrder::MostPlayed,
            SortArg::Name => SearchOrder::Name,
            // The directory's own choice: the best match when there is a
            // query, the most played when there is none
            SortArg::Relevance => SearchOrder::Popular,
        }
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct FacetArgs {
    /// Only entries whose name contains this text, or whose code or slug
    /// is it
    #[serde(default)]
    pub filter: Option<String>,
    /// Maximum number to return, largest first (1-500, default 50)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "category_limit_schema")]
    pub limit: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StationIdArgs {
    /// Station id from search_stations ("st_..."); a Radio Browser UUID
    /// kept from before also works
    pub id: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ListFavoritesArgs {
    /// Only favorites whose name, country or genres contain this text
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Next,
    #[serde(alias = "prev")]
    Previous,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StepFavoriteArgs {
    /// next or previous
    pub direction: Direction,
    /// Seconds to wait for the station to start playing (0-30, default 10;
    /// 0 returns at once)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "wait_schema")]
    pub wait_seconds: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReorderFavoritesArgs {
    /// Favorite ids (from list_favorites) in the order wanted. Favorites
    /// left out keep their order, after these.
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EqualizerArgs {
    /// true to switch the equalizer on, false to switch it off; leave out
    /// to keep it as it is
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Preset name, e.g. "Rock" (get_settings lists them); leave out to
    /// keep the current sound
    #[serde(default)]
    pub preset: Option<String>,
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

fn category_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "description": "Programme type: its name (\"Jazz music\") or code (1-31), from list_categories. Broader than a genre: news, sport, culture ..."
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

/// Text, or a whole number given as one (a programme type's code: `24`)
fn lenient_optional_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum TextOrNumber {
        Text(String),
        Number(u64),
    }
    Ok(match Option::<TextOrNumber>::deserialize(d)? {
        Some(TextOrNumber::Text(s)) => Some(s),
        Some(TextOrNumber::Number(n)) => Some(n.to_string()),
        None => None,
    })
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

/// A station of the directory, as search_stations and get_station give it
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FoundStation {
    /// Id for play_station and get_station
    pub id: Option<String>,
    pub name: String,
    /// Stream URL to pass to play_url or add_favorite
    pub url: String,
    /// Country name, with the region when known ("Greece, Attica")
    pub country: Option<String>,
    /// Two-letter ISO code of the country, to search by it
    pub country_code: Option<String>,
    /// Language names ("Greek, English")
    pub language: Option<String>,
    /// Genre names; each can be passed as genre to search_stations
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
    /// Stations on this page
    pub count: usize,
    /// Stations the directory has for these filters, on all pages (not
    /// given with codec or min_bitrate, which are checked page by page)
    pub total: Option<usize>,
    /// More may follow: search again with offset + limit
    pub has_more: bool,
    pub stations: Vec<FoundStation>,
    /// Advice when nothing was found
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GenreItem {
    /// Pass the name or the slug as genre to search_stations
    pub name: String,
    pub slug: String,
    /// Stations with this genre
    pub stations: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GenreList {
    /// Largest first
    pub genres: Vec<GenreItem>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CountryItem {
    /// Pass the name or the code as country to search_stations
    pub name: String,
    /// Two-letter ISO 3166-1 code
    pub code: String,
    /// Stations in this country
    pub stations: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CountryList {
    /// Largest first
    pub countries: Vec<CountryItem>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LanguageItem {
    /// Pass the name or the code as language to search_stations
    pub name: String,
    /// ISO 639-3 code
    pub code: String,
    /// Stations in this language
    pub stations: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LanguageList {
    /// Largest first
    pub languages: Vec<LanguageItem>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProgrammeTypeItem {
    /// Pass the name or the code as category to search_stations
    pub name: String,
    /// RDS programme type code, 1-31
    pub code: String,
    /// Stations of this type
    pub stations: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProgrammeTypeList {
    /// Largest first
    pub categories: Vec<ProgrammeTypeItem>,
}

/// The settings agents can read
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Settings {
    pub equalizer: EqualizerInfo,
    /// What start_recording and scheduled recordings use; changed in the
    /// window (Tools > Recording Settings)
    pub recording: RecordingSettings,
    pub report_plays: ReportPlays,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EqualizerInfo {
    pub enabled: bool,
    /// The preset in use; none when the bands were set by hand
    pub preset: Option<String>,
    /// The preset names set_equalizer takes
    pub presets: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordingSettings {
    /// Folder the files go to
    pub folder: String,
    /// "mp3", "opus" or "wav"
    pub format: String,
    /// Bitrate for MP3 and Opus; none: the format's usual one
    pub bitrate_kbps: Option<u32>,
    /// Whether the equalizer's sound is recorded (only while it is on)
    pub with_equalizer: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReportPlays {
    pub allowed: bool,
    pub note: String,
}

/// A directory station in the tools' words
fn found_station(
    Found {
        station: s,
        favorite_id,
    }: Found,
) -> FoundStation {
    FoundStation {
        id: s.provider_id.clone().filter(|id| !id.is_empty()),
        favorite_id,
        genres: sorted_genres(&s.genres),
        // "Greece, Attica": the country comes first
        country_code: s
            .country
            .as_deref()
            .and_then(|c| c.split(',').next())
            .and_then(|c| code_for_country(c.trim()))
            .map(str::to_ascii_uppercase),
        name: s.name,
        url: s.url,
        country: s.country,
        language: s.language,
        codec: s.codec,
        bitrate_kbps: s.bitrate.filter(|b| *b > 0),
        homepage: s.homepage,
        logo_url: s.logo_url,
    }
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

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
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
            control: Control::new(cmd_tx, state, favorites),
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
        self.control = self.control.with_test_setup(providers, favorites_file);
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
        let known = self
            .control
            .resolve_play(url.clone(), args.name.clone())
            .await
            .map_err(agent_text)?;
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
                       plays or fails (wait_seconds, default 10). The play is counted in the \
                       directory only if the user allowed Report Plays.",
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
        let station = self.control.station_to_play(id).await.map_err(agent_text)?;
        self.play(
            &ctx,
            station.url,
            Some(station.name),
            station.logo_url,
            station.country,
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
            .control
            .favorite_to_play(id)
            .await
            .map_err(agent_text)?;
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
        self.control.stop().map_err(agent_text)?;
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
        let was_muted = self.control.set_volume(volume).map_err(agent_text)?;
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
        self.control.set_muted(args.muted).map_err(agent_text)?;
        if args.muted {
            self.note_change(&ctx, "mute");
            Ok("Muted".into())
        } else {
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
        let s = self.control.snapshot();
        let favorite_id = match s.station_url.clone() {
            Some(url) => self.control.favorite_id(url).await,
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
        description = "Search the Radiotrope station directory. Combine a name query (typo \
                       tolerant) with genre, country, language, category (programme type), \
                       codec and minimum bitrate; with nothing given it lists the most \
                       popular stations. Filters take names or codes: list_genres, \
                       list_countries, list_languages and list_categories give the valid \
                       ones, and one the directory doesn't have finds nothing. Results carry \
                       names, and the id for play_station and get_station. Page with limit \
                       and offset. Codec and bitrate are checked on each page, so a page can \
                       hold fewer stations than the limit while has_more is true.",
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
            category: text(args.category),
            order: args.sort.map(Into::into).unwrap_or_default(),
        };
        // The directory's count leaves out what is checked on each page
        let counted = filter.codec.is_none() && filter.min_bitrate.is_none();
        let page = self
            .control
            .search(filter, limit, offset)
            .await
            .map_err(agent_text)?;
        let total = page.total.filter(|_| counted);
        let stations: Vec<FoundStation> = page.stations.into_iter().map(found_station).collect();
        Ok(Json(SearchResult {
            count: stations.len(),
            total,
            has_more: page.has_more,
            note: (stations.is_empty() && !page.has_more && offset == 0)
                .then(|| NOTHING_FOUND.to_string()),
            stations,
        }))
    }

    #[tool(
        title = "Station details",
        description = "One station of the directory by its id (from search_stations): name, \
                       stream URL, country, language, genres, codec and bitrate. Looking a \
                       station up doesn't play it.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn get_station(
        &self,
        Parameters(args): Parameters<StationIdArgs>,
    ) -> Result<Json<FoundStation>, String> {
        let id = args.id.trim().to_string();
        if id.is_empty() {
            return Err("id must not be empty".into());
        }
        let station = self.control.station(id).await.map_err(agent_text)?;
        let favorite_id = self.control.favorite_id(station.url.clone()).await;
        Ok(Json(found_station(Found {
            station,
            favorite_id,
        })))
    }

    #[tool(
        title = "List genres",
        description = "The genres of the station directory with how many stations each has, \
                       largest first. Use a name or slug as genre in search_stations. There \
                       are many: narrow with filter, or raise limit.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_genres(
        &self,
        Parameters(args): Parameters<FacetArgs>,
    ) -> Result<Json<GenreList>, String> {
        let genres = self
            .facet(CategoryType::Genre, args)
            .await?
            .into_iter()
            .map(|c| GenreItem {
                name: c.name,
                slug: c.id,
                stations: c.station_count,
            })
            .collect();
        Ok(Json(GenreList { genres }))
    }

    #[tool(
        title = "List countries",
        description = "The countries of the station directory with how many stations each \
                       has, largest first. Use a name or code as country in search_stations.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_countries(
        &self,
        Parameters(args): Parameters<FacetArgs>,
    ) -> Result<Json<CountryList>, String> {
        let countries = self
            .facet(CategoryType::Country, args)
            .await?
            .into_iter()
            .map(|c| CountryItem {
                name: c.name,
                code: c.code.filter(|code| !code.is_empty()).unwrap_or(c.id),
                stations: c.station_count,
            })
            .collect();
        Ok(Json(CountryList { countries }))
    }

    #[tool(
        title = "List languages",
        description = "The languages of the station directory with how many stations each \
                       has, largest first. Use a name or code as language in search_stations.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_languages(
        &self,
        Parameters(args): Parameters<FacetArgs>,
    ) -> Result<Json<LanguageList>, String> {
        let languages = self
            .facet(CategoryType::Language, args)
            .await?
            .into_iter()
            .map(|c| LanguageItem {
                name: c.name,
                code: c.id,
                stations: c.station_count,
            })
            .collect();
        Ok(Json(LanguageList { languages }))
    }

    #[tool(
        title = "List programme types",
        description = "The programme types (broad kinds of station: news, sport, pop music, \
                       culture ...) of the station directory with how many stations each \
                       has, largest first. Use a name or code as category in \
                       search_stations. For music styles see list_genres.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_categories(
        &self,
        Parameters(args): Parameters<FacetArgs>,
    ) -> Result<Json<ProgrammeTypeList>, String> {
        let categories = self
            .facet(CategoryType::ProgrammeType, args)
            .await?
            .into_iter()
            .map(|c| ProgrammeTypeItem {
                name: c.name,
                code: c.id,
                stations: c.station_count,
            })
            .collect();
        Ok(Json(ProgrammeTypeList { categories }))
    }

    #[tool(
        title = "List favorites",
        description = "List the saved favorite stations, in the user's order, with their \
                       ids; query keeps those whose name, country or genres contain it",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_favorites(
        &self,
        Parameters(args): Parameters<ListFavoritesArgs>,
    ) -> Result<Json<FavoritesList>, String> {
        let query = args
            .query
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty());
        let wanted = |fav: &Favorite| {
            query.as_deref().is_none_or(|q| {
                let has = |text: &str| text.to_lowercase().contains(q);
                has(fav.name())
                    || fav.station.country.as_deref().is_some_and(has)
                    || fav.station.genres.iter().any(|g| has(g))
            })
        };
        let favorites = self
            .control
            .favorites_with_stats()
            .await
            .map_err(agent_text)?
            .into_iter()
            .filter(|(fav, _)| wanted(fav))
            .map(|(fav, stats)| FavoriteItem {
                id: fav.id(),
                name: fav.name().to_string(),
                url: fav.url().to_string(),
                country: fav.station.country.clone(),
                genres: sorted_genres(&fav.station.genres),
                play_count: stats.play_count,
            })
            .collect();
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
        self.control.add_favorite(fav).await.map_err(agent_text)?;
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
        let removed = self.control.remove_favorite(id).await.map_err(agent_text)?;
        self.note_change(&ctx, format!("remove favorite {}", removed.name()));
        Ok(format!("Removed \"{}\" from favorites", removed.name()))
    }

    #[tool(
        title = "Next or previous favorite",
        description = "Play the favorite after or before the one playing, in the user's \
                       order and going round at the ends (the window's Next and Previous); \
                       the first favorite when none is playing. Waits until it plays or \
                       fails (wait_seconds, default 10).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn step_favorite(
        &self,
        Parameters(args): Parameters<StepFavoriteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let forward = matches!(args.direction, Direction::Next);
        let (url, name, logo, country) = self
            .control
            .favorite_step(forward)
            .await
            .map_err(agent_text)?;
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
        title = "Reorder favorites",
        description = "Put favorites in a new order: the ids given come first, in that \
                       order; favorites left out keep their order after them",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn reorder_favorites(
        &self,
        Parameters(args): Parameters<ReorderFavoritesArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let ids: Vec<String> = args
            .ids
            .iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return Err("Give the favorite ids in the order wanted".into());
        }
        if ids.len() > MAX_FAVORITES {
            return Err(format!("At most {MAX_FAVORITES} ids"));
        }
        let moved = ids.len();
        self.control
            .reorder_favorites(ids)
            .await
            .map_err(agent_text)?;
        self.note_change(&ctx, "reorder favorites");
        Ok(match moved {
            1 => "Moved 1 favorite to the top".into(),
            n => format!("Put {n} favorites first, in the order given"),
        })
    }

    #[tool(
        title = "Equalizer",
        description = "Switch the equalizer on or off, and pick one of its presets by name \
                       (get_settings lists the presets and shows the equalizer now)",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_equalizer(
        &self,
        Parameters(args): Parameters<EqualizerArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        if args.enabled.is_none() && args.preset.is_none() {
            return Err("Give enabled, preset or both".into());
        }
        let preset = self
            .control
            .set_equalizer(args.enabled, args.preset.as_deref())
            .map_err(|e| match e {
                control::Error::Failed(text) if text.starts_with("No equalizer preset") => {
                    format!("{text}. The presets: {}", preset_names().join(", "))
                }
                other => agent_text(other),
            })?;
        let on = args
            .enabled
            .unwrap_or_else(|| self.control.snapshot().eq_enabled);
        let text = match (preset, args.enabled) {
            (Some(name), _) if on => format!("Equalizer on, preset {name}"),
            (Some(name), _) => format!(
                "Preset {name} chosen; the equalizer is off (set enabled to true to hear it)"
            ),
            (None, Some(true)) => "Equalizer on".to_string(),
            (None, _) => "Equalizer off".to_string(),
        };
        self.note_change(&ctx, text.to_lowercase());
        Ok(text)
    }

    #[tool(
        title = "Settings",
        description = "Read the player's settings an agent may need: the equalizer (on or \
                       off, preset, the preset names), the recording settings (folder, \
                       format, bitrate) and whether Report Plays is allowed. Read-only: \
                       set_equalizer changes the equalizer; the recording settings and \
                       Report Plays are changed by the user, in the window.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_settings(&self) -> Json<Settings> {
        let s = self.control.snapshot();
        let setup = &s.recording_setup;
        Json(Settings {
            equalizer: EqualizerInfo {
                enabled: s.eq_enabled,
                preset: s.eq_preset_name.clone().filter(|name| !name.is_empty()),
                presets: preset_names().into_iter().map(str::to_string).collect(),
            },
            recording: RecordingSettings {
                folder: recordings::folder(setup.dir.as_deref())
                    .display()
                    .to_string(),
                format: setup.format.extension().to_string(),
                bitrate_kbps: setup.bitrate,
                with_equalizer: setup.with_eq,
            },
            report_plays: ReportPlays {
                allowed: crate::view_options::report_plays(),
                note: REPORT_PLAYS_NOTE.to_string(),
            },
        })
    }

    #[tool(
        title = "Start recording",
        description = "Record the station playing to a file, in the format and folder set in \
                       the player's recording settings (get_settings shows them)",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn start_recording(&self, ctx: RequestContext<RoleServer>) -> Result<String, String> {
        let started = self
            .control
            .start_recording(|| self.note_change(&ctx, "start recording"))
            .await
            .map_err(agent_text)?;
        Ok(match started {
            RecordingStart::Already(path) => format!("Already recording to {}", path.display()),
            RecordingStart::Started(path) => format!("Recording to {}", path.display()),
            RecordingStart::Starting => "Recording is starting; get_status shows its file".into(),
        })
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
        let stopped = self
            .control
            .stop_recording(|| self.note_change(&ctx, "stop recording"))
            .await
            .map_err(agent_text)?;
        Ok(match stopped {
            RecordingStop::NotRecording => "Not recording".into(),
            RecordingStop::Saved(text) => text,
            RecordingStop::Stopping(path) => format!("Recording stopped: {}", path.display()),
        })
    }

    #[tool(
        title = "Sleep Timer",
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
            self.control
                .set_sleep_timer(None, true)
                .map_err(agent_text)?;
            self.note_change(&ctx, "sleep timer off");
            return Ok("Sleep timer off".into());
        }
        self.control
            .set_sleep_timer(Some(minutes), args.fade.unwrap_or(true))
            .map_err(agent_text)?;
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
        let s = self.control.snapshot();
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
        let entry = self
            .control
            .build_entry(EntryRequest {
                action: args.action.into(),
                start: args.start,
                days: args.days,
                date: args.date,
                favorite_id: args.favorite_id,
                url: args.url,
                name: args.name,
                end_after_minutes: args.end_after_minutes,
                end_at: args.end_at,
                volume: args.volume,
                fade: args.fade,
                fallback: args.fallback,
            })
            .await
            .map_err(agent_text)?;
        let start = entry.start;
        let title = entry.title();
        let id = self.control.save_entry(entry).await.map_err(agent_text)?;
        self.note_change(&ctx, format!("schedule {title} at {start}"));
        let now = chrono::Local::now();
        Ok(match self.control.entry(id) {
            Ok(e) => {
                let next = e
                    .next_start(&now)
                    .map(|at| format!(", next {}", schedule::when_label(&at, &now)))
                    .unwrap_or_default();
                format!(
                    "Scheduled {title} at {start} ({}){next}. Id {id}",
                    e.details()
                )
            }
            Err(_) => format!("Scheduled {title} at {start}. Id {id}"),
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
        let title = self.control.entry(id).map_err(agent_text)?.title();
        self.control.remove_entry(id).await.map_err(agent_text)?;
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
        let title = self.control.entry(id).map_err(agent_text)?.title();
        let enabled = args.enabled;
        self.control
            .set_entry_enabled(id, enabled)
            .await
            .map_err(agent_text)?;
        let word = if enabled { "on" } else { "off" };
        self.note_change(&ctx, format!("switch {word} {title}"));
        Ok(format!("Switched {word}: {title}"))
    }
}

/// A shared error in the tools' words: those naming an id say which tool
/// lists the ids
fn agent_text(error: control::Error) -> String {
    match error {
        control::Error::NoFavorite(id) => {
            format!("No favorite with id {id}; list_favorites gives the ids")
        }
        control::Error::NoStation(id) => {
            format!("No station with id {id}; search_stations gives the ids")
        }
        control::Error::NoEntry(id) => {
            format!("No schedule entry with id {id}; list_schedule gives the ids")
        }
        control::Error::TooManyFavorites => format!(
            "There are {MAX_FAVORITES} favorites already, the most an agent can add; \
             remove some first"
        ),
        other => other.to_string(),
    }
}

/// The equalizer's preset names, in the window's menu order
fn preset_names() -> Vec<&'static str> {
    radiotrope::audio::PRESETS.iter().map(|p| p.name).collect()
}

/// A whole, positive entry id
fn entry_id(id: f64) -> Result<u64, String> {
    if id >= 1.0 && id.fract() == 0.0 && id <= u64::MAX as f64 {
        Ok(id as u64)
    } else {
        Err("id must be an entry id from list_schedule".into())
    }
}

/// How long a play tool waits, from its wait_seconds argument
fn play_wait(seconds: Option<f64>) -> Duration {
    seconds
        .map(|s| Duration::from_secs_f64(s.clamp(0.0, MAX_PLAY_WAIT_SECS)))
        .unwrap_or(DEFAULT_PLAY_WAIT)
}

impl RadioTools {
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

    /// One of the directory's lists, narrowed and cut as the tool asked
    async fn facet(&self, kind: CategoryType, args: FacetArgs) -> Result<Vec<Category>, String> {
        let limit = args
            .limit
            .map(|v| (v as usize).clamp(1, MAX_CATEGORY_LIMIT))
            .unwrap_or(DEFAULT_CATEGORY_LIMIT);
        self.control
            .categories(kind, args.filter, limit)
            .await
            .map_err(agent_text)
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
        let played = self
            .control
            .play(url, name, logo_url, country, wait, || {
                self.note_change(ctx, format!("play {label}"))
            })
            .await;
        match played {
            Ok(Played::Starting) => Ok(format!(
                "Starting {label}; call get_status to see whether it plays"
            )),
            Ok(Played::StillConnecting) => Ok(format!(
                "{label} is still connecting; call get_status to follow it"
            )),
            Ok(Played::Superseded) => Ok(format!(
                "Started {label}, but another station has been started since"
            )),
            Ok(Played::Playing(s)) => Ok(playing_text(&label, &s)),
            Err(control::Error::NotTaken) => {
                Err(format!("{label} was not started: the player has stopped"))
            }
            Err(control::Error::Failed(e)) => Err(format!("{label} did not start: {e}")),
            Err(e) => Err(agent_text(e)),
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
