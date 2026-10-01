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
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::recordings;
use radiotrope_app::data::settings::Settings;
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
        let url = args.url.trim();
        if url.is_empty() {
            return Err("url must not be empty".into());
        }
        // Enrich from favorites, as the GUI does
        let known = self.favorites().resolve_play(PlayMetadata {
            url: url.to_string(),
            name: args.name.clone(),
            ..Default::default()
        });
        self.play(
            &ctx,
            url.to_string(),
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
        let id = args.id.trim();
        let (url, name, logo, country) = {
            let favorites = self.favorites();
            let fav = favorites
                .get(id)
                .ok_or_else(|| format!("No favorite with id {id}; list_favorites gives the ids"))?;
            (
                fav.url().to_string(),
                fav.name().to_string(),
                fav.station.logo_url.clone(),
                fav.station.country.clone(),
            )
        };
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
    async fn stop(&self, ctx: RequestContext<RoleServer>) -> String {
        self.send(AppCommand::Stop);
        self.note_change(&ctx, "stop");
        "Playback stopped".into()
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
    ) -> String {
        let volume = (args.volume as f32).clamp(0.0, 100.0) / 100.0;
        let was_muted = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let muted = s.is_muted;
            // Set it in the shared state now so the GUI shows it at once
            s.volume = volume;
            if muted && volume > 0.0 {
                s.is_muted = false;
            }
            muted
        };
        self.send(AppCommand::SetVolume(volume));
        let percent = (volume * 100.0).round() as u8;
        self.note_change(&ctx, format!("set volume {percent}"));
        if was_muted && volume > 0.0 {
            format!("Volume set to {percent}% (unmuted)")
        } else {
            format!("Volume set to {percent}%")
        }
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
    ) -> String {
        if args.muted {
            self.send(AppCommand::Mute);
            self.note_change(&ctx, "mute");
            "Muted".into()
        } else {
            self.send(AppCommand::Unmute);
            self.note_change(&ctx, "unmute");
            "Unmuted".into()
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
        let favorite_id = s
            .station_url
            .as_deref()
            .filter(|url| self.favorites().is_favorite(url))
            .map(url_to_id);
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
            match failed {
                Some(e) if stations.is_empty() => Err(format!("Search failed: {e}")),
                _ => Ok((stations, has_more)),
            }
        })
        .await
        .map_err(|e| format!("Search failed: {e}"))?;
        let (stations, has_more) = results?;

        let favorites = self.favorites();
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
    async fn list_favorites(&self) -> Json<FavoritesList> {
        let favorites = self.favorites();
        let favorites = favorites
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
            .collect();
        Json(FavoritesList { favorites })
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
        let url = args.url.trim();
        let name = args.name.trim();
        if url.is_empty() || name.is_empty() {
            return Err("url and name must not be empty".into());
        }
        let mut fav = Favorite::new(name, url);
        if let Some(logo) = args.logo_url.filter(|s| !s.is_empty()) {
            fav = fav.with_logo(logo);
        }
        if args.country.is_some() {
            fav = fav.with_metadata(args.country, None, HashSet::new());
        }
        let id = fav.id();
        let mut favorites = self.favorites();
        favorites.add(fav).map_err(|e| e.to_string())?;
        self.save_favorites(&mut favorites)
            .map_err(|e| format!("Failed to save: {e}"))?;
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
        let mut favorites = self.favorites();
        let removed = favorites.remove(&id).map_err(|e| e.to_string())?;
        self.save_favorites(&mut favorites)
            .map_err(|e| format!("Removed but failed to save: {e}"))?;
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
        let settings = Settings::load().unwrap_or_default();
        self.send(AppCommand::StartRecording {
            folder: recordings::folder(settings.recording_dir.as_deref()),
            format: settings.recording_format.into(),
            bitrate: settings.recording_bitrate,
            // As in the window: the switch only counts with the EQ on
            with_eq: settings.record_with_eq && s.eq_enabled,
            cover: None,
        });
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
        self.send(AppCommand::StopRecording);
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

    fn favorites(&self) -> std::sync::MutexGuard<'_, FavoritesManager> {
        self.favorites.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn snapshot(&self) -> AppSnapshot {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn send(&self, cmd: AppCommand) {
        let _ = self.cmd_tx.send(cmd);
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
        let before = self.snapshot().play_seq;
        // The logo and country go with it, so the header shows them as it
        // does for a station picked in the UI
        self.send(AppCommand::Play {
            url: url.clone(),
            name,
            logo_url,
            country,
        });
        self.note_change(ctx, format!("play {label}"));
        if wait.is_zero() {
            return Ok(format!(
                "Starting {label}; call get_status to see whether it plays"
            ));
        }

        let started = Instant::now();
        loop {
            tokio::time::sleep(POLL).await;
            let s = self.snapshot();
            // Until the player takes the command, it still shows the
            // station before
            if s.play_seq > before + 1 {
                return Ok(format!(
                    "Started {label}, but another station has been started since"
                ));
            }
            if s.play_seq == before + 1 && !s.is_resolving {
                if s.playback == PlaybackState::Playing {
                    return Ok(playing_text(&label, &s));
                }
                if let Some(e) = &s.last_error {
                    return Err(format!("{label} did not start: {e}"));
                }
            }
            if started.elapsed() >= wait {
                return Ok(format!(
                    "{label} is still connecting; call get_status to follow it"
                ));
            }
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
