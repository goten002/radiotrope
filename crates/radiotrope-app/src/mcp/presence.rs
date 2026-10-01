//! Which agents are using the player right now, for the menu bar's agents chip
//!
//! A local agent (`radiotrope --mcp`) counts while its connection is open;
//! sessions started by one app (Claude Desktop opens one for its chat and
//! one for its agent mode) count as one agent.
//! A network agent counts while it holds its event stream open (today's
//! clients open one as they start and it drops when they quit), and
//! otherwise until [`AGENT_IDLE`] after its last request. One whose stream
//! closed is gone unless it comes back within [`STREAM_GRACE`]; one that
//! ends its session leaves at once.
//! Names come from the agents themselves, once they send them.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use radiotrope_app::config::mcp::{AGENT_IDLE, STREAM_GRACE};

/// A network agent, as the server tells them apart
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NetworkId {
    /// An older client's session (its `Mcp-Session-Id`)
    Session(Arc<str>),
    /// A 2026-07-28 client has no session: known by the computer it calls
    /// from
    Address(IpAddr),
}

/// Where an agent reaches the player from
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Place {
    /// A `radiotrope --mcp` connection, by its number
    Local(u64),
    Network(NetworkId),
}

/// The network agent behind a request, handed to the tools with it
#[derive(Debug, Clone)]
pub struct NetworkAgent(pub NetworkId);

/// When an agent was last there
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// It holds a connection open: a local agent, or a network one with its
    /// event stream
    Connected,
    /// No open connection; its last request was this long ago
    Ago(Duration),
}

/// One agent using the player
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// The agent's own name, e.g. "claude-code"; `None` until it says
    pub name: Option<String>,
    /// The computer a network agent calls from; `None` for a local one
    pub address: Option<IpAddr>,
    pub seen: Seen,
}

impl AgentInfo {
    /// The agent's name, e.g. "claude-code"
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("Agent")
    }

    /// Where it calls from: "This computer", or the address of the
    /// computer on the network (one on this computer reads as such)
    pub fn place_label(&self) -> String {
        match self.address {
            Some(ip) if !ip.is_loopback() => ip.to_string(),
            _ => "This computer".to_string(),
        }
    }

    pub fn is_network(&self) -> bool {
        self.address.is_some()
    }

    pub fn is_connected(&self) -> bool {
        self.seen == Seen::Connected
    }

    /// "connected", "just now" or "4 min ago"
    pub fn seen_label(&self) -> String {
        match self.seen {
            Seen::Connected => "connected".to_string(),
            Seen::Ago(ago) if ago.as_secs() < 60 => "just now".to_string(),
            Seen::Ago(ago) => format!("{} min ago", ago.as_secs() / 60),
        }
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
    network: HashMap<NetworkId, Remote>,
}

/// A network agent's traces
struct Remote {
    ip: IpAddr,
    name: Option<String>,
    last_request: Instant,
    /// Event streams it holds open
    streams: u32,
    /// When its last stream closed
    stream_closed: Option<Instant>,
}

impl Remote {
    /// Still there at `now`
    fn present(&self, now: Instant) -> bool {
        if self.streams > 0 {
            return true;
        }
        match self.stream_closed {
            // It held a stream and has made no request since it closed:
            // gone, unless it reconnects
            Some(closed) if self.last_request <= closed => {
                now.saturating_duration_since(closed) < STREAM_GRACE
            }
            _ => now.saturating_duration_since(self.last_request) < AGENT_IDLE,
        }
    }

    fn seen(&self, now: Instant) -> Seen {
        if self.streams > 0 {
            Seen::Connected
        } else {
            Seen::Ago(now.saturating_duration_since(self.last_request))
        }
    }
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

/// Held while a network agent's event stream is open
pub struct StreamGuard {
    presence: Presence,
    id: NetworkId,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mut inner = self.presence.lock();
        if let Some(remote) = inner.network.get_mut(&self.id) {
            remote.streams = remote.streams.saturating_sub(1);
            if remote.streams == 0 {
                remote.stream_closed = Some(Instant::now());
            }
        }
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

    /// A network agent made a request, from `ip`
    pub fn network_seen(&self, id: NetworkId, ip: IpAddr) {
        self.network_seen_at(id, ip, Instant::now());
    }

    fn network_seen_at(&self, id: NetworkId, ip: IpAddr, at: Instant) {
        let mut inner = self.lock();
        let remote = inner.network.entry(id).or_insert(Remote {
            ip,
            name: None,
            last_request: at,
            streams: 0,
            stream_closed: None,
        });
        remote.ip = ip;
        remote.last_request = at;
    }

    /// A network agent opened its event stream; it counts as connected
    /// until the guard drops. `None` for an agent we don't know.
    pub fn stream_opened(&self, id: &NetworkId) -> Option<StreamGuard> {
        let mut inner = self.lock();
        let remote = inner.network.get_mut(id)?;
        remote.streams += 1;
        Some(StreamGuard {
            presence: self.clone(),
            id: id.clone(),
        })
    }

    /// A network agent ended its session
    pub fn network_left(&self, id: &NetworkId) {
        self.lock().network.remove(id);
    }

    /// An agent said its name
    pub fn set_name(&self, place: &Place, name: &str) {
        if name.is_empty() {
            return;
        }
        let name = Some(app_name(name).to_string());
        let mut inner = self.lock();
        match place {
            Place::Local(id) => {
                if let Some(slot) = inner.local.get_mut(id) {
                    slot.0 = name;
                }
            }
            Place::Network(id) => {
                // A client without a session names itself on every request,
                // the first one included, before that request is counted
                if let NetworkId::Address(ip) = id {
                    let now = Instant::now();
                    inner.network.entry(id.clone()).or_insert(Remote {
                        ip: *ip,
                        name: None,
                        last_request: now,
                        streams: 0,
                        stream_closed: None,
                    });
                }
                if let Some(remote) = inner.network.get_mut(id) {
                    remote.name = name;
                }
            }
        }
    }

