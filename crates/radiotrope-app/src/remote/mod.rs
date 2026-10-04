//! Remote Control: the Radiotrope Remote phone app drives the player
//!
//! Off until the user turns it on in Tools > Remote Control. Then the
//! player listens on every network on [`PORT`], announces itself over
//! mDNS, and pairs phones with a code it shows. Phones reach the player
//! through the same control layer as agents ([`crate::control`]).

pub mod mdns;
pub mod pairing;
pub mod server;
pub mod state;

#[cfg(test)]
mod tests;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use radiotrope_app::config::remote::PORT;
use radiotrope_app::data::remote::{Device, RemoteStore};
use radiotrope_app::data::settings::Settings;
use radiotrope_app::network::logo::LogoService;

use crate::control::Control;
use pairing::{Pairing, Shown};

/// The Remote API and its announcement, started and stopped as the
/// settings say
pub struct Remote {
    shared: server::Shared,
    running: Mutex<Option<Running>>,
    /// Counts the changes asked for, so a change set up late never undoes
    /// a newer one
    asked: AtomicU64,
}

struct Running {
    // Dropped in this order: the announcement first, so phones don't try
    // a server that is going away
    _announcer: Option<mdns::Announcer>,
    _server: server::Server,
}

/// What the dialog shows about Remote Control
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// "Off", "Phones can find this player", or what went wrong
    pub text: String,
    pub is_error: bool,
    /// Where phones reach the player, for Add by address, e.g.
    /// "192.168.1.20:8766"; empty while off
    pub addresses: Vec<String>,
}

/// What to do with the status once the server is set up
pub type ShowStatus = Box<dyn FnOnce(Status) + Send>;

impl Remote {
    pub fn new(control: Control, logos: Option<Arc<LogoService>>) -> Self {
        let store = RemoteStore::load_or_create().unwrap_or_else(|e| {
            eprintln!("Remote Control: can't read or save the paired phones: {e}");
            RemoteStore {
                player_id: radiotrope_app::data::remote::random_hex(16).unwrap_or_default(),
                devices: Vec::new(),
            }
        });
        Self::with_store(control, logos, store, None)
    }

    fn with_store(
        control: Control,
        logos: Option<Arc<LogoService>>,
        store: RemoteStore,
        store_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            shared: server::Shared {
                control,
                store: Arc::new(Mutex::new(store)),
                store_path,
                pairing: Arc::new(Mutex::new(Pairing::default())),
                name: Arc::from(mdns::computer_name()),
                logos,
                changes: Arc::new(AtomicU64::new(0)),
            },
            running: Mutex::new(None),
            asked: AtomicU64::new(0),
        }
    }

    /// The name phones see for these settings
    pub fn name_for(settings: &Settings) -> String {
        settings
            .remote_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(mdns::computer_name)
    }

    /// Start, restart or stop to match the settings, on a thread of its
    /// own (stopping waits for the port to be let go), then `show` the
    /// status. A change asked for after this one wins.
    pub fn apply_later(self: &Arc<Self>, settings: &Settings, show: ShowStatus) {
        let ours = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let remote = self.clone();
        let on = settings.remote_control;
        let name = Self::name_for(settings);
        let spawned = std::thread::Builder::new()
            .name("remote-setup".into())
            .spawn(move || {
                let mut running = remote.running.lock().unwrap_or_else(|e| e.into_inner());
                if remote.asked.load(Ordering::SeqCst) != ours {
                    return;
                }
                // Let go of the port before taking it again
                *running = None;
                let status = if on {
                    remote.start(&mut running, &name)
                } else {
                    remote.pairing().cancel();
                    Status {
                        text: "Off".into(),
                        ..Default::default()
                    }
                };
                drop(running);
                remote.shared.changes.fetch_add(1, Ordering::SeqCst);
                show(status);
            });
        if let Err(e) = spawned {
            eprintln!("Remote Control: can't set up the server: {e}");
        }
    }

    fn start(&self, running: &mut Option<Running>, name: &str) -> Status {
        let mut shared = self.shared.clone();
        shared.name = Arc::from(name);
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT));
        let server = match server::start(addr, shared) {
            Ok(server) => server,
            Err(text) => {
                return Status {
                    text,
                    is_error: true,
                    addresses: Vec::new(),
                }
            }
        };
        let player_id = self.store().player_id.clone();
        let (announcer, text, is_error) = match mdns::Announcer::start(name, &player_id, PORT) {
            Ok(a) => (
                Some(a),
                "Phones on this network can find the player".into(),
                false,
            ),
            // Phones can still add it by address
            Err(e) => (None, format!("{e}; phones can add it by address"), true),
        };
        let addresses = crate::mcp::network::interfaces()
            .into_iter()
            .filter(|i| i.usable)
            .map(|i| format!("{}:{}", i.ip, server.port()))
            .collect();
        *running = Some(Running {
            _announcer: announcer,
            _server: server,
        });
        Status {
            text,
            is_error,
            addresses,
        }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, RemoteStore> {
        self.shared.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pairing(&self) -> std::sync::MutexGuard<'_, Pairing> {
        self.shared
            .pairing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Counts what changed for the dialog: phones paired, removed or seen,
    /// a pairing started or ended
    pub fn changes(&self) -> u64 {
        self.shared.changes.load(Ordering::SeqCst)
    }

    /// The paired phones, used most recently first
    pub fn devices(&self) -> Vec<Device> {
        let mut devices = self.store().devices.clone();
        devices.sort_by_key(|d| std::cmp::Reverse(d.last_used.max(d.paired_at)));
        devices
    }

    /// Forget a phone; its token stops working at once
    pub fn remove_device(&self, id: &str) {
        if !self.store().remove(id) {
            return;
        }
        self.shared.changes.fetch_add(1, Ordering::SeqCst);
        let saved = radiotrope_app::data::remote::save_shared(
            &self.shared.store,
            self.shared.store_path.as_deref(),
        );
        if let Err(e) = saved {
            eprintln!("Remote Control: can't save the paired phones: {e}");
        }
    }

    /// The code to show while a phone pairs, and how long it has left
    pub fn shown_pairing(&self) -> Option<(Shown, Duration)> {
        let now = Instant::now();
        let pairing = self.pairing();
        Some((pairing.shown(now)?, pairing.time_left(now)?))
    }

    /// The user closed the code window
    pub fn cancel_pairing(&self) {
        self.pairing().cancel();
        self.shared.changes.fetch_add(1, Ordering::SeqCst);
    }

    /// The user opened the dialog: pairing works again after too many
    /// wrong codes
    pub fn unlock_pairing(&self) {
        self.pairing().unlock();
    }
}
