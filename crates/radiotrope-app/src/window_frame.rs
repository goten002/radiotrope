//! Window actions for our own title bar (desktop builds)

use slint::winit_030::{winit, WinitWindowAccessor};

/// Minimize through winit. Slint's `minimized` property never resets on
/// Wayland (the compositor doesn't report it), so setting it again after the
/// first restore did nothing.
pub fn minimize(window: &slint::Window) {
    window.with_winit_window(|w| w.set_minimized(true));
}

/// Restore a minimized window and ask for focus. Wayland compositors may
/// only flag the window as wanting attention, since they give focus only
/// to what the user starts.
pub fn bring_to_front(window: &slint::Window) {
    MINIMIZED.set(false);
    window.with_winit_window(|w| {
        w.set_minimized(false);
        w.focus_window();
    });
}

thread_local! {
    /// When the window last lost the keyboard focus
    static FOCUS_LOST: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    /// The winit window the events last came from
    static WINDOW_SEEN: std::cell::Cell<Option<winit::window::WindowId>> =
        const { std::cell::Cell::new(None) };
    static ALT_GRAPH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CONTROL_ALT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether the window manager had the window minimized when last asked
    static MINIMIZED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// When the window manager was last asked
    static MINIMIZED_ASKED: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    /// When the window's focus or cover last changed: it may be on its
    /// way to or from the taskbar
    static WINDOW_CHANGED: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    /// Called when the user does something in the window
    static ON_ACTIVITY: std::cell::RefCell<Option<std::rc::Rc<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Call `active` whenever the user does something in the window (a key, a
/// click, the wheel, a touch) or it gets the focus: what rests while
/// nothing happens wakes up on it. One listener; a second call replaces it.
pub fn on_activity(active: impl Fn() + 'static) {
    ON_ACTIVITY.with(|a| *a.borrow_mut() = Some(std::rc::Rc::new(active)));
}

fn note_activity() {
    let active = ON_ACTIVITY.with(|a| a.borrow().clone());
    if let Some(active) = active {
        active();
    }
}

/// Slint's public key event has no AltGr flag and can strip Ctrl+Alt from
/// a Windows AltGr character. Keep the native modifier state for matching.
pub fn alt_graph_held() -> bool {
    ALT_GRAPH.get() || CONTROL_ALT.get()
}

/// Start watching the window's events: note when it loses the focus, for
/// [`was_in_front`], and call `new_window` once the system window exists
/// (again after Wayland makes a new one on show), for settings that only
/// a live window takes. Call once; it takes Slint's one event filter.
pub fn watch_window(window: &slint::Window, new_window: impl Fn() + 'static) {
    let new_window = std::rc::Rc::new(new_window);
    window.on_winit_window_event(move |window, event| {
        use winit::event::WindowEvent;
        match event {
            WindowEvent::KeyboardInput { .. }
            | WindowEvent::MouseInput { .. }
            | WindowEvent::MouseWheel { .. }
            | WindowEvent::Touch(_) => note_activity(),
            // Minimized to the taskbar or restored from it, among others:
            // the minimized flag is read again over the next moments, not
            // only at the next sample
            WindowEvent::Focused(_) | WindowEvent::Occluded(_) => {
                WINDOW_CHANGED.set(Some(std::time::Instant::now()));
                note_activity();
            }
            _ => {}
        }
        match event {
            WindowEvent::Focused(false) => {
                FOCUS_LOST.set(Some(std::time::Instant::now()));
                ALT_GRAPH.set(false);
                CONTROL_ALT.set(false);
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.logical_key
                    == winit::keyboard::Key::Named(winit::keyboard::NamedKey::AltGraph) =>
            {
                ALT_GRAPH.set(event.state.is_pressed());
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                CONTROL_ALT.set(state.control_key() && state.alt_key());
            }
            WindowEvent::Resized(_) | WindowEvent::Focused(true) => {
                let id = window.with_winit_window(|w| w.id());
                if id.is_some() && id != WINDOW_SEEN.get() {
                    WINDOW_SEEN.set(id);
                    // Outside the event, so the callback may use the window
                    let new_window = new_window.clone();
                    slint::Timer::single_shot(std::time::Duration::ZERO, move || new_window());
                }
            }
            _ => {}
        }
        slint::winit_030::EventResult::Propagate
    });
}

/// Whether the window has the focus, or had it until `grace` ago (a click
/// on the tray can take it away just before the click arrives)
pub fn was_in_front(window: &slint::Window, grace: std::time::Duration) -> bool {
    let focused = window.with_winit_window(|w| w.has_focus()).unwrap_or(false);
    focused || FOCUS_LOST.get().is_some_and(|lost| lost.elapsed() < grace)
}

/// Bring a shown window that other windows cover to the front, for a click
/// on the tray icon.
///
/// - Windows: winit's `focus_window`, which may take the foreground.
/// - X11: asks the window manager like a taskbar or pager does (source 2),
///   which it always does. winit asks as the app (source 1), which
///   focus-stealing prevention may turn into a blinking taskbar entry.
/// - Wayland: an app can't raise itself (winit's `focus_window` does
///   nothing there), but compositors give focus to a new window, so it is
///   hidden and shown again, the way the tray shows a hidden window. The
///   compositor picks where it goes, as after any hide.
pub fn raise(window: &slint::Window) {
    #[cfg(target_os = "linux")]
    {
        if wayland_window(window).is_some() {
            let _ = window.hide();
            let _ = window.show();
            return;
        }
        if let Some(id) = x11_window(window) {
            activate_x11(id);
            return;
        }
    }
    bring_to_front(window);
}

/// Whether the window manager has the window minimized. Windows and X11
/// say so; Wayland never does, which counts as no. On X11 this asks the X
/// server and waits for the answer: what runs often reads [`minimized`].
pub fn is_minimized(window: &slint::Window) -> bool {
    window
        .with_winit_window(|w| w.is_minimized())
        .flatten()
        .unwrap_or(false)
}

/// How often the window manager is asked whether the window is minimized
const MINIMIZED_SAMPLE: std::time::Duration = std::time::Duration::from_secs(1);

/// Ask the window manager whether the window is minimized and keep the
/// answer for [`minimized`]
pub fn sample_minimized(window: &slint::Window) -> bool {
    let minimized = is_minimized(window);
    MINIMIZED.set(minimized);
    MINIMIZED_ASKED.set(Some(std::time::Instant::now()));
    minimized
}

/// Keep [`minimized`] current. Called by the poll on every round; the
/// window manager is asked once a second, and on each round for a moment
/// after the window's focus or cover changed (minimizing and restoring
/// both do that).
pub fn refresh_minimized(window: &slint::Window) {
    let now = std::time::Instant::now();
    let recent = |at: Option<std::time::Instant>| {
        at.is_some_and(|at| now.duration_since(at) < MINIMIZED_SAMPLE)
    };
    if recent(WINDOW_CHANGED.get()) || !recent(MINIMIZED_ASKED.get()) {
        sample_minimized(window);
    }
}

/// Whether the window was minimized when last sampled (see
/// [`sample_minimized`]): costs nothing, for what asks every frame
pub fn minimized() -> bool {
    MINIMIZED.get()
}

/// A window on screen comes to the front (see [`raise`]), for a second
/// launch or an agent. Returns false for a hidden or minimized one, which
/// is shown the usual way instead.
pub fn raise_shown(window: &slint::Window) -> bool {
    if !window.is_visible() || sample_minimized(window) {
        return false;
    }
    raise(window);
    true
}

/// Hide a window the window manager has minimized. On X11 a minimized
/// window is already unmapped, so hiding it changed nothing and it stayed
/// on the taskbar; the window manager is told to let go of it the way
/// ICCCM 4.1.4 says, with an UnmapNotify sent to the root window.
pub fn hide_minimized(window: &slint::Window) {
    #[cfg(target_os = "linux")]
    let x11_id = x11_window(window);
    let _ = window.hide();
    // Shown again, it isn't minimized: not hidden once more on a stale flag
    MINIMIZED.set(false);
    #[cfg(target_os = "linux")]
    if let Some(id) = x11_id {
        withdraw_x11(id);
    }
}

/// The window's id when it is an X11 window
#[cfg(target_os = "linux")]
fn x11_window(window: &slint::Window) -> Option<u32> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window
        .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(h) => u32::try_from(h.window).ok(),
            RawWindowHandle::Xcb(h) => Some(h.window.get()),
            _ => None,
        })
        .flatten()
}

