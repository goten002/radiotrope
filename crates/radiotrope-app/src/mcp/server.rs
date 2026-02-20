//! MCP stdio server
//!
//! Reads JSON-RPC requests from stdin, dispatches to handlers, writes
//! responses to stdout. Runs on a plain std::thread — no async runtime.

use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use serde_json::{json, Value};

use crate::app::state::{AppCommand, AppSnapshot};

use super::tools;
use super::types::{JsonRpcRequest, JsonRpcResponse, ToolCallParams, INVALID_PARAMS,
                   METHOD_NOT_FOUND, PARSE_ERROR};

const SERVER_NAME: &str = "radiotrope";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the MCP stdio server (blocking — call from a dedicated thread)
pub fn run(cmd_tx: Sender<AppCommand>, state: Arc<Mutex<AppSnapshot>>) {
    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // stdin closed
        };

        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::error(None, PARSE_ERROR, e.to_string());
                write_response(&stdout, &resp);
                continue;
            }
        };

        let response = handle_request(&request, &cmd_tx, &state);
        write_response(&stdout, &response);
    }
}

fn handle_request(
    req: &JsonRpcRequest,
    cmd_tx: &Sender<AppCommand>,
    state: &Arc<Mutex<AppSnapshot>>,
) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => handle_initialize(req.id.clone()),
        "initialized" => {
            // Notification — no response needed, but we return one if there's an id
            if req.id.is_some() {
                JsonRpcResponse::success(req.id.clone(), json!({}))
            } else {
                // Notifications don't get responses
                JsonRpcResponse::success(None, json!({}))
            }
        }
        "tools/list" => handle_tools_list(req.id.clone()),
        "tools/call" => handle_tools_call(req.id.clone(), &req.params, cmd_tx, state),
        "ping" => JsonRpcResponse::success(req.id.clone(), json!({})),
        _ => JsonRpcResponse::error(
            req.id.clone(),
            METHOD_NOT_FOUND,
            format!("Unknown method: {}", req.method),
        ),
    }
}

fn handle_initialize(id: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION
            }
        }),
    )
}

fn handle_tools_list(id: Option<Value>) -> JsonRpcResponse {
    let tool_defs = tools::list_tools();
    JsonRpcResponse::success(
        id,
        json!({ "tools": tool_defs }),
    )
}

fn handle_tools_call(
    id: Option<Value>,
    params: &Value,
    cmd_tx: &Sender<AppCommand>,
    state: &Arc<Mutex<AppSnapshot>>,
) -> JsonRpcResponse {
    let call_params: ToolCallParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::error(id, INVALID_PARAMS, e.to_string());
        }
    };

    let result = tools::call_tool(&call_params.name, &call_params.arguments, cmd_tx, state);

    JsonRpcResponse::success(
        id,
        serde_json::to_value(result).unwrap_or_else(|_| json!({"error": "serialization failed"})),
    )
}

fn write_response(stdout: &io::Stdout, response: &JsonRpcResponse) {
    // Don't write responses for notifications (no id)
    if response.id.is_none() && response.error.is_none() {
        return;
    }

    let mut out = stdout.lock();
    if let Err(e) = serde_json::to_writer(&mut out, response) {
        eprintln!("Failed to write MCP response: {e}");
        return;
    }
    let _ = writeln!(out);
    let _ = out.flush();
}
