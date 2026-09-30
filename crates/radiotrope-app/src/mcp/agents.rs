//! The agent settings, applied while the player runs
//!
//! Local agents (`radiotrope --mcp`) can be turned off; network agents are
//! off until turned on. Both share one set of tools, so `get_status` shows
//! the last change whichever way the agent came in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use radiotrope_app::data::agent_token;
use radiotrope_app::data::settings::Settings;

use super::network;
use super::tools::RadioTools;

pub struct Agents {
    tools: RadioTools,
    local: Arc<AtomicBool>,
    network: Mutex<Option<network::Server>>,
}

/// What the settings show about network agents
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetworkStatus {
    /// "Listening on ...", "Off", or what went wrong
    pub text: String,
    pub is_error: bool,
    /// Empty until network agents are first turned on
    pub token: String,
    /// A `claude mcp add` line for Claude Code
    pub command: String,
}

impl Agents {
    pub fn new(tools: RadioTools, settings: &Settings) -> Self {
        Self {
            tools,
            local: Arc::new(AtomicBool::new(settings.mcp_local)),
            network: Mutex::new(None),
        }
    }

    pub fn tools(&self) -> RadioTools {
        self.tools.clone()
    }

    /// Read by the local server as each agent connects
    pub fn local_allowed(&self) -> Arc<AtomicBool> {
        self.local.clone()
    }

    pub fn set_local(&self, on: bool) {
        self.local.store(on, Ordering::Relaxed);
    }

    /// Start, restart or stop the network server to match the settings
    pub fn apply_network(&self, settings: &Settings) -> NetworkStatus {
        let mut running = self.network.lock().unwrap_or_else(|e| e.into_inner());
        // The old server lets go of its port before a new one binds it
        running.take();

        let tls = match (&settings.mcp_tls_cert, &settings.mcp_tls_key) {
            (Some(cert), Some(key)) => Ok(Some((cert.clone(), key.clone()))),
            (None, None) => Ok(None),
            _ => Err("Choose both a certificate and a key, or neither".to_string()),
        };
        let with_tls = matches!(tls, Ok(Some(_)));

        if !settings.mcp_network {
            let token = agent_token::load_existing().unwrap_or_default();
            let command = network::url_for(&settings.mcp_address, with_tls)
                .map(|url| claude_command(&url, &token))
                .unwrap_or_default();
            return NetworkStatus {
                text: "Off".into(),
                is_error: false,
                token,
                command,
            };
        }

        let tls = match tls {
            Ok(tls) => tls,
            Err(text) => {
                let token = agent_token::load_existing().unwrap_or_default();
                return NetworkStatus {
                    text,
                    is_error: true,
                    command: network::url_for(&settings.mcp_address, false)
                        .map(|url| claude_command(&url, &token))
                        .unwrap_or_default(),
                    token,
                };
            }
        };
        let token = match agent_token::load_or_create() {
            Ok(t) => t,
            Err(e) => {
                return NetworkStatus {
                    text: format!("Can't make a token: {e}"),
                    is_error: true,
                    ..Default::default()
                }
            }
        };
        let command = network::url_for(&settings.mcp_address, with_tls)
            .map(|url| claude_command(&url, &token))
            .unwrap_or_default();
        let started = network::start(
            &network::Options {
                address: settings.mcp_address.clone(),
                token: token.clone(),
                tls,
            },
            self.tools.clone(),
        );
        match started {
            Ok(server) => {
                let text = format!("Listening on {}", server.url());
                *running = Some(server);
                NetworkStatus {
                    text,
                    is_error: false,
                    token,
                    command,
                }
            }
            Err(e) => NetworkStatus {
                text: e,
                is_error: true,
                token,
                command,
            },
        }
    }
}

fn claude_command(url: &str, token: &str) -> String {
    let token = if token.is_empty() { "<token>" } else { token };
    format!("claude mcp add --transport http radiotrope {url} --header \"Authorization: Bearer {token}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents() -> Agents {
        let (tx, _rx) = crossbeam_channel::bounded(8);
        let tools = RadioTools::new(
            tx,
            Arc::new(Mutex::new(crate::app::state::AppSnapshot::default())),
            Arc::new(Mutex::new(
                radiotrope_app::data::favorites::FavoritesManager::new(),
            )),
        );
        Agents::new(tools, &Settings::default())
    }

    #[test]
    fn network_agents_are_off_by_default() {
        let agents = agents();
        let status = agents.apply_network(&Settings::default());
        assert_eq!(status.text, "Off");
        assert!(status
            .command
            .starts_with("claude mcp add --transport http radiotrope http://127.0.0.1:8765/mcp"));
        assert!(agents.local_allowed().load(Ordering::Relaxed));
    }

    #[test]
    fn half_a_tls_setup_is_refused() {
        let settings = Settings {
            mcp_network: true,
            mcp_address: "127.0.0.1:0".into(),
            mcp_tls_cert: Some("cert.pem".into()),
            ..Settings::default()
        };
        let status = agents().apply_network(&settings);
        assert!(status.is_error);
        assert!(status.text.contains("both"), "{}", status.text);
    }
}
