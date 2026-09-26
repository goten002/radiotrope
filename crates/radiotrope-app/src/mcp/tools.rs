//! MCP tool definitions and handlers
//!
//! Each tool is a function that takes arguments + shared state, returns a ToolResult.

use std::sync::{Arc, Mutex};

use std::collections::HashSet;

use crossbeam_channel::Sender;
use serde_json::{json, Value};

use radiotrope_app::config::ui::SEARCH_PAGE_SIZE;
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::types::Favorite;
use radiotrope_app::providers::ProviderRegistry;

use crate::app::state::{AppCommand, AppSnapshot};

use super::types::{ToolDefinition, ToolResult};

/// Maximum number of results returned by the search tool.
/// Capped below SEARCH_PAGE_SIZE to keep MCP responses concise.
const MCP_SEARCH_LIMIT: usize = 20;

/// Extract a numeric value from a JSON argument, accepting both numbers and string
/// representations. MCP clients frequently send integers as strings (e.g. `"45"`
/// instead of `45`), so we must handle both forms.
fn arg_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

/// Return all tool definitions for tools/list
pub fn list_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "play_url",
            description: "Play a radio station by its stream URL",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Station stream URL (e.g. http://stream.example.com/radio)"
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional display name for the station"
                    }
                },
                "required": ["url"]
            }),
        },
        ToolDefinition {
            name: "play_favorite",
            description: "Play a favorite station by its ID (use list_favorites to get IDs)",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Favorite station ID (from list_favorites)"
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "stop",
            description: "Stop playback",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "set_volume",
            description: "Set playback volume (0-100)",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "volume": {
                        "type": "integer",
                        "description": "Volume level from 0 to 100",
                        "minimum": 0,
                        "maximum": 100
                    }
                },
                "required": ["volume"]
            }),
        },
        ToolDefinition {
            name: "get_status",
            description:
                "Get full application status including playback state, volume, and current station",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "search_stations",
            description: "Search for radio stations by name. Returns matching stations from the radio-browser.info directory.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search term to match against station names"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (1-100, default 20)",
                        "minimum": 1,
                        "maximum": 100
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "list_favorites",
            description: "List all saved favorite stations",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "add_favorite",
            description: "Add a station to favorites",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Station stream URL"
                    },
                    "name": {
                        "type": "string",
                        "description": "Station display name"
                    },
                    "country": {
                        "type": "string",
                        "description": "Optional country name"
                    },
                    "logo_url": {
                        "type": "string",
                        "description": "Optional station logo/favicon URL"
                    }
                },
                "required": ["url", "name"]
            }),
        },
        ToolDefinition {
            name: "remove_favorite",
            description: "Remove a station from favorites by URL",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Station stream URL to remove"
                    }
                },
                "required": ["url"]
            }),
        },
    ]
}

/// Dispatch a tool call to the appropriate handler
pub fn call_tool(
    name: &str,
    args: &Value,
    cmd_tx: &Sender<AppCommand>,
    state: &Arc<Mutex<AppSnapshot>>,
    favorites: &Arc<Mutex<FavoritesManager>>,
) -> ToolResult {
    match name {
        "play_url" => handle_play_url(args, cmd_tx, favorites),
        "play_favorite" => handle_play_favorite(args, cmd_tx, favorites),
        // Keep old name as alias for backwards compatibility
        "play_station" => handle_play_url(args, cmd_tx, favorites),
        "stop" => handle_stop(cmd_tx),
        "set_volume" => handle_set_volume(args, cmd_tx, state),
        "get_status" => handle_get_status(state),
        "search_stations" => handle_search(args),
        "list_favorites" => handle_list_favorites(favorites),
        "add_favorite" => handle_add_favorite(args, favorites),
        "remove_favorite" => handle_remove_favorite(args, favorites),
        _ => ToolResult::error(format!("Unknown tool: {name}")),
    }
}

