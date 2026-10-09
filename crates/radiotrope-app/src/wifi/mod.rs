//! Wi-Fi on the Raspberry Pi, through iwd's D-Bus API.
//!
//! Only compiled with the `embedded` feature. One thread keeps one
//! connection to the system bus for the life of the player, watches iwd's
//! station and device for changes, and runs the commands the Wi-Fi page
//! sends it (scan, connect, disconnect, forget, radio on/off). Everything
//! it learns comes back as [`WifiEvent`]s through the callback given to
//! [`WifiManager::start`], so the UI never blocks on D-Bus.

mod iwd;

use std::fmt;

/// A network iwd can see
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    /// Network SSID
    pub ssid: String,
    /// Signal strength in dBm (higher is stronger, typically -90 to -30)
    pub signal_dbm: i16,
    /// Security type
    pub security: WifiSecurity,
    /// Whether this is the network we are connected to
    pub connected: bool,
    /// Whether iwd has the network saved (it auto-connects to those)
    pub known: bool,
    /// D-Bus object path, what connect and forget refer to
    pub object_path: String,
}

impl WifiNetwork {
    /// Signal strength as 0-4 bars
    pub fn signal_bars(&self) -> u8 {
        signal_bars(self.signal_dbm)
    }
}

/// Signal strength in dBm as 0-4 bars: the usual phone thresholds
pub fn signal_bars(dbm: i16) -> u8 {
    match dbm {
        d if d >= -55 => 4,
        d if d >= -65 => 3,
        d if d >= -75 => 2,
        d if d >= -85 => 1,
        _ => 0,
    }
}

/// Signal strength in words, for the details of a network
pub fn signal_words(dbm: i16) -> &'static str {
    match signal_bars(dbm) {
        4 => "Excellent",
        3 => "Good",
        2 => "Fair",
        1 => "Weak",
        _ => "Very weak",
    }
}

/// Wi-Fi security type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiSecurity {
    Open,
    /// WPA2-PSK or WPA3-SAE (iwd calls both "psk")
    Psk,
    /// 802.1x
    Enterprise,
}

impl fmt::Display for WifiSecurity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WifiSecurity::Open => write!(f, "Open"),
            WifiSecurity::Psk => write!(f, "WPA2/WPA3"),
            WifiSecurity::Enterprise => write!(f, "Enterprise"),
        }
    }
}

/// Where the station is, as iwd reports it
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiState {
    /// No Wi-Fi adapter, or iwd is not running
    NoAdapter,
    /// The radio is off
    Off,
    Disconnected,
    Connecting,
    Connected,
    /// Connected, but iwd is still rolling to a better access point
    Roaming,
}

/// The station right now
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WifiStatus {
    pub state: Option<WifiState>,
    /// The network we are connected to (or connecting to)
    pub ssid: String,
    /// Our IPv4 address on the Wi-Fi interface, when we have one
    pub ip: String,
    /// Whether iwd is scanning right now
    pub scanning: bool,
}

impl WifiStatus {
    pub fn powered(&self) -> bool {
        !matches!(
            self.state,
            None | Some(WifiState::NoAdapter) | Some(WifiState::Off)
        )
    }
}

/// Why a command didn't work, in the user's words
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiError {
    NoAdapter,
    WrongPassword,
    /// The passphrase can't be right: WPA needs 8 to 63 characters
    PasswordFormat,
    NetworkNotFound,
    /// iwd is busy with the previous command
    Busy,
    /// Something else happened on the way; the text is iwd's
    Other(String),
}

impl fmt::Display for WifiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WifiError::NoAdapter => write!(f, "No Wi-Fi adapter found"),
            WifiError::WrongPassword => write!(f, "Wrong password. Try again."),
            WifiError::PasswordFormat => write!(f, "A Wi-Fi password is 8 to 63 characters"),
            WifiError::NetworkNotFound => write!(f, "Network out of range"),
            WifiError::Busy => write!(f, "Still busy with the previous network"),
            WifiError::Other(text) => write!(f, "{text}"),
        }
    }
}

/// What the Wi-Fi thread tells the UI
#[derive(Debug, Clone)]
pub enum WifiEvent {
    /// The station changed (connected, disconnected, radio off, scanning)
    Status(WifiStatus),
    /// A scan finished: the networks in range, strongest first
    Networks(Vec<WifiNetwork>),
    /// A connect finished
    Connected(Result<String, WifiError>),
    /// A disconnect, forget or power change failed
    Failed(WifiError),
}

/// What the UI asks the Wi-Fi thread to do
#[derive(Debug, Clone)]
pub enum WifiCommand {
    Scan,
    Connect {
        object_path: String,
        passphrase: Option<String>,
    },
    Disconnect,
    Forget {
        object_path: String,
    },
    SetPowered(bool),
    /// Send the status again (the page just opened)
    Refresh,
}

/// Handle to the Wi-Fi thread: send it commands
#[derive(Clone)]
pub struct WifiManager {
    tx: tokio::sync::mpsc::UnboundedSender<WifiCommand>,
}

impl WifiManager {
    /// Start the Wi-Fi thread. `on_event` is called from that thread for
    /// every change; it hands the event to the UI's event loop.
    pub fn start(on_event: impl Fn(WifiEvent) + Send + 'static) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let spawned = std::thread::Builder::new()
            .name("wifi".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("wifi: no runtime: {e}");
                        on_event(WifiEvent::Status(WifiStatus {
                            state: Some(WifiState::NoAdapter),
                            ..WifiStatus::default()
                        }));
                        return;
                    }
                };
                runtime.block_on(iwd::run(rx, on_event));
            });
        if let Err(e) = spawned {
            eprintln!("wifi: thread failed to start: {e}");
        }
        Self { tx }
    }

    pub fn send(&self, command: WifiCommand) {
        let _ = self.tx.send(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_follow_the_usual_thresholds() {
        assert_eq!(signal_bars(-40), 4);
        assert_eq!(signal_bars(-55), 4);
        assert_eq!(signal_bars(-56), 3);
        assert_eq!(signal_bars(-70), 2);
        assert_eq!(signal_bars(-80), 1);
        assert_eq!(signal_bars(-90), 0);
    }

    #[test]
    fn errors_read_as_sentences() {
        assert_eq!(
            WifiError::WrongPassword.to_string(),
            "Wrong password. Try again."
        );
        assert_eq!(WifiError::Other("x".into()).to_string(), "x");
    }

    #[test]
    fn powered_means_a_station_that_is_on() {
        let off = WifiStatus {
            state: Some(WifiState::Off),
            ..WifiStatus::default()
        };
        assert!(!off.powered());
        let on = WifiStatus {
            state: Some(WifiState::Disconnected),
            ..WifiStatus::default()
        };
        assert!(on.powered());
        assert!(!WifiStatus::default().powered());
    }
}