/// Ask the window manager to raise and focus `window` (EWMH
/// _NET_ACTIVE_WINDOW, source 2: on the user's behalf)
#[cfg(target_os = "linux")]
fn activate_x11(window: u32) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask};
    let Ok((conn, screen)) = x11rb::connect(None) else {
        return;
    };
    let Some(root) = conn.setup().roots.get(screen).map(|s| s.root) else {
        return;
    };
    let atom = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    let Some(atom) = atom else {
        return;
    };
    let event = ClientMessageEvent::new(32, window, atom.atom, [2, x11rb::CURRENT_TIME, 0, 0, 0]);
    let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
    // Wait for the answer so the event goes out before the connection closes
    let sent = conn.send_event(false, root, mask, event);
    if let Ok(sent) = sent {
        let _ = sent.check();
    }
}

#[cfg(target_os = "linux")]
fn withdraw_x11(window: u32) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt, EventMask, UnmapNotifyEvent, UNMAP_NOTIFY_EVENT};
    let Ok((conn, screen)) = x11rb::connect(None) else {
        return;
    };
    let Some(root) = conn.setup().roots.get(screen).map(|s| s.root) else {
        return;
    };
    let event = UnmapNotifyEvent {
        response_type: UNMAP_NOTIFY_EVENT,
        sequence: 0,
        event: root,
        window,
        from_configure: false,
    };
    let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
    // Waiting for the answer also makes sure the event went out before the
    // connection closes (it didn't always without)
    let sent = conn.send_event(false, root, mask, event);
    if let Ok(sent) = sent {
        let _ = sent.check();
    }
}