fn handle_play_url(
    args: &Value,
    cmd_tx: &Sender<AppCommand>,
    favorites: &Arc<Mutex<FavoritesManager>>,
) -> ToolResult {
    // Accept both "url" and legacy "query" param
    let url = args
        .get("url")
        .or_else(|| args.get("query"))
        .and_then(|v| v.as_str());
    let url = match url {
        Some(u) if !u.trim().is_empty() => u.trim(),
        _ => return ToolResult::error("Missing required parameter: url"),
    };
    let name = args.get("name").and_then(|v| v.as_str()).map(String::from);
    // Enrich from favorites using the same logic as the UI path
    let name = favorites
        .lock()
        .ok()
        .map(|f| {
            f.resolve_play(PlayMetadata {
                url: url.to_string(),
                name: name.clone(),
                ..Default::default()
            })
            .name
        })
        .unwrap_or(name);
    cmd_tx
        .send(AppCommand::Play {
            url: url.to_string(),
            name,
        })
        .ok();
    ToolResult::text(format!("Resolving stream: {url}"))
}

fn handle_play_favorite(
    args: &Value,
    cmd_tx: &Sender<AppCommand>,
    favorites: &Arc<Mutex<FavoritesManager>>,
) -> ToolResult {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.trim().is_empty() => id.trim(),
        _ => return ToolResult::error("Missing required parameter: id"),
    };

    let f = favorites.lock().unwrap_or_else(|e| e.into_inner());
    let fav = match f.get(id) {
        Some(fav) => fav,
        None => return ToolResult::error(format!("No favorite found with ID: {id}")),
    };

    let url = fav.url().to_string();
    let name = fav.name().to_string();
    drop(f);

    cmd_tx
        .send(AppCommand::Play {
            url: url.clone(),
            name: Some(name.clone()),
        })
        .ok();
    ToolResult::text(format!("Playing favorite: {name} ({url})"))
}

fn handle_stop(cmd_tx: &Sender<AppCommand>) -> ToolResult {
    cmd_tx.send(AppCommand::Stop).ok();
    ToolResult::text("Playback stopped")
}

fn handle_set_volume(
    args: &Value,
    cmd_tx: &Sender<AppCommand>,
    state: &Arc<Mutex<AppSnapshot>>,
) -> ToolResult {
    let volume = match args.get("volume").and_then(arg_as_f64) {
        Some(v) => (v as f32).clamp(0.0, 100.0) / 100.0,
        None => {
            return ToolResult::error("Missing required parameter: volume (expected number 0-100)")
        }
    };
    let was_muted = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        let muted = s.is_muted;
        // Pre-set volume in shared state so the UI poll timer reflects it immediately
        s.volume = volume;
        if muted && volume > 0.0 {
            s.is_muted = false;
        }
        muted
    };
    cmd_tx.send(AppCommand::SetVolume(volume)).ok();
    let display = (volume * 100.0) as u8;
    if was_muted && volume > 0.0 {
        ToolResult::text(format!("Volume set to {display}% (auto-unmuted)"))
    } else {
        ToolResult::text(format!("Volume set to {display}%"))
    }
}

fn handle_get_status(state: &Arc<Mutex<AppSnapshot>>) -> ToolResult {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let mut status = format!(
        "Playback: {:?}\nStation: {}\nTrack: {}\nArtist: {}\nVolume: {}%",
        s.playback,
        s.station_name.as_deref().unwrap_or("—"),
        if s.title.is_empty() { "—" } else { &s.title },
        if s.artist.is_empty() {
            "—"
        } else {
            &s.artist
        },
        (s.volume * 100.0) as u8,
    );
    if s.is_resolving {
        status.push_str("\nResolving: true");
    }
    if let Some(ref err) = s.last_error {
        status.push_str(&format!("\nLast error: {err}"));
    }
    ToolResult::text(status)
}

