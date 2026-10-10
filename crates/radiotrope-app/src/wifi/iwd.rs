//! The iwd side of [`super::WifiManager`]: one system-bus connection, the
//! station it finds, the passphrase agent, and the loop that answers the
//! page's commands and iwd's property changes.

use std::future::poll_fn;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_core::Stream;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::Connection;

use super::{WifiCommand, WifiError, WifiEvent, WifiNetwork, WifiSecurity, WifiState, WifiStatus};

const IWD: &str = "net.connman.iwd";
const AGENT_PATH: &str = "/app/radiotrope/wifi/agent";
/// How long a scan may take before we read what iwd has
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);
/// How often to look for the station while there is none
const ADAPTER_RETRY: Duration = Duration::from_secs(5);

// ── iwd interfaces ──────────────────────────────────────────────────────

#[zbus::proxy(
    interface = "net.connman.iwd.Station",
    default_service = "net.connman.iwd"
)]
trait Station {
    fn scan(&self) -> zbus::Result<()>;
    fn get_ordered_networks(&self) -> zbus::Result<Vec<(OwnedObjectPath, i16)>>;
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn scanning(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn connected_network(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "net.connman.iwd.Network",
    default_service = "net.connman.iwd"
)]
trait Network {
    fn connect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Type")]
    fn network_type(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn known_network(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "net.connman.iwd.KnownNetwork",
    default_service = "net.connman.iwd"
)]
trait KnownNetwork {
    fn forget(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "net.connman.iwd.Device",
    default_service = "net.connman.iwd"
)]
trait Device {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_powered(&self, powered: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "net.connman.iwd.AgentManager",
    default_service = "net.connman.iwd",
    default_path = "/net/connman/iwd"
)]
trait AgentManager {
    fn register_agent(&self, path: ObjectPath<'_>) -> zbus::Result<()>;
}

// ── Passphrase agent ────────────────────────────────────────────────────

/// What iwd asks when a network needs a passphrase it doesn't have.
/// Registered once; `Connect` puts the passphrase in before it starts and
/// `asked` tells it afterwards whether iwd came for it (then a failure
/// means the password was wrong).
#[derive(Default)]
struct AgentShared {
    passphrase: Mutex<Option<String>>,
    asked: AtomicBool,
}

struct PassphraseAgent {
    shared: Arc<AgentShared>,
}

#[zbus::interface(name = "net.connman.iwd.Agent")]
impl PassphraseAgent {
    fn release(&self) {}

    fn request_passphrase(&self, _network: ObjectPath<'_>) -> zbus::fdo::Result<String> {
        self.shared.asked.store(true, Ordering::SeqCst);
        let passphrase = self
            .shared
            .passphrase
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        passphrase.ok_or_else(|| zbus::fdo::Error::Failed("no passphrase given".into()))
    }

    fn cancel(&self, _reason: &str) {}
}

// ── The station ─────────────────────────────────────────────────────────

/// The iwd station we talk to, with its device
struct Session {
    conn: Connection,
    station: StationProxy<'static>,
    device: DeviceProxy<'static>,
    /// The interface name (wlan0), for the IP address
    interface: String,
    agent: Arc<AgentShared>,
}

/// Translate a D-Bus error into the user's words
fn map_error(e: zbus::Error, asked_for_passphrase: bool) -> WifiError {
    match &e {
        zbus::Error::MethodError(name, message, _) => {
            let short = name
                .as_str()
                .strip_prefix("net.connman.iwd.")
                .unwrap_or(name.as_str());
            match short {
                "Failed" if asked_for_passphrase => WifiError::WrongPassword,
                "InvalidFormat" => WifiError::PasswordFormat,
                "NotFound" => WifiError::NetworkNotFound,
                "Busy" | "InProgress" => WifiError::Busy,
                "NotConfigured" => WifiError::Other("This network needs a password".into()),
                "org.freedesktop.DBus.Error.ServiceUnknown" => WifiError::NoAdapter,
                _ => WifiError::Other(
                    message
                        .clone()
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| format!("Wi-Fi error: {short}")),
                ),
            }
        }
        _ => WifiError::Other(format!("Wi-Fi error: {e}")),
    }
}

/// Whether the error means iwd itself is gone
fn is_service_gone(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, _, _)
        if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown")
        || matches!(e, zbus::Error::InputOutput(_))
}

impl Session {
    /// Find the (first) Wi-Fi station iwd manages
    async fn open(conn: &Connection, agent: Arc<AgentShared>) -> Result<Self, WifiError> {
        let manager = zbus::fdo::ObjectManagerProxy::builder(conn)
            .destination(IWD)
            .and_then(|b| b.path("/"))
            .map_err(|e| WifiError::Other(e.to_string()))?
            .build()
            .await
            .map_err(|e| WifiError::Other(e.to_string()))?;
        let objects = manager
            .get_managed_objects()
            .await
            .map_err(|e| map_error(zbus::Error::from(e), false))?;
        let path = objects
            .iter()
            .find(|(_, interfaces)| interfaces.contains_key("net.connman.iwd.Station"))
            .map(|(path, _)| path.clone())
            .ok_or(WifiError::NoAdapter)?;
        let station = StationProxy::builder(conn)
            .path(path.clone())
            .map_err(|e| WifiError::Other(e.to_string()))?
            .build()
            .await
            .map_err(|e| WifiError::Other(e.to_string()))?;
        let device = DeviceProxy::builder(conn)
            .path(path)
            .map_err(|e| WifiError::Other(e.to_string()))?
            .build()
            .await
            .map_err(|e| WifiError::Other(e.to_string()))?;
        let interface = device.name().await.unwrap_or_default();
        Ok(Self {
            conn: conn.clone(),
            station,
            device,
            interface,
            agent,
        })
    }

