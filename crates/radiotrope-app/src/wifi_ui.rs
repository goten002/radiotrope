//! Wi-Fi settings and Power Off / Restart (the Raspberry Pi build)

use std::time::Duration;

use slint::{ComponentHandle, ModelRc, VecModel};

use radiotrope_app::text::status;

use crate::{App, WifiNetworkEntry};

/// Power Off and Restart on the Pi. The player runs sandboxed as its own
/// user, so it can't call systemd itself: it creates a file in its runtime
/// directory and a root path unit (radiotrope-poweroff.path,
/// radiotrope-reboot.path) does the rest. Playback and a recording end
/// cleanly when systemd stops the service on the way down.
pub(crate) fn setup_power(ui: &App) {
    ui.on_power_requested(|restart| {
        let flag = if restart { "reboot" } else { "poweroff" };
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/run/radiotrope"));
        let path = dir.join(flag);
        match std::fs::write(&path, b"") {
            Ok(()) => println!("{flag} requested through {}", path.display()),
            Err(e) => eprintln!("{flag}: could not create {}: {e}", path.display()),
        }
    });
}

/// Wire the Wi-Fi page to the iwd thread (Raspberry Pi build only). The
/// thread reports through `WifiEvent`s, applied on the UI thread here.
pub(crate) fn setup(ui: &App) {
    use radiotrope_app::wifi::{WifiCommand, WifiEvent, WifiManager, WifiState};

    // Backspace for the on-screen keyboard (Slint has no string slicing)
    ui.on_wifi_backspace(|text| {
        let mut chars: Vec<char> = text.chars().collect();
        chars.pop();
        chars.into_iter().collect::<String>().into()
    });
    // The password field shows one dot per character until the eye is tapped
    ui.on_wifi_mask(|text| "•".repeat(text.chars().count()).into());

    let manager = {
        let ui_weak = ui.as_weak();
        WifiManager::start(move |event| {
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    apply_wifi_event(&ui, event);
                }
            });
        })
    };

    {
        let manager = manager.clone();
        let ui_weak = ui.as_weak();
        ui.on_wifi_scan_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_wifi_scanning(true);
            }
            manager.send(WifiCommand::Scan);
        });
    }
    {
        let manager = manager.clone();
        let ui_weak = ui.as_weak();
        ui.on_wifi_connect_requested(move |path, password| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_wifi_connection_status(status::CONNECTING.into());
            }
            let passphrase = (!password.is_empty()).then(|| password.to_string());
            manager.send(WifiCommand::Connect {
                object_path: path.to_string(),
                passphrase,
            });
        });
    }
    {
        let manager = manager.clone();
        ui.on_wifi_disconnect_requested(move || manager.send(WifiCommand::Disconnect));
    }
    {
        let manager = manager.clone();
        ui.on_wifi_forget_requested(move |path| {
            manager.send(WifiCommand::Forget {
                object_path: path.to_string(),
            });
            // The list shows "Saved": read it again
            manager.send(WifiCommand::Scan);
        });
    }
    {
        let manager = manager.clone();
        ui.on_wifi_power_requested(move |on| manager.send(WifiCommand::SetPowered(on)));
    }
    {
        let manager = manager.clone();
        ui.on_wifi_opened(move || {
            manager.send(WifiCommand::Refresh);
            manager.send(WifiCommand::Scan);
        });
    }

    /// Show what the Wi-Fi thread reported
    fn apply_wifi_event(ui: &App, event: WifiEvent) {
        match event {
            WifiEvent::Status(status) => {
                ui.set_wifi_powered(status.powered());
                ui.set_wifi_current_ssid(status.ssid.into());
                ui.set_wifi_ip(status.ip.into());
                ui.set_wifi_state(
                    match status.state {
                        None | Some(WifiState::NoAdapter) => "no-adapter",
                        Some(WifiState::Off) => "off",
                        Some(WifiState::Disconnected) => "disconnected",
                        Some(WifiState::Connecting) => "connecting",
                        Some(WifiState::Connected) | Some(WifiState::Roaming) => "connected",
                    }
                    .into(),
                );
                if status.scanning {
                    ui.set_wifi_scanning(true);
                }
            }
            WifiEvent::Networks(networks) => {
                let entries: Vec<WifiNetworkEntry> = networks
                    .iter()
                    .map(|n| WifiNetworkEntry {
                        ssid: n.ssid.clone().into(),
                        signal_bars: n.signal_bars() as i32,
                        signal_text: format!(
                            "{} ({} dBm)",
                            radiotrope_app::wifi::signal_words(n.signal_dbm),
                            n.signal_dbm
                        )
                        .into(),
                        security: n.security.to_string().into(),
                        open: n.security == radiotrope_app::wifi::WifiSecurity::Open,
                        connected: n.connected,
                        known: n.known,
                        object_path: n.object_path.clone().into(),
                    })
                    .collect();
                ui.set_wifi_networks(ModelRc::from(std::rc::Rc::new(VecModel::from(entries))));
                ui.set_wifi_scanning(false);
            }
            WifiEvent::Connected(Ok(ssid)) => {
                show_wifi_status(ui, format!("Connected to {ssid}"), false);
            }
            WifiEvent::Connected(Err(e)) => {
                show_wifi_status(ui, e.to_string(), true);
            }
            WifiEvent::Failed(e) => {
                show_wifi_status(ui, e.to_string(), true);
            }
        }
    }

    /// A line in the Wi-Fi page for a few seconds; a newer line is left alone
    fn show_wifi_status(ui: &App, text: String, is_error: bool) {
        let text: slint::SharedString = text.into();
        ui.set_wifi_connection_status(text.clone());
        ui.set_wifi_status_is_error(is_error);
        let ui_weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_secs(6), move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_wifi_connection_status() == text {
                    ui.set_wifi_connection_status("".into());
                }
            }
        });
    }
}