    /// The agents using the player now: local ones first, then network
    /// ones by address
    pub fn agents(&self) -> Vec<AgentInfo> {
        self.agents_at(Instant::now())
    }

    fn agents_at(&self, now: Instant) -> Vec<AgentInfo> {
        let mut inner = self.lock();
        inner.network.retain(|_, remote| remote.present(now));
        let mut network: Vec<AgentInfo> = inner
            .network
            .values()
            .map(|remote| AgentInfo {
                name: remote.name.clone(),
                address: Some(remote.ip),
                seen: remote.seen(now),
            })
            .collect();
        network.sort_by(|a, b| (a.address, &a.name).cmp(&(b.address, &b.name)));
        // One entry per app, in the order they connected, with the names of
        // all its sessions
        let mut local: Vec<(Option<u32>, AgentInfo)> = Vec::new();
        for (name, app) in inner.local.values() {
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
                        address: None,
                        seen: Seen::Connected,
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
        presence.set_name(&first.place(), "claude-code");
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
        presence.set_name(&chat.place(), "claude-ai");
        presence.set_name(&agent_mode.place(), "local-agent-mode-radiotrope");
        presence.set_name(&other.place(), "claude-code");
        // Names we don't know pass as they are
        let codex = presence.local_connected(Some(300));
        presence.set_name(&codex.place(), "codex-mcp-client");
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

    fn session(id: &str) -> NetworkId {
        NetworkId::Session(id.into())
    }

    const LAN: &str = "192.168.1.20";

    #[test]
    fn network_agents_without_a_stream_drop_off_when_idle() {
        let presence = Presence::default();
        let ip: IpAddr = LAN.parse().unwrap();
        let start = Instant::now();
        presence.network_seen_at(session("a"), ip, start);
        presence.set_name(&Place::Network(session("a")), "claude-code");
        let agents = presence.agents_at(start + Duration::from_secs(150));
        assert_eq!(agents[0].describe(), "claude-code, 192.168.1.20");
        assert_eq!(agents[0].seen_label(), "2 min ago");
        assert!(!agents[0].is_connected());
        assert!(presence.agents_at(start + AGENT_IDLE).is_empty());
    }

    #[test]
    fn a_network_agent_with_its_stream_open_stays_connected() {
        let presence = Presence::default();
        let ip: IpAddr = LAN.parse().unwrap();
        let start = Instant::now();
        presence.network_seen_at(session("a"), ip, start);
        let stream = presence.stream_opened(&session("a")).unwrap();
        let agents = presence.agents_at(start + AGENT_IDLE * 3);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].seen_label(), "connected");
        // Quit: gone once the grace for a reconnect is over
        drop(stream);
        assert_eq!(presence.agents().len(), 1);
        let later = Instant::now() + STREAM_GRACE;
        assert!(presence.agents_at(later).is_empty());
    }

    #[test]
    fn a_network_agent_that_reconnects_its_stream_stays() {
        let presence = Presence::default();
        let ip: IpAddr = LAN.parse().unwrap();
        presence.network_seen(session("a"), ip);
        drop(presence.stream_opened(&session("a")).unwrap());
        let _again = presence.stream_opened(&session("a")).unwrap();
        let agents = presence.agents_at(Instant::now() + STREAM_GRACE * 2);
        assert_eq!(agents.len(), 1);
        assert!(agents[0].is_connected());
    }

    #[test]
    fn a_network_agent_that_ends_its_session_leaves_at_once() {
        let presence = Presence::default();
        let ip: IpAddr = LAN.parse().unwrap();
        presence.network_seen(session("a"), ip);
        let _stream = presence.stream_opened(&session("a")).unwrap();
        presence.network_left(&session("a"));
        assert!(presence.agents().is_empty());
        assert!(presence.stream_opened(&session("a")).is_none());
    }

    #[test]
    fn two_network_agents_on_one_computer_get_a_row_each() {
        let presence = Presence::default();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        presence.network_seen(session("a"), ip);
        presence.network_seen(session("b"), ip);
        presence.set_name(&Place::Network(session("a")), "codex-mcp-client");
        presence.set_name(&Place::Network(session("b")), "claude-code");
        let rows: Vec<_> = presence.agents().iter().map(AgentInfo::describe).collect();
        assert_eq!(
            rows,
            [
                "claude-code, This computer",
                "codex-mcp-client, This computer"
            ]
        );
    }

    #[test]
    fn a_client_without_a_session_is_known_by_its_address() {
        let presence = Presence::default();
        let ip: IpAddr = LAN.parse().unwrap();
        // It names itself during its first request, before that is counted
        let id = NetworkId::Address(ip);
        presence.set_name(&Place::Network(id.clone()), "new-agent");
        presence.network_seen(id, ip);
        let agents = presence.agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].describe(), "new-agent, 192.168.1.20");
        assert_eq!(agents[0].seen_label(), "just now");
    }

    #[test]
    fn a_name_for_an_agent_that_left_is_ignored() {
        let presence = Presence::default();
        let place = presence.local_connected(None).place();
        presence.set_name(&place, "late");
        presence.set_name(&Place::Network(session("gone")), "late");
        assert!(presence.agents().is_empty());
    }
}
