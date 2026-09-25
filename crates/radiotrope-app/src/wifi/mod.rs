//! WiFi management via iwd D-Bus interface
//!
//! Only compiled with the `embedded` feature flag.
//! Provides scanning, connecting, and status for WiFi networks
//! using Intel Wireless Daemon (iwd) over D-Bus.

mod iwd;

use std::fmt;

/// A discovered WiFi network
#[derive(Debug, Clone)]
pub struct WifiNetwork {
    /// Network SSID
    pub ssid: String,
    /// Signal strength in dBm (higher = stronger, typical range: -90 to -30)
    pub signal_dbm: i16,
    /// Security type
    pub security: WifiSecurity,
    /// Whether we're currently connected to this network
    pub connected: bool,
    /// D-Bus object path (used to identify network for connection)
    pub object_path: String,
}

impl WifiNetwork {
    /// Signal strength as percentage (0-100)
    pub fn signal_percent(&self) -> u8 {
        // Map dBm range [-90, -30] to [0, 100]
        let clamped = self.signal_dbm.clamp(-90, -30);
        (((clamped + 90) as f32 / 60.0) * 100.0) as u8
    }

    /// Signal bars (0-4)
    pub fn signal_bars(&self) -> u8 {
        match self.signal_percent() {
            0..=20 => 0,
            21..=40 => 1,
            41..=60 => 2,
            61..=80 => 3,
            _ => 4,
        }
    }
}

/// WiFi security type
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiSecurity {
    Open,
    Psk,        // WPA2-PSK / WPA3-SAE
    Enterprise, // 802.1x
}

impl fmt::Display for WifiSecurity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WifiSecurity::Open => write!(f, "Open"),
            WifiSecurity::Psk => write!(f, "WPA2"),
            WifiSecurity::Enterprise => write!(f, "Enterprise"),
        }
    }
}

/// WiFi connection state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiState {
    Disconnected,
    Scanning,
    Connecting,
    Connected { ssid: String },
    Error(String),
}

/// WiFi manager — wraps iwd D-Bus communication
pub struct WifiManager {
    runtime: tokio::runtime::Runtime,
}

impl WifiManager {
    /// Create a new WiFi manager
    pub fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;
        Ok(Self { runtime })
    }

    /// Scan for available WiFi networks
    pub fn scan(&self) -> Result<Vec<WifiNetwork>, String> {
        self.runtime.block_on(iwd::scan())
    }

    /// Connect to a WiFi network
    pub fn connect(&self, network_path: &str, passphrase: Option<&str>) -> Result<(), String> {
        self.runtime
            .block_on(iwd::connect(network_path, passphrase))
    }

    /// Disconnect from the current network
    pub fn disconnect(&self) -> Result<(), String> {
        self.runtime.block_on(iwd::disconnect())
    }

    /// Get current connection state
    pub fn state(&self) -> Result<WifiState, String> {
        self.runtime.block_on(iwd::get_state())
    }

    /// Get the currently connected SSID, if any
    pub fn current_ssid(&self) -> Option<String> {
        match self.state() {
            Ok(WifiState::Connected { ssid }) => Some(ssid),
            _ => None,
        }
    }
}
