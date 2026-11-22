//! The window's side of Remote Control: the Tools > Remote Control dialog
//! and the code shown while a phone pairs
//!
//! Every change is saved at once and the server set up again to match.
//! A timer shows what the server changed on its own: phones pairing,
//! paired or seen. It runs only while the server is on or the dialog is
//! open: with both off, nothing can change.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, VecModel};

use radiotrope_app::config::remote::{PORT, WEBRTC_PORTS};
use radiotrope_app::data::remote::Device;
use radiotrope_app::data::settings::Settings;

use crate::remote::{Remote, Status};
use crate::{App, RemoteDevice};

/// How often the window looks for a pairing phone or a change in the list
const REFRESH: std::time::Duration = std::time::Duration::from_millis(250);

/// Where to get the Radiotrope Remote app. The app is not released yet:
/// set this when it is, and the dialog shows a "Get the app" link.
const REMOTE_APP_URL: Option<&str> = None;

/// Wire the dialog and start the server if it is on. The timer must be
/// kept for as long as the window is.
pub fn setup(ui: &App, remote: Option<Arc<Remote>>, settings: &Settings) -> Rc<slint::Timer> {
    let timer = Rc::new(slint::Timer::default());
    ui.set_remote_app_url(REMOTE_APP_URL.unwrap_or_default().into());
    ui.set_remote_on(settings.remote_control);
    ui.set_remote_share_recordings(settings.remote_share_recordings);
    ui.set_remote_name(settings.remote_name.clone().unwrap_or_default().into());
    ui.set_remote_name_placeholder(crate::remote::mdns::computer_name().into());
    ui.set_remote_address_note(address_note().into());
    let Some(remote) = remote else { return timer };
    show_devices(ui, &remote.devices());
    remote.apply_later(settings, show_status_later(ui));

    // Wakes the stopped timer: the server was turned on, or the dialog opened
    let wake = {
        let timer = Rc::downgrade(&timer);
        move || {
            if let Some(timer) = timer.upgrade() {
                if !timer.running() {
                    timer.restart();
                }
            }
        }
    };

    let apply = {
        let ui_weak = ui.as_weak();
        let remote = remote.clone();
        let wake = wake.clone();
        move |change: &dyn Fn(&mut Settings)| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let settings = crate::window_settings::update_settings(change);
            ui.set_remote_on(settings.remote_control);
            ui.set_remote_share_recordings(settings.remote_share_recordings);
            ui.set_remote_name(settings.remote_name.clone().unwrap_or_default().into());
            remote.apply_later(&settings, show_status_later(&ui));
            if settings.remote_control {
                wake();
            }
        }
    };
    let apply = Rc::new(apply);

    ui.on_remote_toggled({
        let apply = apply.clone();
        move |on| apply(&|s| s.remote_control = on)
    });
    ui.on_remote_share_toggled({
        let apply = apply.clone();
        move |on| apply(&|s| s.remote_share_recordings = on)
    });
    ui.on_remote_apply_name({
        let apply = apply.clone();
        let ui_weak = ui.as_weak();
        move |text| {
            let name = text.trim().to_string();
            let saved = ui_weak
                .upgrade()
                .map(|ui| ui.get_remote_name().to_string())
                .unwrap_or_default();
            if name == saved {
                return;
            }
            // Empty: the computer's name again
            let name = (!name.is_empty()).then_some(name);
            apply(&|s| s.remote_name = name.clone());
        }
    });
    ui.on_remote_opened({
        let remote = remote.clone();
        let ui_weak = ui.as_weak();
        move || {
            wake();
            remote.unlock_pairing();
            if let Some(ui) = ui_weak.upgrade() {
                show_devices(&ui, &remote.devices());
            }
        }
    });
    ui.on_remote_remove_device({
        let remote = remote.clone();
        let ui_weak = ui.as_weak();
        move |id| {
            remote.remove_device(&id);
            if let Some(ui) = ui_weak.upgrade() {
                show_devices(&ui, &remote.devices());
            }
        }
    });
    ui.on_remote_cancel_pairing({
        let remote = remote.clone();
        let ui_weak = ui.as_weak();
        move || {
            remote.cancel_pairing();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pairing_shown(false);
            }
        }
    });

    ui.on_remote_pair_qr({
        let remote = remote.clone();
        move || {
            remote.start_qr_pairing();
        }
    });
    ui.on_remote_cancel_qr({
        let remote = remote.clone();
        let ui_weak = ui.as_weak();
        move || {
            remote.cancel_qr_pairing();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pairing_qr_shown(false);
            }
        }
    });

    let ui_weak = ui.as_weak();
    let this_timer = Rc::downgrade(&timer);
    // What the QR code drawn holds, so it is drawn again only when that
    // changes (a new code, or the computer's addresses)
    let mut drawn_qr = String::new();
    // Compared as shown, so "Used 4 min ago" redraws once a minute
    let mut seen_changes = remote.changes();
    let mut shown_devices: Vec<RemoteDevice> = Vec::new();
    timer.start(slint::TimerMode::Repeated, REFRESH, move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        match remote.shown_pairing() {
            Some((shown, left)) => {
                if !ui.get_pairing_shown() {
                    // The code is no use in a hidden window
                    crate::bring_to_front(ui.window());
                }
                ui.set_pairing_device(shown.device_name.into());
                ui.set_pairing_code_spoken(spoken_code(&shown.code).into());
                ui.set_pairing_code(shown.code.into());
                ui.set_pairing_seconds(left.as_secs_f32().ceil() as i32);
                ui.set_pairing_shown(true);
            }
            None => ui.set_pairing_shown(false),
        }
        match remote.shown_qr() {
            Some((payload, left)) => {
                if payload != drawn_qr {
                    if let Some(image) = qr_image(&payload) {
                        ui.set_pairing_qr(image);
                    }
                    drawn_qr = payload;
                }
                ui.set_pairing_qr_seconds(left.as_secs_f32().ceil() as i32);
                ui.set_pairing_qr_shown(true);
            }
            // Used by a phone, run out, or cancelled
            None => {
                ui.set_pairing_qr_shown(false);
                drawn_qr.clear();
            }
        }
        let changes = remote.changes();
        let rows = rows(&remote.devices(), now());
        if changes != seen_changes || rows != shown_devices {
            seen_changes = changes;
            ui.set_remote_devices(Rc::new(VecModel::from(rows.clone())).into());
            shown_devices = rows;
        }
        // Off and out of sight: sleep until the server or the dialog wakes it
        let pairing = ui.get_pairing_shown() || ui.get_pairing_qr_shown();
        if !keeps_running(ui.get_remote_on(), ui.get_dialog_open(), pairing) {
            if let Some(timer) = this_timer.upgrade() {
                timer.stop();
            }
        }
    });
    timer
}

