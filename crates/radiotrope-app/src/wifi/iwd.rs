//! iwd D-Bus interface proxies and operations

use super::{WifiNetwork, WifiSecurity, WifiState};
use zbus::Connection;

// =============================================================================
// D-Bus proxy traits for iwd interfaces
// =============================================================================

#[zbus::proxy(
    interface = "net.connman.iwd.Station",
    default_service = "net.connman.iwd"
)]
trait Station {
    fn scan(&self) -> zbus::Result<()>;
    fn get_ordered_networks(&self) -> zbus::Result<Vec<(zbus::zvariant::OwnedObjectPath, i16)>>;
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn connected_network(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
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
    interface = "net.connman.iwd.SimpleAgent",
    default_service = "net.connman.iwd"
)]
trait SimpleAgent {
    fn release(&self) -> zbus::Result<()>;
    fn request_passphrase(&self, network: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<String>;
    fn cancel(&self, reason: &str) -> zbus::Result<()>;
}

// =============================================================================
// Agent implementation for providing passphrases
// =============================================================================

/// Agent that provides a passphrase when iwd requests one during connection
struct PassphraseAgent {
    passphrase: String,
}

#[zbus::interface(name = "net.connman.iwd.Agent")]
impl PassphraseAgent {
    fn release(&self) -> zbus::fdo::Result<()> {
        Ok(())
    }

    fn request_passphrase(
        &self,
        _network: zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::fdo::Result<String> {
        Ok(self.passphrase.clone())
    }

    fn cancel(&self, _reason: &str) -> zbus::fdo::Result<()> {
        Ok(())
    }
}

// =============================================================================
// High-level operations
// =============================================================================

/// Find the first wireless station object path
async fn find_station(conn: &Connection) -> Result<String, String> {
    // iwd creates objects under /net/connman/iwd/N/M
    // We need to find the one implementing net.connman.iwd.Station
    let proxy = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination("net.connman.iwd")
        .map_err(|e| e.to_string())?
        .path("/")
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| format!("Failed to create ObjectManager proxy: {}", e))?;

    let objects = proxy
        .get_managed_objects()
        .await
        .map_err(|e| format!("Failed to get managed objects: {}", e))?;

    for (path, interfaces) in &objects {
        if interfaces.contains_key("net.connman.iwd.Station") {
            return Ok(path.to_string());
        }
    }

    Err("No wireless station found. Is WiFi hardware available?".to_string())
}

/// Scan for available WiFi networks
pub async fn scan() -> Result<Vec<WifiNetwork>, String> {
    let conn = Connection::system()
        .await
        .map_err(|e| format!("D-Bus connection failed: {}", e))?;

    let station_path = find_station(&conn).await?;

    let station = StationProxy::builder(&conn)
        .path(station_path.as_str())
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| format!("Failed to create Station proxy: {}", e))?;

    // Trigger scan
    let _ = station.scan().await; // May fail if already scanning, that's OK

    // Wait for scan to complete
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Get ordered networks (sorted by signal strength)
    let networks = station
        .get_ordered_networks()
        .await
        .map_err(|e| format!("Failed to get networks: {}", e))?;

    let mut result = Vec::new();

    for (net_path, signal) in &networks {
        let net_proxy = NetworkProxy::builder(&conn)
            .path(net_path.as_ref())
            .map_err(|e| e.to_string())?
            .build()
            .await
            .map_err(|e| format!("Failed to create Network proxy: {}", e))?;

        let name = net_proxy.name().await.unwrap_or_default();
        let net_type = net_proxy.network_type().await.unwrap_or_default();
        let connected = net_proxy.connected().await.unwrap_or(false);

        if name.is_empty() {
            continue; // Skip hidden networks
        }

        let security = match net_type.as_str() {
            "open" => WifiSecurity::Open,
            "psk" => WifiSecurity::Psk,
            "8021x" => WifiSecurity::Enterprise,
            _ => WifiSecurity::Psk,
        };

        result.push(WifiNetwork {
            ssid: name,
            signal_dbm: *signal,
            security,
            connected,
            object_path: net_path.to_string(),
        });
    }

