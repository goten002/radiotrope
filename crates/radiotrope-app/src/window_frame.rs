//! Window actions for our own title bar (desktop builds)

use slint::winit_030::{winit, WinitWindowAccessor};

/// Minimize through winit. Slint's `minimized` property never resets on
/// Wayland (the compositor doesn't report it), so setting it again after the
/// first restore did nothing.
pub fn minimize(window: &slint::Window) {
    window.with_winit_window(|w| w.set_minimized(true));
}

/// Opens the system window menu at `(x, y)` (logical, window coordinates).
///
/// Called on the right button's press and on its release: Wayland needs the
/// press, Windows opens it on release. The answer on press decides: false
/// means there is no system menu (X11, macOS) and the caller shows ours.
pub fn show_system_menu(window: &slint::Window, x: f32, y: f32, pressed: bool) -> bool {
    window
        .with_winit_window(|w| {
            let pos = winit::dpi::LogicalPosition::new(x as f64, y as f64);
            platform_menu(w, pos, pressed)
        })
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn platform_menu(
    w: &winit::window::Window,
    pos: winit::dpi::LogicalPosition<f64>,
    pressed: bool,
) -> bool {
    if !pressed {
        w.show_window_menu(pos);
    }
    true
}

#[cfg(target_os = "linux")]
fn platform_menu(
    w: &winit::window::Window,
    pos: winit::dpi::LogicalPosition<f64>,
    pressed: bool,
) -> bool {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // X11 has no reliable way to ask for the window manager's menu from
    // here (`_GTK_SHOW_WINDOW_MENU` raced with our pointer grab and opened
    // about half the time under Xfce), so X11 gets our own menu
    match w.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Wayland(_)) => {
            if pressed {
                w.show_window_menu(pos);
            }
            true
        }
        _ => false,
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn platform_menu(
    _w: &winit::window::Window,
    _pos: winit::dpi::LogicalPosition<f64>,
    _pressed: bool,
) -> bool {
    false
}