fn handle_search(args: &Value) -> ToolResult {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) if !q.trim().is_empty() => q.trim(),
        Some(_) => return ToolResult::error("Parameter 'query' must not be empty"),
        None => return ToolResult::error("Missing required parameter: query"),
    };

    let limit = args
        .get("limit")
        .and_then(arg_as_f64)
        .map(|v| (v as usize).clamp(1, SEARCH_PAGE_SIZE))
        .unwrap_or(MCP_SEARCH_LIMIT);

    let registry = match ProviderRegistry::with_defaults() {
        Ok(r) => r,
        Err(e) => return ToolResult::error(format!("Failed to initialize provider: {e}")),
    };

    let stations = match registry.search_all(query, limit) {
        Ok(s) => s,
        Err(e) => return ToolResult::error(format!("Search failed: {e}")),
    };

    if stations.is_empty() {
        return ToolResult::text(format!("No stations found for \"{query}\""));
    }

    let results: Vec<Value> = stations
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "url": s.url,
                "country": s.country.as_deref().unwrap_or(""),
                "logo_url": s.logo_url.as_deref().unwrap_or(""),
            })
        })
        .collect();

    let response = json!({
        "query": query,
        "count": results.len(),
        "stations": results,
    });

    ToolResult::text(serde_json::to_string_pretty(&response).unwrap_or_default())
}

fn handle_list_favorites(favorites: &Arc<Mutex<FavoritesManager>>) -> ToolResult {
    let f = favorites.lock().unwrap_or_else(|e| e.into_inner());
    if f.is_empty() {
        return ToolResult::text("No favorites saved");
    }
    let sorted = f.sorted(radiotrope_app::data::types::FavoriteSort::Manual);
    let items: Vec<Value> = sorted
        .iter()
        .map(|fav| {
            json!({
                "id": fav.id(),
                "name": fav.name(),
                "url": fav.url(),
                "country": fav.station.country.as_deref().unwrap_or(""),
            })
        })
        .collect();
    ToolResult::text(serde_json::to_string_pretty(&items).unwrap_or_default())
}

fn handle_add_favorite(args: &Value, favorites: &Arc<Mutex<FavoritesManager>>) -> ToolResult {
    let url = match args.get("url").and_then(|v| v.as_str()) {
        Some(u) if !u.trim().is_empty() => u.trim(),
        _ => return ToolResult::error("Missing required parameter: url"),
    };
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim(),
        _ => return ToolResult::error("Missing required parameter: name"),
    };
    let country = args
        .get("country")
        .and_then(|v| v.as_str())
        .map(String::from);
    let logo_url = args
        .get("logo_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let mut fav = Favorite::new(name, url);
    if let Some(logo) = logo_url {
        fav = fav.with_logo(logo);
    }
    if country.is_some() {
        fav = fav.with_metadata(country, None, HashSet::new());
    }

    let mut f = favorites.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = f.add(fav) {
        return ToolResult::error(format!("{e}"));
    }
    if let Err(e) = f.save() {
        return ToolResult::error(format!("Failed to save: {e}"));
    }
    ToolResult::text(format!("Added \"{}\" to favorites", name))
}

fn handle_remove_favorite(args: &Value, favorites: &Arc<Mutex<FavoritesManager>>) -> ToolResult {
    let url = match args.get("url").and_then(|v| v.as_str()) {
        Some(u) if !u.trim().is_empty() => u.trim(),
        _ => return ToolResult::error("Missing required parameter: url"),
    };

    let mut f = favorites.lock().unwrap_or_else(|e| e.into_inner());
    match f.remove_by_url(url) {
        Ok(removed) => {
            if let Err(e) = f.save() {
                return ToolResult::error(format!("Removed but failed to save: {e}"));
            }
            ToolResult::text(format!("Removed \"{}\" from favorites", removed.name()))
        }
        Err(e) => ToolResult::error(format!("{e}")),
    }
}