    async fn network(&self, path: &str) -> Result<NetworkProxy<'static>, WifiError> {
        NetworkProxy::builder(&self.conn)
            .path(path.to_string())
            .map_err(|e| WifiError::Other(e.to_string()))?
            .build()
            .await
            .map_err(|e| WifiError::Other(e.to_string()))
    }

    /// The station right now
    async fn status(&self) -> Result<WifiStatus, zbus::Error> {
        let powered = self.device.powered().await?;
        if !powered {
            return Ok(WifiStatus {
                state: Some(WifiState::Off),
                ..WifiStatus::default()
            });
        }
        let state = match self.station.state().await?.as_str() {
            "connected" => WifiState::Connected,
            "connecting" => WifiState::Connecting,
            "roaming" => WifiState::Roaming,
            _ => WifiState::Disconnected,
        };
        let scanning = self.station.scanning().await.unwrap_or(false);
        let mut ssid = String::new();
        let mut ip = String::new();
        if matches!(
            state,
            WifiState::Connected | WifiState::Connecting | WifiState::Roaming
        ) {
            if let Ok(path) = self.station.connected_network().await {
                if let Ok(network) = self.network(path.as_str()).await {
                    ssid = network.name().await.unwrap_or_default();
                }
            }
            if state == WifiState::Connected {
                ip = interface_ip(&self.interface);
            }
        }
        Ok(WifiStatus {
            state: Some(state),
            ssid,
            ip,
            scanning,
        })
    }