/// The window's id when it is a Wayland window
fn wayland_window(window: &slint::Window) -> Option<winit::window::WindowId> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window
        .with_winit_window(|w| {
            let handle = w.window_handle().ok()?;
            matches!(handle.as_raw(), RawWindowHandle::Wayland(_)).then(|| w.id())
        })
        .flatten()
}

thread_local! {
    static SIZED_WINDOW: std::cell::Cell<Option<winit::window::WindowId>> =
        const { std::cell::Cell::new(None) };
}

/// Wayland can't hide a window, so Slint destroys it on hide and makes a
/// new one on show. The new one lost its minimum size (Slint only sends
/// it when the layout's limits change) and could be shrunk to nothing.
/// Called often; gives each new Wayland window `min` again.
pub fn keep_min_size(window: &slint::Window, min: slint::LogicalSize) {
    let Some(id) = wayland_window(window) else {
        return;
    };
    if SIZED_WINDOW.get() == Some(id) {
        return;
    }
    SIZED_WINDOW.set(Some(id));
    let min = min.to_physical(window.scale_factor());
    window.with_winit_window(|w| {
        w.set_min_inner_size(Some(winit::dpi::PhysicalSize::new(min.width, min.height)));
    });
}

/// Slint makes the window when the event loop starts, shown or not, and
/// on Wayland a window that exists is on the taskbar. Started hidden, it
/// sat there empty, and the first Show didn't draw it. Hiding it again
/// destroys it, so the next Show makes a working one.
pub fn drop_hidden_window(window: &slint::Window) {
    if !window.is_visible() && wayland_window(window).is_some() {
        let _ = window.show();
        let _ = window.hide();
    }
}

