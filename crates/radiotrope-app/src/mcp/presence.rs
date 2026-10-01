//! Which agents are using the player right now, for the menu bar's agents chip
//!
//! A local agent (`radiotrope --mcp`) counts while its connection is open;
//! sessions started by one app (Claude Desktop opens one for its chat and
//! one for its agent mode) count as one agent.
//! Network agents have no lasting connection (each request stands alone),
//! so one counts while it keeps making requests: it drops off after
//! [`AGENT_IDLE`](radiotrope_app::config::mcp::AGENT_IDLE) without one.
//! Names come from the agents themselves, once they send them.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use radiotrope_app::config::mcp::AGENT_IDLE;

/// Where an agent reaches the player from
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Place {
    /// A `radiotrope --mcp` connection, by its number
    Local(u64),
    /// A computer on the network
    Network(IpAddr),
}

/// A network request's sender, handed to the tools with the request
#[derive(Debug, Clone, Copy)]
pub struct RemoteIp(pub IpAddr);

/// One agent using the player
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// The agent's own name, e.g. "claude-code"; `None` until it says
    pub name: Option<String>,
    pub place: Place,
}

impl AgentInfo {
    /// The agent's name, e.g. "claude-code"
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("Agent")
    }

    /// Where it calls from: "This computer", or the address of the
    /// computer on the network (one on this computer reads as such)
    pub fn place_label(&self) -> String {
        match self.place {
            Place::Network(ip) if !ip.is_loopback() => ip.to_string(),
            _ => "This computer".to_string(),
        }
    }

    pub fn is_network(&self) -> bool {
        matches!(self.place, Place::Network(_))
    }
}

/// The name to show for an agent's own name. Claude Desktop's agent mode
/// calls itself "local-agent-mode-<server>" beside its chat's "claude-ai";
/// it shows as the chat, so the app reads once.
fn app_name(name: &str) -> &str {
    if name.starts_with("local-agent-mode-") {
        "claude-ai"
    } else {
        name
    }
}

#[derive(Default)]
struct Inner {
    next_local: u64,
    /// Name, and the app that started the relay (its process id)
    local: BTreeMap<u64, (Option<String>, Option<u32>)>,
    network: HashMap<IpAddr, (Option<String>, Instant)>,
}

/// Shared by every session, local and network
#[derive(Clone, Default)]
pub struct Presence(Arc<Mutex<Inner>>);

/// Held while a local agent is connected; dropping it removes the agent
pub struct LocalGuard {
    presence: Presence,
    id: u64,
}

impl LocalGuard {
    pub fn place(&self) -> Place {
        Place::Local(self.id)
    }
}

impl Drop for LocalGuard {
    fn drop(&mut self) {
        self.presence.lock().local.remove(&self.id);
    }
}

