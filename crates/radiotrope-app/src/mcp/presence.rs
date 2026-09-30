//! Which agents are using the player right now, for the menu bar's agents chip
//!
//! A local agent (`radiotrope --mcp`) counts while its connection is open.
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
    /// "claude-code, this computer" or "claude-code, 192.168.1.20"
    pub fn describe(&self) -> String {
        let name = self.name.as_deref().unwrap_or("Agent");
        match self.place {
            Place::Local(_) => format!("{name}, this computer"),
            Place::Network(ip) => format!("{name}, {ip}"),
        }
    }
}

#[derive(Default)]
struct Inner {
    next_local: u64,
    local: BTreeMap<u64, Option<String>>,
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

    /// A local agent connected
    pub fn local_connected(&self) -> LocalGuard {
        let mut inner = self.lock();
        let id = inner.next_local;
        inner.next_local += 1;
        inner.local.insert(id, None);
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
            Place::Local(id) => inner.local.get_mut(&id),
            Place::Network(ip) => inner.network.get_mut(&ip).map(|(name, _)| name),
        };
        if let Some(slot) = slot {
            *slot = Some(name.to_string());
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
        inner
            .local
            .iter()
            .map(|(id, name)| AgentInfo {
                name: name.clone(),
                place: Place::Local(*id),
            })
            .chain(network)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_agents_count_while_connected() {
        let presence = Presence::default();
        let first = presence.local_connected();
        let second = presence.local_connected();
        presence.set_name(first.place(), "claude-code");
        assert_eq!(
            presence
                .agents()
                .iter()
                .map(AgentInfo::describe)
                .collect::<Vec<_>>(),
            ["claude-code, this computer", "Agent, this computer"]
        );
        drop(first);
        drop(second);
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
    fn a_name_for_an_agent_that_left_is_ignored() {
        let presence = Presence::default();
        let place = presence.local_connected().place();
        presence.set_name(place, "late");
        presence.set_name(Place::Network("10.0.0.1".parse().unwrap()), "late");
        assert!(presence.agents().is_empty());
    }
}
