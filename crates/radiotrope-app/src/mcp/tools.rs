//! MCP tool definitions and handlers
//!
//! Each tool is a function that takes arguments + shared state, returns a ToolResult.

use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use serde_json::{json, Value};

use radiotrope_app::config::ui::SEARCH_PAGE_SIZE;
use radiotrope_app::providers::ProviderRegistry;

use crate::app::state::{AppCommand, AppSnapshot};

use super::types::{ToolDefinition, ToolResult};

/// Maximum number of results returned by the search tool.
/// Capped below SEARCH_PAGE_SIZE to keep MCP responses concise.
const MCP_SEARCH_LIMIT: usize = 20;

/// Return all tool definitions for tools/list
pub fn list_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "play_station",
            description: "Play a radio station by URL",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Station URL"
                    }
                },
                "required": ["query"]
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
    ]
}

/// Dispatch a tool call to the appropriate handler
pub fn call_tool(
    name: &str,
    args: &Value,
    cmd_tx: &Sender<AppCommand>,
    state: &Arc<Mutex<AppSnapshot>>,
) -> ToolResult {
    match name {
        "play_station" => handle_play(args, cmd_tx),
        "stop" => handle_stop(cmd_tx),
        "set_volume" => handle_set_volume(args, cmd_tx, state),
        "get_status" => handle_get_status(state),
        "search_stations" => handle_search(args),
        _ => ToolResult::error(format!("Unknown tool: {name}")),
    }
}

fn handle_play(args: &Value, cmd_tx: &Sender<AppCommand>) -> ToolResult {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return ToolResult::error("Missing required parameter: query"),
    };
    cmd_tx
        .send(AppCommand::Play {
            url: query.to_string(),
            name: None,
        })
        .ok();
    ToolResult::text(format!("Resolving stream: {query}"))
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
    let volume = match args.get("volume").and_then(|v| v.as_f64()) {
        Some(v) => (v as f32).clamp(0.0, 100.0) / 100.0,
        None => return ToolResult::error("Missing required parameter: volume"),
    };
    let was_muted = state.lock().unwrap_or_else(|e| e.into_inner()).is_muted;
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
        .and_then(|v| v.as_u64())
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
