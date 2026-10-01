//! The agent settings, applied while the player runs
//!
//! Local agents (`radiotrope --mcp`) are always welcome; network agents are
//! off until turned on. Both share one set of tools, so `get_status` shows
//! the last change whichever way the agent came in.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use radiotrope_app::data::agent_token;
use radiotrope_app::data::settings::{McpAuth, Settings};

use super::network;
use super::tools::RadioTools;

pub struct Agents {
    tools: RadioTools,
    network: Mutex<Option<network::Server>>,
    /// What the network server was last set up with
    applied: Mutex<Option<Applied>>,
    /// Counts the changes asked for, so a change set up late never undoes
    /// a newer one
    asked: AtomicU64,
    /// The last change asked for that has been set up
    done: AtomicU64,
}

/// What to do with the status once the network server is set up
pub type ShowStatus = Box<dyn FnOnce(NetworkStatus) + Send>;

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
            asked: AtomicU64::new(0),
            done: AtomicU64::new(0),
        }
    }

    pub fn tools(&self) -> RadioTools {
        self.tools.clone()
    }

    /// Start, restart or stop the network server to match the settings
    /// (the window uses [`apply_network_later`](Self::apply_network_later))
    #[cfg(test)]
    pub fn apply_network(&self, settings: &Settings) -> NetworkStatus {
        let mut running = self.network.lock().unwrap_or_else(|e| e.into_inner());
        self.apply_to(&mut running, settings)
    }

    /// Start, restart or stop the network server to match the settings, on
    /// a thread of its own, then `show` the status. Stopping the old server
    /// waits for it to let go of its port, which the window must not. A
    /// change asked for after this one wins: this one then does nothing.
    pub fn apply_network_later(self: &Arc<Self>, settings: &Settings, show: ShowStatus) {
        let ours = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let agents = self.clone();
        let settings = settings.clone();
        let spawned = std::thread::Builder::new()
            .name("mcp-network-setup".into())
            .spawn(move || {
                // One at a time; the newest change only
                let mut running = agents.network.lock().unwrap_or_else(|e| e.into_inner());
                if agents.asked.load(Ordering::SeqCst) != ours {
                    return;
                }
                let status = agents.apply_to(&mut running, &settings);
                agents.done.store(ours, Ordering::SeqCst);
                drop(running);
                show(status);
            });
        if let Err(e) = spawned {
            eprintln!("MCP network: can't set up the server: {e}");
            self.done.store(ours, Ordering::SeqCst);
        }
    }

    fn apply_to(
        &self,
        running: &mut Option<network::Server>,
        settings: &Settings,
    ) -> NetworkStatus {
        let address = listen_address(settings);
        let status = self.start_network(running, settings, &address);
        *self.applied.lock().unwrap_or_else(|e| e.into_inner()) = Some(Applied {
            settings: settings.clone(),
            address,
            failed: status.is_error,
        });
        status
    }

    /// Whether the network server needs setting up again: the picked
    /// interface's address has changed, or it could not listen last time.
    /// The settings to set it up with, then.
    pub fn network_to_recheck(&self) -> Option<Settings> {
        // A change still being set up comes first; what was applied before
        // it is old news
        if self.asked.load(Ordering::SeqCst) != self.done.load(Ordering::SeqCst) {
            return None;
        }
        let applied = self.applied.lock().unwrap_or_else(|e| e.into_inner());
        let applied = applied.as_ref()?;
        applied
            .is_stale(&listen_address(&applied.settings))
            .then(|| applied.settings.clone())
    }

    /// Set the network server up again when it needs it (see
    /// [`network_to_recheck`](Self::network_to_recheck)). `None` when
    /// nothing needed doing.
    #[cfg(test)]
    pub fn recheck_network(&self) -> Option<NetworkStatus> {
        let settings = self.network_to_recheck()?;
        Some(self.apply_network(&settings))
    }

    fn start_network(
        &self,
        running: &mut Option<network::Server>,
        settings: &Settings,
        address: &str,
    ) -> NetworkStatus {
        // The old server lets go of its port before a new one binds it.
        // Its sessions end with it, and so the agents it served are gone.
        if running.take().is_some() {
            self.tools.presence().network_stopped();
        }

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
    fn a_change_set_up_late_never_undoes_a_newer_one() {
        let agents = Arc::new(agents());
        let (tx, rx) = std::sync::mpsc::channel();
        // Hold the server's lock, as a slow stop would, while two changes
        // are asked for
        let held = agents.network.lock().unwrap();
        for port in [0, 1] {
            let settings = Settings {
                mcp_network: false,
                mcp_address: format!("127.0.0.1:{}", 9000 + port),
                ..Settings::default()
            };
            let tx = tx.clone();
            agents.apply_network_later(&settings, Box::new(move |s| tx.send(s.url).unwrap()));
        }
        // Nothing to recheck while a change waits
        assert!(agents.network_to_recheck().is_none());
        drop(held);
        let shown = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(shown, "http://127.0.0.1:9001/mcp");
        // The older one did nothing and showed nothing
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_err());
        let applied = agents.applied.lock().unwrap();
        assert_eq!(applied.as_ref().unwrap().address, "127.0.0.1:9001");
    }

    #[test]
    fn stopping_the_server_takes_its_agents_off_the_chip() {
        let agents = agents();
        let presence = agents.tools().presence();
        let settings = Settings {
            mcp_network: true,
            mcp_address: "127.0.0.1:0".into(),
            ..Settings::default()
        };
        assert!(!agents.apply_network(&settings).is_error);
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        presence.network_seen(super::super::presence::NetworkId::Address(ip), ip);
        assert_eq!(presence.agents().len(), 1);
        // A restart, e.g. for a new token
        assert!(!agents.apply_network(&settings).is_error);
        assert!(presence.agents().is_empty());
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