/// Switch between the system title bar (`framed`) and ours while the app
/// runs, keeping the content the same size.
///
/// On Windows, winit changes the frame without changing the window's outer
/// size, so only the area inside the frame grows or shrinks, and the app
/// kept drawing at the old size: turning the system bar on showed the old
/// menu row and scroll bar twice, turning it off cut off the menu and the
/// right edge. Resizing the window so the inside keeps its size avoids that.
/// Elsewhere the window manager keeps the inside size already, and Slint
/// applies `no-frame` as before.
pub fn set_system_frame(window: &slint::Window, framed: bool) {
    #[cfg(target_os = "windows")]
    {
        let content = window.size();
        let resized = window
            .with_winit_window(|w| {
                if w.is_decorated() == framed {
                    return false;
                }
                w.set_decorations(framed);
                // A maximized or full screen window fills the screen either way
                !w.is_maximized() && w.fullscreen().is_none()
            })
            .unwrap_or(false);
        if resized {
            window.set_size(content);
        }
        window.request_redraw();
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (window, framed);
}

/// What rounded corners can do here, from [`set_corners`]
pub struct Corners {
    /// The desktop can show them
    pub available: bool,
    /// The app draws them itself (otherwise the system does)
    pub drawn: bool,
    /// Why they aren't available, for the greyed View item
    pub hint: &'static str,
}

/// Round the window's corners (or not) for our title bar.
///
/// - Windows 11 rounds the window itself and gives it its shadow, which
///   it also squares when maximized or snapped. Windows 10 has neither
///   setting and refuses it, so the corners aren't available there.
/// - Linux: the app draws them, leaving the corners of the window
///   see-through. That needs a compositor: every Wayland desktop has one,
///   on X11 one may be missing (the corners would show black), so it is
///   asked again on every call.
/// - macOS rounds a window that has the system's title bar, which is the
///   only kind the player has there; one without it would be ours to draw.
pub fn set_corners(window: &slint::Window, custom_frame: bool, rounded: bool) -> Corners {
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::WindowExtWindows;
        let available = window
            .with_winit_window(|w| {
                // Without the system frame winit drops the shadow, and with
                // it the rounding; this keeps both (no effect when framed)
                w.set_undecorated_shadow(true);
                // The system frame keeps the system's own choice
                let preference = match (custom_frame, rounded) {
                    (false, _) => DWMWCP_DEFAULT,
                    (true, true) => DWMWCP_ROUND,
                    (true, false) => DWMWCP_DONOTROUND,
                };
                dwm_corner_preference(w, preference)
            })
            .unwrap_or(true);
        Corners {
            available,
            drawn: false,
            hint: "Windows 11 only",
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (custom_frame, rounded);
        let available = x11_window(window).is_none() || x11_composited();
        Corners {
            available,
            drawn: true,
            hint: "Needs a compositor",
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (window, rounded);
        mac_corners(custom_frame)
    }
}

/// [`set_corners`] on macOS: the system rounds a window with its title bar
#[cfg(any(test, not(any(target_os = "windows", target_os = "linux"))))]
fn mac_corners(custom_frame: bool) -> Corners {
    Corners {
        available: true,
        drawn: custom_frame,
        hint: "",
    }
}

#[cfg(target_os = "windows")]
const DWMWCP_DEFAULT: i32 = 0;
#[cfg(target_os = "windows")]
const DWMWCP_DONOTROUND: i32 = 1;
#[cfg(target_os = "windows")]
const DWMWCP_ROUND: i32 = 2;

/// Set the window's DWM corner preference; false where Windows doesn't
/// know it (before Windows 11, build 22000). winit's own setter hides
/// the answer, which is how we tell.
#[cfg(target_os = "windows")]
fn dwm_corner_preference(w: &winit::window::Window, preference: i32) -> bool {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attribute: u32,
            value: *const core::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    let Ok(handle) = w.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return false;
    };
    // SAFETY: a live window handle, and the value is the i32 the
    // attribute takes
    let result = unsafe {
        DwmSetWindowAttribute(
            handle.hwnd.get(),
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&preference as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        )
    };
    result >= 0
}

/// Whether an X11 compositor runs (it owns the `_NET_WM_CM_Sn` selection),
/// so see-through pixels show what is behind the window
#[cfg(target_os = "linux")]
fn x11_composited() -> bool {
    use x11rb::protocol::xproto::ConnectionExt;
    let Ok((conn, screen)) = x11rb::connect(None) else {
        return false;
    };
    let name = format!("_NET_WM_CM_S{screen}");
    let Some(atom) = conn
        .intern_atom(false, name.as_bytes())
        .ok()
        .and_then(|cookie| cookie.reply().ok())
    else {
        return false;
    };
    conn.get_selection_owner(atom.atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|reply| reply.owner != x11rb::NONE)
}

/// Colour the system title bar for the View menu's theme (`"system"`,
/// `"dark"` or `"light"`). Only Windows needs it: its title bar follows the
/// OS app mode, which a picked Dark or Light theme may not match.
///
/// winit's `set_theme` on Windows only recolours the frame: the window keeps
/// following the OS (ThemeChanged still arrives, so System Theme and its
/// hint stay live), but an OS switch recolours the frame back to the OS
/// theme, so this runs again then too. On Linux, Slint gives the frame the
/// desktop's theme itself.
pub fn set_title_bar_theme(window: &slint::Window, mode: &str) {
    #[cfg(target_os = "windows")]
    window.with_winit_window(|w| {
        w.set_theme(match mode {
            "dark" => Some(winit::window::Theme::Dark),
            "light" => Some(winit::window::Theme::Light),
            _ => None,
        })
    });
    #[cfg(not(target_os = "windows"))]
    let _ = (window, mode);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_rounds_a_window_with_its_title_bar() {
        // The system's title bar (the only one there): nothing for us to
        // draw, and menus and dialogs are round with the window
        let framed = mac_corners(false);
        assert!(framed.available && !framed.drawn);
        // A window without it would be ours to round
        assert!(mac_corners(true).drawn);
    }
}