    /// Scan, wait for iwd to finish, and read what it sees
    async fn scan(&self) -> Result<Vec<WifiNetwork>, WifiError> {
        if let Err(e) = self.station.scan().await {
            // Already scanning: fine, we wait for that one
            let mapped = map_error(e, false);
            if mapped != WifiError::Busy {
                return Err(mapped);
            }
        }
        let deadline = tokio::time::Instant::now() + SCAN_TIMEOUT;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            match self.station.scanning().await {
                Ok(false) => break,
                Ok(true) if tokio::time::Instant::now() < deadline => continue,
                Ok(true) => break,
                Err(e) => return Err(map_error(e, false)),
            }
        }
        let ordered = self
            .station
            .get_ordered_networks()
            .await
            .map_err(|e| map_error(e, false))?;
        let mut networks = Vec::with_capacity(ordered.len());
        for (path, signal) in ordered {
            // iwd reports the strength in hundredths of a dBm (-6000 is
            // -60 dBm); the UI thinks in whole dBm
            let signal_dbm = signal / 100;
            let Ok(network) = self.network(path.as_str()).await else {
                continue;
            };
            let ssid = network.name().await.unwrap_or_default();
            if ssid.is_empty() {
                // Hidden: iwd can't connect to it from here anyway
                continue;
            }
            let security = match network.network_type().await.unwrap_or_default().as_str() {
                "open" => WifiSecurity::Open,
                "8021x" => WifiSecurity::Enterprise,
                _ => WifiSecurity::Psk,
            };
            // The property is absent while the network is not saved
            let known = network.known_network().await.is_ok();
            networks.push(WifiNetwork {
                ssid,
                signal_dbm,
                security,
                connected: network.connected().await.unwrap_or(false),
                known,
                object_path: path.to_string(),
            });
        }
        Ok(networks)
    }

    /// Connect, with the passphrase ready for iwd to ask for
    async fn connect(&self, path: &str, passphrase: Option<String>) -> Result<String, WifiError> {
        if let Some(p) = &passphrase {
            if p.chars().count() < 8 || p.len() > 63 {
                return Err(WifiError::PasswordFormat);
            }
        }
        *self
            .agent
            .passphrase
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = passphrase;
        self.agent.asked.store(false, Ordering::SeqCst);
        // iwd forgets our agent when it restarts, so register every time;
        // "already registered" is the usual answer
        if let Ok(manager) = AgentManagerProxy::new(&self.conn).await {
            if let Err(e) = manager
                .register_agent(ObjectPath::try_from(AGENT_PATH).unwrap())
                .await
            {
                if !matches!(&e, zbus::Error::MethodError(name, _, _)
                    if name.as_str() == "net.connman.iwd.AlreadyExists")
                {
                    eprintln!("wifi: agent not registered: {e}");
                }
            }
        }
        let network = self.network(path).await?;
        let result = network.connect().await;
        let asked = self.agent.asked.load(Ordering::SeqCst);
        *self
            .agent
            .passphrase
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        match result {
            Ok(()) => Ok(network.name().await.unwrap_or_default()),
            Err(e) => {
                // iwd's own words go to the journal: "Failed" after a
                // passphrase reads as a wrong password to the user, but
                // it can also be the driver or the kernel crypto
                eprintln!("wifi: connect to {path} failed (asked for passphrase: {asked}): {e}");
                Err(map_error(e, asked))
            }
        }
    }

    async fn forget(&self, path: &str) -> Result<(), WifiError> {
        let network = self.network(path).await?;
        let known = network
            .known_network()
            .await
            .map_err(|_| WifiError::Other("This network is not saved".into()))?;
        let known = KnownNetworkProxy::builder(&self.conn)
            .path(known)
            .map_err(|e| WifiError::Other(e.to_string()))?
            .build()
            .await
            .map_err(|e| WifiError::Other(e.to_string()))?;
        known.forget().await.map_err(|e| map_error(e, false))
    }
}