    Ok(result)
}

/// Connect to a WiFi network
pub async fn connect(network_path: &str, passphrase: Option<&str>) -> Result<(), String> {
    let conn = Connection::system()
        .await
        .map_err(|e| format!("D-Bus connection failed: {}", e))?;

    // If a passphrase is provided, register an agent to supply it
    if let Some(pass) = passphrase {
        let agent = PassphraseAgent {
            passphrase: pass.to_string(),
        };
        let agent_path = "/radiotrope/wifi_agent";

        conn.object_server()
            .at(agent_path, agent)
            .await
            .map_err(|e| format!("Failed to register agent: {}", e))?;

        // Register agent with iwd's AgentManager
        let am_proxy = zbus::Proxy::new(
            &conn,
            "net.connman.iwd",
            "/net/connman/iwd",
            "net.connman.iwd.AgentManager",
        )
        .await
        .map_err(|e| format!("Failed to create AgentManager proxy: {}", e))?;

        am_proxy
            .call_method(
                "RegisterAgent",
                &(zbus::zvariant::ObjectPath::try_from(agent_path).unwrap(),),
            )
            .await
            .map_err(|e| format!("Failed to register agent: {}", e))?;
    }

    // Connect
    let net_proxy = NetworkProxy::builder(&conn)
        .path(network_path)
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| format!("Failed to create Network proxy: {}", e))?;

    net_proxy
        .connect()
        .await
        .map_err(|e| format!("Connection failed: {}", e))?;

    // Unregister agent
    if passphrase.is_some() {
        let am_proxy = zbus::Proxy::new(
            &conn,
            "net.connman.iwd",
            "/net/connman/iwd",
            "net.connman.iwd.AgentManager",
        )
        .await
        .ok();

        if let Some(proxy) = am_proxy {
            let agent_path = "/radiotrope/wifi_agent";
            let _ = proxy
                .call_method(
                    "UnregisterAgent",
                    &(zbus::zvariant::ObjectPath::try_from(agent_path).unwrap(),),
                )
                .await;
        }
    }

    Ok(())
}

/// Disconnect from the current network
pub async fn disconnect() -> Result<(), String> {
    let conn = Connection::system()
        .await
        .map_err(|e| format!("D-Bus connection failed: {}", e))?;

    let station_path = find_station(&conn).await?;

    let station = StationProxy::builder(&conn)
        .path(station_path.as_str())
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| format!("Failed to create Station proxy: {}", e))?;

    station
        .disconnect()
        .await
        .map_err(|e| format!("Disconnect failed: {}", e))?;

    Ok(())
}

/// Get current WiFi state
pub async fn get_state() -> Result<WifiState, String> {
    let conn = Connection::system()
        .await
        .map_err(|e| format!("D-Bus connection failed: {}", e))?;

    let station_path = match find_station(&conn).await {
        Ok(p) => p,
        Err(_) => return Ok(WifiState::Disconnected),
    };

    let station = StationProxy::builder(&conn)
        .path(station_path.as_str())
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| format!("Failed to create Station proxy: {}", e))?;

    let state = station.state().await.unwrap_or_default();

    match state.as_str() {
        "connected" => {
            // Get connected network name
            if let Ok(net_path) = station.connected_network().await {
                let net_proxy = NetworkProxy::builder(&conn)
                    .path(net_path.as_ref())
                    .map_err(|e| e.to_string())?
                    .build()
                    .await
                    .map_err(|e| e.to_string())?;

                let ssid = net_proxy.name().await.unwrap_or_default();
                Ok(WifiState::Connected { ssid })
            } else {
                Ok(WifiState::Connected {
                    ssid: "Unknown".to_string(),
                })
            }
        }
        "connecting" => Ok(WifiState::Connecting),
        "scanning" => Ok(WifiState::Scanning),
        _ => Ok(WifiState::Disconnected),
    }
}