/// The refresh timer is needed while the server runs, while a dialog (this
/// one, or the pairing overlay over it) may show what it finds, or while a
/// pairing overlay is still up
fn keeps_running(server_on: bool, dialog_open: bool, pairing_shown: bool) -> bool {
    server_on || dialog_open || pairing_shown
}

/// The pairing code digit by digit, so a screen reader says "4 8 3 6 2 1"
/// rather than "four hundred eighty-three thousand…"
fn spoken_code(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace())
        .map(String::from)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Empty modules around the code; the tile adds a little more
const QR_QUIET: usize = 4;

/// The QR code for `payload`, a pixel per module: black on transparent, so
/// the light theme's card shows through and the dark theme puts a white
/// tile behind it. Drawn pixelated at the size the dialog gives it
pub(crate) fn qr_image(payload: &str) -> Option<slint::Image> {
    use slint::{Rgba8Pixel, SharedPixelBuffer};
    let code =
        qrcode::QrCode::with_error_correction_level(payload.as_bytes(), qrcode::EcLevel::M).ok()?;
    let width = code.width();
    let size = width + 2 * QR_QUIET;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(size as u32, size as u32);
    let black = Rgba8Pixel::new(0, 0, 0, 255);
    let pixels = buffer.make_mut_slice();
    for (i, color) in code.to_colors().into_iter().enumerate() {
        if color == qrcode::Color::Dark {
            let (x, y) = (i % width + QR_QUIET, i / width + QR_QUIET);
            pixels[y * size + x] = black;
        }
    }
    Some(slint::Image::from_rgba8(buffer))
}

fn show_status_later(ui: &App) -> crate::remote::ShowStatus {
    let ui_weak = ui.as_weak();
    Box::new(move |status| {
        let _ = ui_weak.upgrade_in_event_loop(move |ui| show_status(&ui, &status));
    })
}

fn show_status(ui: &App, status: &Status) {
    ui.set_remote_status(status.text.as_str().into());
    ui.set_remote_status_error(status.is_error);
    ui.set_remote_status_short(short_status(status).into());
    ui.set_remote_address(status.addresses.join(", ").into());
}