/// Our IPv4 address on `interface`, or "" while there is none
fn interface_ip(interface: &str) -> String {
    let Ok(all) = if_addrs::get_if_addrs() else {
        return String::new();
    };
    all.iter()
        .filter(|i| i.name == interface)
        .find_map(|i| match i.addr {
            if_addrs::IfAddr::V4(ref v4) => Some(v4.ip.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The next item of a property stream (zbus streams are futures-core
/// streams, which have no `next` of their own)
async fn next<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

// ── The loop ────────────────────────────────────────────────────────────

/// Run until the command channel closes
pub async fn run(mut rx: UnboundedReceiver<WifiCommand>, on_event: impl Fn(WifiEvent)) {
    let agent = Arc::new(AgentShared::default());
    let conn = loop {
        match Connection::system().await {
            Ok(conn) => break conn,
            Err(e) => {
                eprintln!("wifi: system bus: {e}");
                on_event(WifiEvent::Status(WifiStatus {
                    state: Some(WifiState::NoAdapter),
                    ..WifiStatus::default()
                }));
                tokio::select! {
                    _ = tokio::time::sleep(ADAPTER_RETRY) => {}
                    cmd = rx.recv() => {
                        if cmd.is_none() {
                            return;
                        }
                        on_event(WifiEvent::Failed(WifiError::NoAdapter));
                    }
                }
            }
        }
    };
    if let Err(e) = conn
        .object_server()
        .at(
            AGENT_PATH,
            PassphraseAgent {
                shared: agent.clone(),
            },
        )
        .await
    {
        eprintln!("wifi: passphrase agent: {e}");
    }

    loop {
        let session = match Session::open(&conn, agent.clone()).await {
            Ok(session) => session,
            Err(e) => {
                if e != WifiError::NoAdapter {
                    eprintln!("wifi: {e}");
                }
                on_event(WifiEvent::Status(WifiStatus {
                    state: Some(WifiState::NoAdapter),
                    ..WifiStatus::default()
                }));
                // Answer what comes in meanwhile, and look again
                tokio::select! {
                    _ = tokio::time::sleep(ADAPTER_RETRY) => {}
                    cmd = rx.recv() => match cmd {
                        None => return,
                        Some(WifiCommand::Refresh) => {}
                        Some(_) => on_event(WifiEvent::Failed(WifiError::NoAdapter)),
                    }
                }
                continue;
            }
        };
        if !run_session(&session, &mut rx, &on_event).await {
            return;
        }
    }
}

/// Report the station as it is now; false when iwd is gone
async fn send_status(session: &Session, on_event: &impl Fn(WifiEvent)) -> bool {
    match session.status().await {
        Ok(status) => {
            on_event(WifiEvent::Status(status));
            true
        }
        Err(e) => {
            if is_service_gone(&e) {
                return false;
            }
            eprintln!("wifi: status: {e}");
            true
        }
    }
}

/// Serve the page from one station. Returns false when the command
/// channel closed, true when iwd went away and a new session is needed.
async fn run_session(
    session: &Session,
    rx: &mut UnboundedReceiver<WifiCommand>,
    on_event: &impl Fn(WifiEvent),
) -> bool {
    let mut state_changes = session.station.receive_state_changed().await;
    let mut network_changes = session.station.receive_connected_network_changed().await;
    let mut scanning_changes = session.station.receive_scanning_changed().await;
    let mut power_changes = session.device.receive_powered_changed().await;

    if !send_status(session, on_event).await {
        return true;
    }

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return false };
                match cmd {
                    WifiCommand::Refresh => {}
                    WifiCommand::Scan => match session.scan().await {
                        Ok(networks) => on_event(WifiEvent::Networks(networks)),
                        Err(WifiError::NoAdapter) => return true,
                        Err(e) => on_event(WifiEvent::Failed(e)),
                    },
                    WifiCommand::Connect { object_path, passphrase } => {
                        let result = session.connect(&object_path, passphrase).await;
                        if result == Err(WifiError::NoAdapter) {
                            return true;
                        }
                        on_event(WifiEvent::Connected(result));
                    }
                    WifiCommand::Disconnect => {
                        if let Err(e) = session.station.disconnect().await {
                            let e = map_error(e, false);
                            if e == WifiError::NoAdapter {
                                return true;
                            }
                            on_event(WifiEvent::Failed(e));
                        }
                    }
                    WifiCommand::Forget { object_path } => {
                        if let Err(e) = session.forget(&object_path).await {
                            on_event(WifiEvent::Failed(e));
                        }
                    }
                    WifiCommand::SetPowered(on) => {
                        if let Err(e) = session.device.set_powered(on).await {
                            on_event(WifiEvent::Failed(map_error(e, false)));
                        }
                    }
                }
                if !send_status(session, on_event).await {
                    return true;
                }
            }
            changed = next(&mut state_changes) => {
                if changed.is_none() { return true; }
                if !send_status(session, on_event).await { return true; }
            }
            changed = next(&mut network_changes) => {
                if changed.is_none() { return true; }
                if !send_status(session, on_event).await { return true; }
            }
            changed = next(&mut scanning_changes) => {
                if changed.is_none() { return true; }
                if !send_status(session, on_event).await { return true; }
            }
            changed = next(&mut power_changes) => {
                if changed.is_none() { return true; }
                if !send_status(session, on_event).await { return true; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method_error(name: &str) -> zbus::Error {
        let msg = zbus::message::Message::method_call("/", "X")
            .unwrap()
            .build(&())
            .unwrap();
        zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from(name).unwrap(),
            Some("detail".to_string()),
            msg,
        )
    }

    #[test]
    fn iwd_errors_become_words() {
        assert_eq!(
            map_error(method_error("net.connman.iwd.Failed"), true),
            WifiError::WrongPassword
        );
        assert_eq!(
            map_error(method_error("net.connman.iwd.Failed"), false),
            WifiError::Other("detail".into())
        );
        assert_eq!(
            map_error(method_error("net.connman.iwd.InvalidFormat"), false),
            WifiError::PasswordFormat
        );
        assert_eq!(
            map_error(method_error("net.connman.iwd.NotFound"), false),
            WifiError::NetworkNotFound
        );
        assert_eq!(
            map_error(method_error("net.connman.iwd.InProgress"), false),
            WifiError::Busy
        );
        assert_eq!(
            map_error(
                method_error("org.freedesktop.DBus.Error.ServiceUnknown"),
                false
            ),
            WifiError::NoAdapter
        );
    }

    #[test]
    fn no_address_without_the_interface() {
        assert_eq!(interface_ip("no-such-interface-0"), "");
    }
}
