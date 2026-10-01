//! The agent settings, applied while the player runs
//!
//! Local agents (`radiotrope --mcp`) are always welcome; network agents are
//! off until turned on. Both share one set of tools, so `get_status` shows
//! the last change whichever way the agent came in.

use std::sync::Mutex;

use radiotrope_app::data::agent_token;
use radiotrope_app::data::settings::{McpAuth, Settings};

use super::network;
use super::tools::RadioTools;

pub struct Agents {
    tools: RadioTools,
    network: Mutex<Option<network::Server>>,
    /// What the network server was last set up with
    applied: Mutex<Option<Applied>>,
}

/// The settings the network server was last set up with, and how that went
struct Applied {
    settings: Settings,
    /// The "host:port" it was to listen on
    address: String,
    /// It could not
    failed: bool,
}

impl Applied {
    /// Set up again: the picked interface has another address now, or
    /// listening failed last time (the network may be up now)
    fn is_stale(&self, address_now: &str) -> bool {
        self.settings.mcp_network && (self.failed || self.address != address_now)
    }
}

/// What the settings show about network agents
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetworkStatus {
    /// "Listening on ...", "Off", or what went wrong
    pub text: String,
    pub is_error: bool,
    /// Empty until network agents first need a token
    pub token: String,
    /// The URL network agents connect to; empty for an address that
    /// doesn't work
    pub url: String,
}

impl Agents {
    pub fn new(tools: RadioTools) -> Self {
        Self {
            tools,
            network: Mutex::new(None),
            applied: Mutex::new(None),
        }
    }

    pub fn tools(&self) -> RadioTools {
        self.tools.clone()
    }

    /// Start, restart or stop the network server to match the settings
    pub fn apply_network(&self, settings: &Settings) -> NetworkStatus {
        let address = listen_address(settings);
        let status = self.start_network(settings, &address);
        *self.applied.lock().unwrap_or_else(|e| e.into_inner()) = Some(Applied {
            settings: settings.clone(),
            address,
            failed: status.is_error,
        });
        status
    }

    /// Set the network server up again when the picked interface's address
    /// has changed, or when it could not listen last time. `None` when
    /// nothing needed doing.
    pub fn recheck_network(&self) -> Option<NetworkStatus> {
        let settings = {
            let applied = self.applied.lock().unwrap_or_else(|e| e.into_inner());
            let applied = applied.as_ref()?;
            if !applied.is_stale(&listen_address(&applied.settings)) {
                return None;
            }
            applied.settings.clone()
        };
        Some(self.apply_network(&settings))
    }

    fn start_network(&self, settings: &Settings, address: &str) -> NetworkStatus {
        let mut running = self.network.lock().unwrap_or_else(|e| e.into_inner());
        // The old server lets go of its port before a new one binds it
        running.take();

        if !settings.mcp_network {
            let token = agent_token::load_existing().unwrap_or_default();
            return NetworkStatus {
                text: "Off".into(),
                is_error: false,
                token,
                url: network::url_for(address).unwrap_or_default(),
            };
        }

        // A token is made the first time one is needed; with none needed,
        // the old one still shows (greyed) for switching back
        let token = match settings.mcp_auth {
            McpAuth::None => agent_token::load_existing().unwrap_or_default(),
            McpAuth::Token => match agent_token::load_or_create() {
                Ok(t) => t,
                Err(e) => {
                    return NetworkStatus {
                        text: format!("Can't make a token: {e}"),
                        is_error: true,
                        ..Default::default()
                    }
                }
            },
        };
        let url = network::url_for(address).unwrap_or_default();
        let started = network::start(
            &network::Options {
                address: address.to_string(),
                token: (settings.mcp_auth == McpAuth::Token).then(|| token.clone()),
            },
            self.tools.clone(),
        );
        match started {
            Ok(server) => {
                let text = if server.on_all_networks() {
                    "Listening on all networks".to_string()
                } else {
                    format!("Listening on {}", server.url())
                };
                *running = Some(server);
                NetworkStatus {
                    text,
                    is_error: false,
                    token,
                    url,
                }
            }
            Err(e) => NetworkStatus {
                text: e,
                is_error: true,
                token,
                url,
            },
        }
    }
}

/// The "host:port" to listen on. A picked interface listens on the address
/// it has now, which may differ from the saved one after a new lease.
pub fn listen_address(settings: &Settings) -> String {
    let saved = &settings.mcp_address;
    let (Some(name), Some(addr)) = (&settings.mcp_interface, network::split_address(saved)) else {
        return saved.clone();
    };
    network::interfaces()
        .into_iter()
        .find(|i| &i.name == name)
        .map(|i| std::net::SocketAddr::new(i.ip, addr.port()).to_string())
        .unwrap_or_else(|| saved.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn agents() -> Agents {
        let (tx, _rx) = crossbeam_channel::bounded(8);
        let tools = RadioTools::new(
            tx,
            Arc::new(Mutex::new(crate::app::state::AppSnapshot::default())),
            Arc::new(Mutex::new(
                radiotrope_app::data::favorites::FavoritesManager::new(),
            )),
        );
        Agents::new(tools)
    }

    #[test]
    fn network_agents_are_off_by_default() {
        let agents = agents();
        let status = agents.apply_network(&Settings::default());
        assert_eq!(status.text, "Off");
        assert_eq!(status.url, "http://127.0.0.1:8765/mcp");
    }

    #[test]
    fn the_server_is_set_up_again_when_its_address_changes_or_failed() {
        let applied = |network: bool, address: &str, failed: bool| Applied {
            settings: Settings {
                mcp_network: network,
                ..Settings::default()
            },
            address: address.into(),
            failed,
        };
        let now = "192.168.1.20:8765";
        assert!(!applied(true, now, false).is_stale(now));
        // A new lease
        assert!(applied(true, "192.168.1.9:8765", false).is_stale(now));
        // Listening failed, e.g. before the network was up
        assert!(applied(true, now, true).is_stale(now));
        // Off stays off
        assert!(!applied(false, "192.168.1.9:8765", true).is_stale(now));
    }

    #[test]
    fn a_failed_start_is_tried_again() {
        let agents = agents();
        assert!(agents.recheck_network().is_none());
        let mut settings = Settings {
            mcp_network: true,
            mcp_address: "127.0.0.1:0".into(),
            ..Settings::default()
        };
        assert!(!agents.apply_network(&settings).is_error);
        assert!(agents.recheck_network().is_none());

        // An address this computer doesn't have (TEST-NET-1)
        settings.mcp_address = "192.0.2.1:0".into();
        assert!(agents.apply_network(&settings).is_error);
        let again = agents.recheck_network().expect("tried again");
        assert!(again.is_error);
    }

    #[test]
    fn a_picked_interface_listens_on_its_address_today() {
        let mut settings = Settings {
            mcp_address: "10.9.9.9:9000".into(),
            ..Settings::default()
        };
        assert_eq!(listen_address(&settings), "10.9.9.9:9000");

        // Gone interfaces keep the saved address
        settings.mcp_interface = Some("no-such-interface".into());
        assert_eq!(listen_address(&settings), "10.9.9.9:9000");

        if let Some(found) = network::interfaces().into_iter().next() {
            settings.mcp_interface = Some(found.name.clone());
            assert_eq!(
                listen_address(&settings),
                std::net::SocketAddr::new(found.ip, 9000).to_string()
            );
        }
    }
}