/// The chip on the Remote Control row: a few words for an error, with the
/// full reason in the bubble next to it. Empty when nothing went wrong.
fn short_status(status: &Status) -> &'static str {
    match (status.is_error, status.addresses.is_empty()) {
        (false, _) => "",
        // No address: the server itself didn't start
        (true, true) => "Can't start",
        // The server runs, only phones can't find it by themselves
        (true, false) => "Not visible",
    }
}

fn show_devices(ui: &App, devices: &[Device]) {
    ui.set_remote_devices(Rc::new(VecModel::from(rows(devices, now()))).into());
}

fn rows(devices: &[Device], now: i64) -> Vec<RemoteDevice> {
    devices
        .iter()
        .map(|d| RemoteDevice {
            id: d.id.as_str().into(),
            name: d.name.as_str().into(),
            seen: seen_label(d.last_used.max(d.paired_at), now).into(),
        })
        .collect()
}

/// "Used just now", "Used 5 min ago", "Used 3 h ago", "Used 2 days ago"
fn seen_label(at: i64, now: i64) -> String {
    let ago = (now - at).max(0);
    match ago {
        0..=59 => "Used just now".into(),
        60..=3599 => format!("Used {} min ago", ago / 60),
        3600..=86_399 => format!("Used {} h ago", ago / 3600),
        86_400..=172_799 => "Used yesterday".into(),
        _ => format!("Used {} days ago", ago / 86_400),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The address bubble: what to do when a phone doesn't find the player,
/// and the ports it needs (control, then listening on the phone)
pub(crate) fn address_note() -> String {
    let windows = if cfg!(windows) {
        " If Windows asks, allow private networks."
    } else {
        ""
    };
    format!(
        "If a phone doesn't find the player, type this address in the app.{windows} {}",
        firewall_hint()
    )
}

/// "Your firewall must allow TCP 8766 and UDP 8767–8770."
fn firewall_hint() -> String {
    format!(
        "Your firewall must allow TCP {PORT} and UDP {}\u{2013}{}.",
        WEBRTC_PORTS.start(),
        WEBRTC_PORTS.end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_labels() {
        assert_eq!(seen_label(100, 130), "Used just now");
        assert_eq!(seen_label(0, 300), "Used 5 min ago");
        assert_eq!(seen_label(0, 3 * 3600 + 5), "Used 3 h ago");
        assert_eq!(seen_label(0, 90_000), "Used yesterday");
        assert_eq!(seen_label(0, 3 * 86_400), "Used 3 days ago");
        // A clock set back
        assert_eq!(seen_label(500, 100), "Used just now");
    }

    #[test]
    fn pairing_code_is_spoken_digit_by_digit() {
        assert_eq!(spoken_code("483 621"), "4 8 3 6 2 1");
        assert_eq!(spoken_code("482913"), "4 8 2 9 1 3");
    }

    #[test]
    fn address_note_names_every_port() {
        let note = address_note();
        assert!(note.contains("TCP 8766"), "{note}");
        assert!(note.contains("UDP 8767\u{2013}8770"), "{note}");
    }

    #[test]
    fn refresh_sleeps_only_when_nothing_can_change() {
        assert!(!keeps_running(false, false, false));
        assert!(keeps_running(true, false, false));
        assert!(keeps_running(false, true, false));
        assert!(keeps_running(false, false, true));
    }

    #[test]
    fn app_link_waits_for_a_release() {
        // Hidden until the app is out; then it must be a web link
        if let Some(url) = REMOTE_APP_URL {
            assert!(url.starts_with("https://"), "{url}");
        }
    }

    #[test]
    fn short_status_keeps_the_row_short() {
        let running = Status {
            text: "Phones on this network can find the player".into(),
            is_error: false,
            addresses: vec!["192.168.1.20:8766".into()],
        };
        assert_eq!(short_status(&running), "");
        assert_eq!(short_status(&Status::default()), "");

        let no_server = Status {
            text: "Can't start Remote Control: port 8766 is in use by another program. Close that program and try again.".into(),
            is_error: true,
            addresses: Vec::new(),
        };
        assert_eq!(short_status(&no_server), "Can't start");

        let not_announced = Status {
            text: "Phones can't find the player by themselves. Add it in the app by address. (no network)".into(),
            is_error: true,
            addresses: vec!["192.168.1.20:8766".into()],
        };
        assert_eq!(short_status(&not_announced), "Not visible");

        // The full reason stays for the bubble
        for status in [&no_server, &not_announced] {
            assert!(status.text.len() > short_status(status).len());
        }
    }
}
