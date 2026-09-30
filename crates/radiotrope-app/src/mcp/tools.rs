//! MCP tools
//!
//! Each tool is a method on [`RadioTools`]; the `#[tool]` attributes give
//! the name, title, description and behaviour hints clients show and use.
//! Tools that return data return it as structured JSON (with an output
//! schema) and as the same JSON in text, for clients that only read text.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{schemars, tool, tool_router};
use serde::{Deserialize, Deserializer, Serialize};

use radiotrope::audio::PlaybackState;
use radiotrope_app::config::ui::SEARCH_PAGE_SIZE;
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::types::{url_to_id, Favorite, FavoriteSort, Station};
use radiotrope_app::providers::ProviderRegistry;

use crate::app::state::{AppCommand, AppSnapshot};

/// Default number of results returned by search_stations
const DEFAULT_SEARCH_LIMIT: usize = 20;

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
    pub(super) tool_router: ToolRouter<Self>,
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FavoriteIdArgs {
    /// Favorite id, from list_favorites
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetVolumeArgs {
    /// Volume from 0 (silent) to 100 (full)
    #[serde(deserialize_with = "lenient_number")]
    #[schemars(schema_with = "volume_schema")]
    pub volume: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Words to match against station names
    pub query: String,
    /// Maximum number of stations to return (1-100, default 20)
    #[serde(default, deserialize_with = "lenient_optional_number")]
    #[schemars(schema_with = "limit_schema")]
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
    pub query: String,
    pub count: usize,
    pub stations: Vec<FoundStation>,
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

    fn send(&self, cmd: AppCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    #[tool(
        title = "Play a stream URL",
        description = "Play a radio station by its stream URL. Starts in the background: \
                       call get_status to see whether it plays.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn play_url(&self, Parameters(args): Parameters<PlayUrlArgs>) -> Result<String, String> {
        let url = args.url.trim();
        if url.is_empty() {
            return Err("url must not be empty".into());
        }
        // Enrich from favorites, as the GUI does
        let name = self
            .favorites()
            .resolve_play(PlayMetadata {
                url: url.to_string(),
                name: args.name.clone(),
                ..Default::default()
            })
            .name;
        self.send(AppCommand::Play {
            url: url.to_string(),
            name,
        });
        Ok(format!("Resolving stream: {url}"))
    }

    #[tool(
        title = "Play a favorite",
        description = "Play a favorite station by its id (ids come from list_favorites)",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn play_favorite(
        &self,
        Parameters(args): Parameters<FavoriteIdArgs>,
    ) -> Result<String, String> {
        let id = args.id.trim();
        let (url, name) = {
            let favorites = self.favorites();
            let fav = favorites
                .get(id)
                .ok_or_else(|| format!("No favorite with id {id}; list_favorites gives the ids"))?;
            (fav.url().to_string(), fav.name().to_string())
        };
        self.send(AppCommand::Play {
            url: url.clone(),
            name: Some(name.clone()),
        });
        Ok(format!("Playing favorite: {name} ({url})"))
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
    async fn stop(&self) -> String {
        self.send(AppCommand::Stop);
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
    async fn set_volume(&self, Parameters(args): Parameters<SetVolumeArgs>) -> String {
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
        if was_muted && volume > 0.0 {
            format!("Volume set to {percent}% (unmuted)")
        } else {
            format!("Volume set to {percent}%")
        }
    }

    #[tool(
        title = "Player status",
        description = "What is playing: playback state, station, song, volume, stream format, \
                       recording and the last error",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_status(&self) -> Json<Status> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
        })
    }

    #[tool(
        title = "Search stations",
        description = "Search the radio-browser.info directory for stations by name. \
                       Station names and tags come from that public directory.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn search_stations(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<Json<SearchResult>, String> {
        let query = args.query.trim().to_string();
        if query.is_empty() {
            return Err("query must not be empty".into());
        }
        let limit = args
            .limit
            .map(|v| (v as usize).clamp(1, SEARCH_PAGE_SIZE))
            .unwrap_or(DEFAULT_SEARCH_LIMIT);
        // Blocking HTTP (reqwest's blocking client, which must not even be
        // built inside the async runtime): off it, so other calls keep flowing
        let tools = self.clone();
        let q = query.clone();
        let stations = tokio::task::spawn_blocking(move || {
            tools
                .providers()?
                .search_all(&q, limit)
                .map_err(|e| format!("Search failed: {e}"))
        })
        .await
        .map_err(|e| format!("Search failed: {e}"))??;

        let favorites = self.favorites();
        let stations: Vec<FoundStation> = stations
            .into_iter()
            .map(|s: Station| FoundStation {
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
            query,
            count: stations.len(),
            stations,
        }))
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
        Ok(format!("Removed \"{}\" from favorites", removed.name()))
    }
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
}
