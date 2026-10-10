//! The window's side of Remote Control: the Tools > Remote Control dialog
//! and the code shown while a phone pairs
//!
//! Every change is saved at once and the server set up again to match.
//! A timer shows what the server changed on its own: phones pairing,
//! paired or seen.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, VecModel};

use radiotrope_app::config::remote::PORT;
use radiotrope_app::data::remote::Device;
use radiotrope_app::data::settings::Settings;

use crate::remote::{Remote, Status};
use crate::{App, RemoteDevice};

/// How often the window looks for a pairing phone or a change in the list
const REFRESH: std::time::Duration = std::time::Duration::from_millis(250);

/// Wire the dialog and start the server if it is on. The timer must be
/// kept for as long as the window is.
pub fn setup(ui: &App, remote: Option<Arc<Remote>>, settings: &Settings) -> slint::Timer {
    let timer = slint::Timer::default();
    ui.set_remote_on(settings.remote_control);
    ui.set_remote_share_recordings(settings.remote_share_recordings);
    ui.set_remote_name(settings.remote_name.clone().unwrap_or_default().into());
    ui.set_remote_name_placeholder(crate::remote::mdns::computer_name().into());
    ui.set_remote_address_note(address_note().into());
    let Some(remote) = remote else { return timer };
    show_devices(ui, &remote.devices());
    remote.apply_later(settings, show_status_later(ui));

    let apply = {
        let ui_weak = ui.as_weak();
        let remote = remote.clone();
        move |change: &dyn Fn(&mut Settings)| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut settings = Settings::load().unwrap_or_default();
            change(&mut settings);
            if let Err(e) = settings.save() {
                eprintln!("Failed to save Remote Control settings: {e}");
            }
            ui.set_remote_on(settings.remote_control);
            ui.set_remote_share_recordings(settings.remote_share_recordings);
            ui.set_remote_name(settings.remote_name.clone().unwrap_or_default().into());
            remote.apply_later(&settings, show_status_later(&ui));
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
    // What the QR code drawn holds, so it is drawn again only when that
    // changes (a new code, or the computer's addresses)
    let mut drawn_qr = String::new();
    // Compared as shown, so "Used 4 min ago" redraws once a minute
    let mut seen_changes = remote.changes();
    let mut shown_devices: Vec<RemoteDevice> = Vec::new();
    let mut shown_activity: Vec<String> = Vec::new();
    timer.start(slint::TimerMode::Repeated, REFRESH, move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        match remote.shown_pairing() {
            Some((shown, left)) => {
                if !ui.get_pairing_shown() {
                    // The code is no use in a hidden window
                    crate::bring_to_front(ui.window());
                }
                ui.set_pairing_device(shown.device_name.into());
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
        let activity = activity_lines(&remote.recordings_activity(), chrono::Local::now());
        if activity != shown_activity {
            let model: Vec<slint::SharedString> = activity.iter().map(|l| l.into()).collect();
            ui.set_remote_activity(Rc::new(VecModel::from(model)).into());
            shown_activity = activity;
        }
    });
    timer
}

/// Empty modules around the code; the tile adds a little more
const QR_QUIET: usize = 4;

/// The QR code for `payload`, a pixel per module: black on transparent, so
/// the light theme's card shows through and the dark theme puts a white
/// tile behind it. Drawn pixelated at the size the dialog gives it
fn qr_image(payload: &str) -> Option<slint::Image> {
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
    ui.set_remote_address(status.addresses.join(", ").into());
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

/// How many of the phones' downloads and deletions the dialog shows
const ACTIVITY_SHOWN: usize = 5;

/// "Pixel 8 downloaded Jazz FM - 2026-10-08 20-15-03.mp3 · 14:02", newest
/// first; the date shows for days before today
fn activity_lines(
    activity: &[crate::remote::recordings::Activity],
    now: chrono::DateTime<chrono::Local>,
) -> Vec<String> {
    use chrono::TimeZone;
    activity
        .iter()
        .take(ACTIVITY_SHOWN)
        .map(|a| {
            let when = match chrono::Local.timestamp_opt(a.at, 0).single() {
                Some(t) if t.date_naive() == now.date_naive() => t.format("%H:%M").to_string(),
                Some(t) => t.format("%b %-d %H:%M").to_string(),
                None => String::new(),
            };
            let did = if a.deleted { "deleted" } else { "downloaded" };
            format!("{} {did} {} · {when}", a.device, a.file)
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

fn address_note() -> String {
    let firewall = if cfg!(windows) {
        "Windows may ask to allow Radiotrope on private networks: allow it."
    } else {
        "A firewall must let phones reach this port."
    };
    format!(
        "Phones on this network find the player by themselves. If one doesn't, add the player in the app by this address. Radiotrope listens on port {PORT}. {firewall}"
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
    fn activity_lines_say_who_did_what_when() {
        use crate::remote::recordings::Activity;
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 8, 15, 0, 0)
            .unwrap();
        let at = |h| {
            chrono::Local
                .with_ymd_and_hms(2026, 10, h, 14, 2, 0)
                .unwrap()
                .timestamp()
        };
        let a = |deleted, at| Activity {
            device: "Pixel 8".into(),
            deleted,
            file: "Jazz FM - 2026-10-08 20-15-03.mp3".into(),
            at,
        };
        let lines = activity_lines(&[a(false, at(8)), a(true, at(7))], now);
        assert_eq!(
            lines,
            [
                "Pixel 8 downloaded Jazz FM - 2026-10-08 20-15-03.mp3 · 14:02",
                "Pixel 8 deleted Jazz FM - 2026-10-08 20-15-03.mp3 · Oct 7 14:02",
            ]
        );
        assert_eq!(
            activity_lines(&vec![a(false, at(8)); 9], now).len(),
            ACTIVITY_SHOWN
        );
    }
}