impl Presence {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A local agent connected, from `app` (the relay's parent process)
    /// where known
    pub fn local_connected(&self, app: Option<u32>) -> LocalGuard {
        let mut inner = self.lock();
        let id = inner.next_local;
        inner.next_local += 1;
        inner.local.insert(id, (None, app));
        LocalGuard {
            presence: self.clone(),
            id,
        }
    }

    /// A network agent made a request
    pub fn network_seen(&self, ip: IpAddr) {
        self.network_seen_at(ip, Instant::now());
    }

    fn network_seen_at(&self, ip: IpAddr, at: Instant) {
        let mut inner = self.lock();
        let entry = inner.network.entry(ip).or_insert((None, at));
        entry.1 = at;
    }

    /// An agent said its name
    pub fn set_name(&self, place: Place, name: &str) {
        if name.is_empty() {
            return;
        }
        let mut inner = self.lock();
        let slot = match place {
            Place::Local(id) => inner.local.get_mut(&id).map(|(name, _)| name),
            Place::Network(ip) => inner.network.get_mut(&ip).map(|(name, _)| name),
        };
        if let Some(slot) = slot {
            *slot = Some(app_name(name).to_string());
        }
    }

    /// The agents using the player now: local ones first, then network
    /// ones by address
    pub fn agents(&self) -> Vec<AgentInfo> {
        self.agents_at(Instant::now())
    }

    fn agents_at(&self, now: Instant) -> Vec<AgentInfo> {
        let mut inner = self.lock();
        inner
            .network
            .retain(|_, (_, seen)| now.saturating_duration_since(*seen) < AGENT_IDLE);
        let mut network: Vec<AgentInfo> = inner
            .network
            .iter()
            .map(|(ip, (name, _))| AgentInfo {
                name: name.clone(),
                place: Place::Network(*ip),
            })
            .collect();
        network.sort_by_key(|a| match a.place {
            Place::Network(ip) => Some(ip),
            Place::Local(_) => None,
        });
        // One entry per app, in the order they connected, with the names of
        // all its sessions
        let mut local: Vec<(Option<u32>, AgentInfo)> = Vec::new();
        for (id, (name, app)) in &inner.local {
            let same_app = app.and_then(|app| {
                local
                    .iter_mut()
                    .find(|(other, _)| *other == Some(app))
                    .map(|(_, agent)| agent)
            });
            match same_app {
                Some(agent) => {
                    if let Some(name) = name {
                        match &mut agent.name {
                            Some(names) if names.split(" + ").all(|n| n != name) => {
                                names.push_str(" + ");
                                names.push_str(name);
                            }
                            Some(_) => {}
                            None => agent.name = Some(name.clone()),
                        }
                    }
                }
                None => local.push((
                    *app,
                    AgentInfo {
                        name: name.clone(),
                        place: Place::Local(*id),
                    },
                )),
            }
        }
        local
            .into_iter()
            .map(|(_, agent)| agent)
            .chain(network)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl AgentInfo {
        fn describe(&self) -> String {
            format!("{}, {}", self.name(), self.place_label())
        }
    }

    #[test]
    fn local_agents_count_while_connected() {
        let presence = Presence::default();
        let first = presence.local_connected(None);
        let second = presence.local_connected(None);
        presence.set_name(first.place(), "claude-code");
        assert_eq!(
            presence
                .agents()
                .iter()
                .map(AgentInfo::describe)
                .collect::<Vec<_>>(),
            ["claude-code, This computer", "Agent, This computer"]
        );
        drop(first);
        drop(second);
        assert!(presence.agents().is_empty());
    }

    #[test]
    fn the_sessions_of_one_app_count_once() {
        let presence = Presence::default();
        let chat = presence.local_connected(Some(100));
        let agent_mode = presence.local_connected(Some(100));
        let other = presence.local_connected(Some(200));
        presence.set_name(chat.place(), "claude-ai");
        presence.set_name(agent_mode.place(), "local-agent-mode-radiotrope");
        presence.set_name(other.place(), "claude-code");
        // Names we don't know pass as they are
        let codex = presence.local_connected(Some(300));
        presence.set_name(codex.place(), "codex-mcp-client");
        let describe = |p: &Presence| {
            p.agents()
                .iter()
                .map(AgentInfo::describe)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            describe(&presence),
            [
                "claude-ai, This computer",
                "claude-code, This computer",
                "codex-mcp-client, This computer"
            ]
        );
        // Still there while one of its sessions is
        drop(chat);
        assert_eq!(
            describe(&presence),
            [
                "claude-ai, This computer",
                "claude-code, This computer",
                "codex-mcp-client, This computer"
            ]
        );
        drop(agent_mode);
        drop(other);
        drop(codex);
        assert!(presence.agents().is_empty());
    }

    #[test]
    fn network_agents_drop_off_when_idle() {
        let presence = Presence::default();
        let ip: IpAddr = "192.168.1.20".parse().unwrap();
        let start = Instant::now();
        presence.network_seen_at(ip, start);
        presence.set_name(Place::Network(ip), "claude-code");
        presence.network_seen_at(ip, start);
        let agents = presence.agents_at(start + AGENT_IDLE / 2);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].describe(), "claude-code, 192.168.1.20");
        assert!(presence.agents_at(start + AGENT_IDLE).is_empty());
    }

    #[test]
    fn a_network_agent_on_this_computer_reads_as_such() {
        let presence = Presence::default();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        presence.network_seen(ip);
        presence.set_name(Place::Network(ip), "codex-mcp-client");
        let agents = presence.agents();
        assert_eq!(agents[0].describe(), "codex-mcp-client, This computer");
        assert!(agents[0].is_network());
    }

    #[test]
    fn a_name_for_an_agent_that_left_is_ignored() {
        let presence = Presence::default();
        let place = presence.local_connected(None).place();
        presence.set_name(place, "late");
        presence.set_name(Place::Network("10.0.0.1".parse().unwrap()), "late");
        assert!(presence.agents().is_empty());
    }
}
