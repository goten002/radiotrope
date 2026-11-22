//! The system tray icon (desktop builds)
//!
//! Windows shows it in the notification area, Linux desktops through the
//! StatusNotifierItem D-Bus service (KDE, XFCE, Cinnamon, and GNOME with
//! the AppIndicator extension). The menu is the `tray-icon` crate's, kept
//! in step with the window by the window's poll (no timer of its own):
//! everything it does goes through the window's own callbacks, so it acts
//! like the window's controls do.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model};
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use radiotrope_app::config::ui::{TRAY_FAVORITES, TRAY_FRONT_GRACE, TRAY_TEXT_CHARS};
use radiotrope_app::text::{self, status, SEPARATOR};

use crate::app::state::PlayerPhase;
use crate::{App, Defaults};

/// Our name on the D-Bus tray, which KDE keeps the icon's settings under
const TRAY_ID: &str = radiotrope_app::config::APP_ID;

const SHOW: &str = "show";
const PLAY: &str = "play";
const MUTE: &str = "mute";
const RECORD: &str = "record";
const QUIT: &str = "quit";
const FAVORITE: &str = "fav:";
const SLEEP: &str = "sleep:";

/// The sleep timer's lengths in the tray menu, in minutes, as the
/// Sleep Timer dialog's chips
const SLEEP_LENGTHS: [(u32, &str); 5] = [
    (15, "15 min"),
    (30, "30 min"),
    (45, "45 min"),
    (60, "1 h"),
    (120, "2 h"),
];

/// What a click in the tray asks for
#[derive(Debug, Clone, PartialEq)]
enum Action {
    ToggleWindow,
    PlayStop,
    Mute,
    Record,
    Favorite(String),
    /// Start the sleep timer for this many minutes; 0 turns it off
    Sleep(u32),
    Quit,
}

impl Action {
    fn from_menu(id: &str) -> Option<Self> {
        Some(match id {
            SHOW => Action::ToggleWindow,
            PLAY => Action::PlayStop,
            MUTE => Action::Mute,
            RECORD => Action::Record,
            QUIT => Action::Quit,
            _ => match id.strip_prefix(SLEEP) {
                Some(minutes) => Action::Sleep(minutes.parse().ok()?),
                None => Action::Favorite(id.strip_prefix(FAVORITE)?.to_string()),
            },
        })
    }
}

/// What the player shows, as the tray needs it
#[derive(Debug, Clone, Default, PartialEq)]
struct Player {
    window_shown: bool,
    station: String,
    station_url: String,
    song: String,
    phase: PlayerPhase,
    muted: bool,
    recording: bool,
    /// How long the recording runs, in seconds
    recording_secs: u64,
    can_record: bool,
    /// When the sleep timer stops playback ("23:30"), empty when off
    sleep_until: String,
    /// (id, name, url), in the window's order: the first
    /// [`TRAY_FAVORITES`], which are all the menu lists
    favorites: Vec<(String, String, String)>,
    /// The window lists more favorites than these
    more_favorites: bool,
}

/// The tray menu and tooltip for a [`Player`]
#[derive(Debug, Clone, PartialEq)]
struct View {
    header: String,
    tooltip: String,
    show_label: &'static str,
    play_label: &'static str,
    can_play: bool,
    muted: bool,
    record_label: &'static str,
    can_record: bool,
    /// The Sleep Timer runs: Stop Timer can be picked
    sleeping: bool,
    /// (id, label, ticked)
    favorites: Vec<(String, String, bool)>,
    /// The line under the favorites, greyed: there are none, or more than
    /// the menu lists
    favorites_note: Option<&'static str>,
}

/// The header of a station that didn't start
const FAILED: &str = "Can't connect";
const NO_FAVORITES: &str = "No favorites yet";
/// Under the last favorite the menu lists, when the window has more
const MORE_FAVORITES: &str = "More in the window";

impl View {
    fn of(p: &Player) -> Self {
        let active = p.phase.is_active();
        let station = cut(&p.station);
        let song = cut(&p.song);
        // The song stays out of the menu (it made the menu very wide); the
        // tooltip still carries it.
        let header = match p.phase {
            PlayerPhase::Failed if !station.is_empty() => {
                format!("{station}{SEPARATOR}{FAILED}")
            }
            PlayerPhase::Reconnecting => {
                format!("{station}{SEPARATOR}{}", status::RECONNECTING)
            }
            phase if phase.is_loading() => format!("{station}{SEPARATOR}{}", status::CONNECTING),
            PlayerPhase::Playing => station.clone(),
            _ => "Not playing".to_string(),
        };
        let mut tooltip = match (active, song.is_empty()) {
            (false, _) => "Radiotrope".to_string(),
            (true, true) => format!("Radiotrope\n{station}"),
            (true, false) => format!("Radiotrope\n{station}\n{song}"),
        };
        if p.phase == PlayerPhase::Failed && !station.is_empty() {
            tooltip.push_str(&format!("\n{station}{SEPARATOR}{FAILED}"));
        }
        // To the minute: the tooltip isn't sent again every second
        if p.recording {
            tooltip.push_str(&format!(
                "\nRecording{SEPARATOR}{}",
                text::minutes(p.recording_secs.max(1))
            ));
        }
        if !p.sleep_until.is_empty() {
            tooltip.push_str(&format!("\nSleep Timer stops at {}", p.sleep_until));
        }
        let favorites = p
            .favorites
            .iter()
            .take(TRAY_FAVORITES)
            .map(|(id, name, url)| (id.clone(), cut(name), active && *url == p.station_url))
            .collect();
        View {
            header,
            tooltip,
            show_label: if p.window_shown {
                "Hide Radiotrope"
            } else {
                "Show Radiotrope"
            },
            play_label: if active { "Stop" } else { "Play" },
            can_play: active || !p.station_url.is_empty(),
            muted: p.muted,
            record_label: if p.recording {
                "Stop Recording"
            } else {
                "Start Recording"
            },
            can_record: p.recording || p.can_record,
            sleeping: !p.sleep_until.is_empty(),
            favorites_note: if p.favorites.is_empty() {
                Some(NO_FAVORITES)
            } else if p.more_favorites {
                Some(MORE_FAVORITES)
            } else {
                None
            },
            favorites,
        }
    }
}

/// `text` cut to [`TRAY_TEXT_CHARS`] characters
fn cut(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= TRAY_TEXT_CHARS {
        return text.to_string();
    }
    let mut short: String = text.chars().take(TRAY_TEXT_CHARS - 1).collect();
    short.push('…');
    short
}

/// The seconds of a running time as the window shows it ("4:05",
/// "1:02:03"); 0 for anything else
fn clock_secs(clock: &str) -> u64 {
    clock
        .split(':')
        .try_fold(0u64, |secs, part| {
            Some(secs * 60 + part.parse::<u64>().ok()?)
        })
        .unwrap_or(0)
}

/// Menu text shown as written: `&` marks a keyboard letter in menus on
/// both systems
fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

struct Items {
    header: MenuItem,
    show: MenuItem,
    play: MenuItem,
    mute: CheckMenuItem,
    favorites: Submenu,
    favorite_items: Vec<CheckMenuItem>,
    /// The greyed line under the favorites
    favorites_note: Option<MenuItem>,
    record: MenuItem,
    sleep_off: MenuItem,
}

struct Tray {
    icon: TrayIcon,
    items: Items,
    shown: Option<View>,
}

impl Tray {
    fn new() -> Result<Self, String> {
        let header = MenuItem::new("Not playing", false, None);
        let show = MenuItem::with_id(SHOW, "Hide Radiotrope", true, None);
        let play = MenuItem::with_id(PLAY, "Play", false, None);
        let mute = CheckMenuItem::with_id(MUTE, "Mute", true, false, None);
        let favorites = Submenu::new("Favorites", true);
        let record = MenuItem::with_id(RECORD, "Start Recording", false, None);
        let sleep = Submenu::new("Sleep Timer", true);
        for (minutes, label) in SLEEP_LENGTHS {
            let item = MenuItem::with_id(format!("{SLEEP}{minutes}"), label, true, None);
            sleep.append(&item).map_err(|e| e.to_string())?;
        }
        let sleep_off = MenuItem::with_id(format!("{SLEEP}0"), "Stop Timer", false, None);
        sleep
            .append_items(&[&PredefinedMenuItem::separator(), &sleep_off])
            .map_err(|e| e.to_string())?;
        let quit = MenuItem::with_id(QUIT, "Quit Radiotrope", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &header,
            &PredefinedMenuItem::separator(),
            &show,
            &PredefinedMenuItem::separator(),
            &play,
            &mute,
            &favorites,
            &record,
            &sleep,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| e.to_string())?;
        let icon = TrayIconBuilder::new()
            .with_id(TRAY_ID)
            .with_icon(app_icon()?)
            .with_tooltip("Radiotrope")
            .with_title("Radiotrope")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Tray {
            icon,
            items: Items {
                header,
                show,
                play,
                mute,
                favorites,
                favorite_items: Vec::new(),
                favorites_note: None,
                record,
                sleep_off,
            },
            shown: None,
        })
    }

    /// Bring the menu and tooltip up to `view`, changing only what differs.
    /// `clicked`: a menu item was just picked. A click ticks or unticks a
    /// check item on its own, so the ticks are all set again.
    fn show(&mut self, view: View, clicked: bool) {
        let old = self.shown.as_ref();
        let items = &mut self.items;
        if old.is_none_or(|o| o.header != view.header) {
            items.header.set_text(menu_text(&view.header));
        }
        if old.is_none_or(|o| o.tooltip != view.tooltip) {
            let _ = self.icon.set_tooltip(Some(&view.tooltip));
        }
        if old.is_none_or(|o| o.show_label != view.show_label) {
            items.show.set_text(view.show_label);
        }
        if old.is_none_or(|o| o.play_label != view.play_label || o.can_play != view.can_play) {
            items.play.set_text(view.play_label);
            items.play.set_enabled(view.can_play);
        }
        if clicked || old.is_none_or(|o| o.muted != view.muted) {
            items.mute.set_checked(view.muted);
        }
        if old
            .is_none_or(|o| o.record_label != view.record_label || o.can_record != view.can_record)
        {
            items.record.set_text(view.record_label);
            items.record.set_enabled(view.can_record);
        }
        if old.is_none_or(|o| o.sleeping != view.sleeping) {
            items.sleep_off.set_enabled(view.sleeping);
        }
        let same_list = old.is_some_and(|o| same_favorites(o, &view));
        if !same_list {
            for item in items.favorite_items.drain(..) {
                let _ = items.favorites.remove(&item);
            }
            if let Some(note) = items.favorites_note.take() {
                let _ = items.favorites.remove(&note);
            }
            for (id, name, _) in &view.favorites {
                let item = CheckMenuItem::with_id(
                    format!("{FAVORITE}{id}"),
                    menu_text(name),
                    true,
                    false,
                    None,
                );
                let _ = items.favorites.append(&item as &dyn IsMenuItem);
                items.favorite_items.push(item);
            }
            if let Some(text) = view.favorites_note {
                let note = MenuItem::new(text, false, None);
                let _ = items.favorites.append(&note);
                items.favorites_note = Some(note);
            }
        }
        // New items start unticked; the others keep the tick they show
        let shown_ticks = old.filter(|_| same_list && !clicked);
        for (index, (item, (_, _, ticked))) in
            items.favorite_items.iter().zip(&view.favorites).enumerate()
        {
            let shown = match shown_ticks {
                Some(o) => o.favorites.get(index).map(|f| f.2),
                None if same_list => None,
                None => Some(false),
            };
            if shown != Some(*ticked) {
                item.set_checked(*ticked);
            }
        }
        self.shown = Some(view);
    }
}

/// Whether two views list the same favorites, ticks aside: the menu's items
/// stay as they are
fn same_favorites(a: &View, b: &View) -> bool {
    a.favorites_note == b.favorites_note
        && a.favorites.len() == b.favorites.len()
        && a.favorites
            .iter()
            .zip(&b.favorites)
            .all(|(a, b)| a.0 == b.0 && a.1 == b.1)
}

/// The app's icon: the exe's own on Windows (every size, so the display
/// scale gets a sharp one), the 64 px picture elsewhere
fn app_icon() -> Result<tray_icon::Icon, String> {
    #[cfg(windows)]
    if let Ok(icon) = tray_icon::Icon::from_resource(1, None) {
        return Ok(icon);
    }
    let png = image::load_from_memory(include_bytes!("../../../assets/icons/icon-64.png"))
        .map_err(|e| e.to_string())?
        .into_rgba8();
    let (width, height) = png.dimensions();
    tray_icon::Icon::from_rgba(png.into_raw(), width, height).map_err(|e| e.to_string())
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    /// The last left click on the icon, to take a double click as one
    static LAST_CLICK: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static QUITTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Start the tray: the icon while View > Show Tray Icon is on, and the
/// clicks on it. Call once, on the UI thread. The window's poll keeps it
/// in step from then on (see [`refresh`]).
pub fn start(ui: &App) {
    let ui_weak = ui.as_weak();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let TrayIconEvent::Click {
            button,
            button_state: MouseButtonState::Up,
            ..
        } = event
        else {
            return;
        };
        let action = match button {
            MouseButton::Left => Action::ToggleWindow,
            MouseButton::Middle => Action::PlayStop,
            MouseButton::Right => return,
        };
        // On Windows this runs on the UI thread, on Linux on the tray's own
        let _ =
            ui_weak.upgrade_in_event_loop(move |ui| act(&ui, action, button == MouseButton::Left));
    }));
    let ui_weak = ui.as_weak();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let MenuId(id) = event.id;
        let Some(action) = Action::from_menu(&id) else {
            return;
        };
        let _ = ui_weak.upgrade_in_event_loop(move |ui| act(&ui, action, false));
    }));

    refresh(ui);
}

/// Bring the icon and its menu in step with the window. The window's poll
/// calls this on every round, so the tray costs nothing between them and
/// next to nothing with the icon off.
pub(crate) fn refresh(ui: &App) {
    refresh_after(ui, false);
}

/// [`refresh`]; `clicked` right after a menu item was picked
fn refresh_after(ui: &App, clicked: bool) {
    let wanted = ui.get_tray_icon();
    TRAY.with(|tray| {
        let mut tray = tray.borrow_mut();
        match (wanted, tray.is_some()) {
            (true, false) => match Tray::new() {
                Ok(new) => *tray = Some(new),
                Err(e) => {
                    eprintln!("Tray icon unavailable: {e}");
                    // Not again every tick: the setting goes off
                    ui.set_tray_icon(false);
                }
            },
            // Dropping it takes the icon away
            (false, true) => *tray = None,
            _ => {}
        }
        if let Some(tray) = tray.as_mut() {
            tray.show(View::of(&player(ui)), clicked);
        }
    });
    // A hidden window with no tray to bring it back would leave the radio
    // playing with no way to reach it
    let window = ui.window();
    if !window.is_visible() && !has_tray(ui) && !QUITTING.get() {
        show_window(ui);
    }
    // Minimized by the window manager or the taskbar (Windows and X11;
    // on Wayland only our own title bar button can tell)
    // (the flag as the poll keeps it: asking is a round trip to the X
    // server)
    if window.is_visible() && hides_on_minimize(ui) && crate::window_frame::minimized() {
        crate::window_frame::hide_minimized(window);
    }
    let defaults = ui.global::<Defaults>();
    crate::window_frame::keep_min_size(
        window,
        slint::LogicalSize::new(
            defaults.get_window_min_width(),
            defaults.get_window_min_height(),
        ),
    );
}

fn player(ui: &App) -> Player {
    let window = ui.window();
    let favorites = ui.get_favorites_list();
    let recording = ui.get_is_recording();
    Player {
        window_shown: window.is_visible() && !crate::window_frame::minimized(),
        station: ui.get_station_name().to_string(),
        station_url: ui.get_station_url().to_string(),
        song: ui.get_now_playing_title().to_string(),
        phase: crate::player_ui::shown_phase(),
        muted: ui.get_is_muted(),
        recording,
        recording_secs: if recording {
            clock_secs(&ui.get_recording_text())
        } else {
            0
        },
        can_record: ui.get_can_record(),
        sleep_until: if ui.get_sleep_active() {
            ui.get_sleep_until().to_string()
        } else {
            String::new()
        },
        // Only the ones the menu lists: the whole list can be a thousand
        favorites: favorites
            .iter()
            .take(TRAY_FAVORITES)
            .map(|f| (f.id.to_string(), f.name.to_string(), f.url.to_string()))
            .collect(),
        more_favorites: favorites.row_count() > TRAY_FAVORITES,
    }
}

fn act(ui: &App, action: Action, click: bool) {
    // What this changes shows at the next round of the poll: soon
    crate::player_ui::wake_poll();
    match action {
        Action::ToggleWindow => {
            // A double click (a Windows habit) is one toggle, not two
            if click {
                let guard = Duration::from_millis(
                    ui.global::<Defaults>().get_repeat_click_guard().max(0) as u64,
                );
                let now = Instant::now();
                let repeat = LAST_CLICK
                    .get()
                    .is_some_and(|last| now.duration_since(last) < guard);
                LAST_CLICK.set(Some(now));
                if repeat {
                    return;
                }
            }
            // A shown window that others cover comes to the front; only one
            // already in front hides
            let window = ui.window();
            if !window.is_visible() || crate::window_frame::sample_minimized(window) {
                show_window(ui);
            } else if !click || crate::window_frame::was_in_front(window, TRAY_FRONT_GRACE) {
                let _ = ui.hide();
            } else {
                crate::window_frame::raise(window);
            }
        }
        Action::PlayStop => ui.invoke_play_or_stop(),
        Action::Mute => ui.invoke_mute_clicked(),
        Action::Record => {
            if ui.get_is_recording() || ui.get_can_record() {
                ui.invoke_toggle_recording();
            }
        }
        Action::Favorite(id) => {
            let favorites = ui.get_favorites_list();
            if let Some(station) = favorites.iter().find(|f| f.id == id.as_str()) {
                ui.invoke_play_favorite(station);
            }
        }
        Action::Sleep(0) => ui.invoke_sleep_off(),
        Action::Sleep(minutes) => ui.invoke_sleep_start(minutes as i32),
        // Asks first when a recording or a Scheduler entry would stop
        Action::Quit => ui.invoke_quit_app(),
    }
    // Ticks set by the click itself are put right at once
    refresh_after(ui, true);
}

/// Show the window and bring it forward (the tray, a second launch, an
/// agent)
pub fn show_window(ui: &App) {
    crate::bring_to_front(ui.window());
}

/// Whether the tray icon is on and the desktop shows it
pub fn has_tray(ui: &App) -> bool {
    ui.get_tray_icon() && ui.get_tray_available()
}

/// Whether closing the window should only hide it
pub fn hides_on_close(ui: &App) -> bool {
    has_tray(ui) && ui.get_close_to_tray() && !QUITTING.get()
}

/// Whether minimizing the window should hide it to the tray
pub fn hides_on_minimize(ui: &App) -> bool {
    has_tray(ui) && ui.get_hide_on_minimize() && !QUITTING.get()
}

/// End the app (Quit in the tray and the Open menu, Ctrl+Q, or a close
/// that doesn't hide)
pub fn quit() {
    QUITTING.set(true);
    let _ = slint::quit_event_loop();
}

/// Whether the desktop shows tray icons, and watching for that to change
pub mod host {
    /// Whether the desktop shows tray icons now. Windows always does.
    pub fn available() -> bool {
        imp::available()
    }

    /// Call `changed` (on a thread of its own) whenever [`available`]
    /// changes. A no-op where the tray is always there.
    pub fn watch(changed: impl Fn(bool) + Send + 'static) {
        imp::watch(changed)
    }

    #[cfg(target_os = "linux")]
    mod imp {
        use radiotrope_app::config::ui::TRAY_HOST_CHECK;
        use zbus::blocking::{fdo::DBusProxy, Connection, Proxy};
        use zbus::proxy::CacheProperties;

        const WATCHER: &str = "org.kde.StatusNotifierWatcher";

        /// A tray host has taken the watcher's name and says a panel shows
        /// the icons (KDE, XFCE and others; GNOME only with the extension)
        fn check(conn: &Connection) -> bool {
            let has_watcher = DBusProxy::new(conn).is_ok_and(|dbus| {
                WATCHER
                    .try_into()
                    .is_ok_and(|name| dbus.name_has_owner(name).unwrap_or(false))
            });
            if !has_watcher {
                return false;
            }
            let host = zbus::blocking::proxy::Builder::<Proxy>::new(conn)
                .destination(WATCHER)
                .and_then(|b| b.path("/StatusNotifierWatcher"))
                .and_then(|b| b.interface(WATCHER))
                .map(|b| b.cache_properties(CacheProperties::No))
                .and_then(|b| b.build());
            // A watcher that can't say is taken at its word
            host.and_then(|p| p.get_property::<bool>("IsStatusNotifierHostRegistered"))
                .unwrap_or(true)
        }

        pub fn available() -> bool {
            Connection::session().is_ok_and(|conn| check(&conn))
        }

        /// The signals after which the answer of [`check`] may differ: the
        /// watcher's name got or lost its owner (the panel or GNOME's
        /// extension started or went away), or a host joined or left it
        const SIGNALS: [&str; 2] = [
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',\
             member='NameOwnerChanged',path='/org/freedesktop/DBus',\
             arg0='org.kde.StatusNotifierWatcher'",
            "type='signal',interface='org.kde.StatusNotifierWatcher',\
             path='/StatusNotifierWatcher'",
        ];

        pub fn watch(changed: impl Fn(bool) + Send + 'static) {
            let spawned = std::thread::Builder::new()
                .name("tray-host".into())
                .spawn(move || {
                    let Ok(conn) = Connection::session() else {
                        changed(false);
                        return;
                    };
                    // Asked for before the first check, so no change
                    // between the two is missed
                    let signals = listen(&conn);
                    let mut last = check(&conn);
                    changed(last);
                    let Ok(signals) = signals else {
                        // No signals: ask now and then, as before them
                        loop {
                            std::thread::sleep(TRAY_HOST_CHECK);
                            tell(&conn, &mut last, &changed);
                        }
                    };
                    // Sleeps until the bus has something for us
                    for message in signals {
                        let is_signal =
                            message.is_ok_and(|m| m.message_type() == zbus::message::Type::Signal);
                        if is_signal {
                            tell(&conn, &mut last, &changed);
                        }
                    }
                });
            if let Err(e) = spawned {
                eprintln!("Failed to start the tray check: {e}");
            }
        }

        /// Check again, and call `changed` if the answer differs
        fn tell(conn: &Connection, last: &mut bool, changed: &impl Fn(bool)) {
            let now = check(conn);
            if now != *last {
                *last = now;
                changed(now);
            }
        }

        /// Ask the bus for [`SIGNALS`]; the messages that arrive from then
        /// on
        fn listen(conn: &Connection) -> zbus::Result<zbus::blocking::MessageIterator> {
            let messages = zbus::blocking::MessageIterator::from(conn);
            let dbus = DBusProxy::new(conn)?;
            for rule in SIGNALS {
                dbus.add_match_rule(rule.try_into()?)?;
            }
            Ok(messages)
        }
    }

    #[cfg(not(target_os = "linux"))]
    mod imp {
        pub fn available() -> bool {
            true
        }

        pub fn watch(_changed: impl Fn(bool) + Send + 'static) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing() -> Player {
        Player {
            window_shown: true,
            station: "Fly 104".into(),
            station_url: "http://fly/104".into(),
            song: "Dua Lipa - Houdini".into(),
            phase: PlayerPhase::Playing,
            can_record: true,
            favorites: vec![
                ("a".into(), "Fly 104".into(), "http://fly/104".into()),
                ("b".into(), "R&B Hits".into(), "http://rnb".into()),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn the_sleep_timer_shows_in_the_tooltip_and_can_be_turned_off() {
        let off = View::of(&playing());
        assert!(!off.sleeping);
        let p = Player {
            sleep_until: "23:30".into(),
            ..playing()
        };
        let v = View::of(&p);
        assert!(v.sleeping);
        assert!(
            v.tooltip.ends_with("\nSleep Timer stops at 23:30"),
            "{}",
            v.tooltip
        );
        assert_eq!(Action::from_menu("sleep:30"), Some(Action::Sleep(30)));
        assert_eq!(Action::from_menu("sleep:0"), Some(Action::Sleep(0)));
        assert_eq!(Action::from_menu("sleep:x"), None);
    }

    #[test]
    fn playing_shows_station_only_and_stop() {
        let v = View::of(&playing());
        assert_eq!(v.header, "Fly 104");
        assert_eq!(v.tooltip, "Radiotrope\nFly 104\nDua Lipa - Houdini");
        assert_eq!(v.play_label, "Stop");
        assert!(v.can_play);
        assert_eq!(v.show_label, "Hide Radiotrope");
        assert_eq!(v.record_label, "Start Recording");
        assert!(v.can_record);
        assert_eq!(v.favorites[0], ("a".into(), "Fly 104".into(), true));
        assert!(!v.favorites[1].2);
    }

    #[test]
    fn idle_greys_play_and_record_without_a_station() {
        let v = View::of(&Player::default());
        assert_eq!(v.header, "Not playing");
        assert_eq!(v.tooltip, "Radiotrope");
        assert_eq!(v.play_label, "Play");
        assert!(!v.can_play);
        assert!(!v.can_record);
        assert_eq!(v.show_label, "Show Radiotrope");
    }

    #[test]
    fn stopped_station_can_play_and_ticks_no_favorite() {
        let p = Player {
            phase: PlayerPhase::Stopped,
            can_record: false,
            ..playing()
        };
        let v = View::of(&p);
        assert!(v.can_play);
        assert_eq!(v.play_label, "Play");
        assert!(v.favorites.iter().all(|f| !f.2));
    }

    #[test]
    fn starting_station_says_so() {
        for phase in [PlayerPhase::Connecting, PlayerPhase::Buffering] {
            let p = Player {
                phase,
                song: String::new(),
                ..playing()
            };
            assert_eq!(View::of(&p).header, "Fly 104 · Connecting…");
            assert_eq!(View::of(&p).play_label, "Stop");
        }
        let p = Player {
            phase: PlayerPhase::Reconnecting,
            ..playing()
        };
        assert_eq!(View::of(&p).header, "Fly 104 · Reconnecting…");
        assert_eq!(View::of(&p).play_label, "Stop");
    }

    #[test]
    fn a_failed_station_is_named_with_what_went_wrong() {
        let p = Player {
            phase: PlayerPhase::Failed,
            song: String::new(),
            can_record: false,
            ..playing()
        };
        let v = View::of(&p);
        assert_eq!(v.header, "Fly 104 · Can't connect");
        assert_eq!(v.tooltip, "Radiotrope\nFly 104 · Can't connect");
        // Play tries it again
        assert_eq!(v.play_label, "Play");
        assert!(v.can_play);
        assert!(v.favorites.iter().all(|f| !f.2));
    }

    #[test]
    fn a_recording_shows_in_the_tooltip_to_the_minute() {
        let p = Player {
            recording: true,
            recording_secs: clock_secs("12:34"),
            ..playing()
        };
        let v = View::of(&p);
        assert_eq!(
            v.tooltip,
            "Radiotrope\nFly 104\nDua Lipa - Houdini\nRecording · 12 min"
        );
        assert_eq!(v.record_label, "Stop Recording");
        // The same tooltip for the whole minute
        let later = Player {
            recording_secs: clock_secs("12:59"),
            ..p.clone()
        };
        assert_eq!(View::of(&later).tooltip, v.tooltip);
        let started = Player {
            recording_secs: 0,
            ..p
        };
        assert!(View::of(&started).tooltip.ends_with("Recording · <1 min"));
        assert_eq!(clock_secs("4:05"), 245);
        assert_eq!(clock_secs("1:02:03"), 3723);
        assert_eq!(clock_secs(""), 0);
        assert_eq!(clock_secs("\u{2014}"), 0);
    }

    #[test]
    fn long_text_is_cut_on_a_character() {
        let long = "Ελληνικό ".repeat(20);
        let short = cut(&long);
        assert_eq!(short.chars().count(), TRAY_TEXT_CHARS);
        assert!(short.ends_with('…'));
        assert_eq!(cut("  Fly 104 "), "Fly 104");
    }

    #[test]
    fn ampersands_show_as_written() {
        assert_eq!(menu_text("R&B Hits"), "R&&B Hits");
    }

    #[test]
    fn favorites_list_is_capped() {
        let p = Player {
            favorites: (0..40)
                .map(|i| (i.to_string(), format!("S{i}"), format!("u{i}")))
                .collect(),
            ..Default::default()
        };
        assert_eq!(View::of(&p).favorites.len(), TRAY_FAVORITES);
    }

    #[test]
    fn the_favorites_say_when_there_are_none_or_more() {
        assert_eq!(
            View::of(&Player::default()).favorites_note,
            Some("No favorites yet")
        );
        assert_eq!(View::of(&playing()).favorites_note, None);
        let more = Player {
            more_favorites: true,
            ..playing()
        };
        let v = View::of(&more);
        assert_eq!(v.favorites_note, Some("More in the window"));
        // The note coming or going rebuilds the list; a tick alone doesn't
        assert!(!same_favorites(&v, &View::of(&playing())));
        let stopped = Player {
            phase: PlayerPhase::Stopped,
            ..playing()
        };
        assert!(same_favorites(&View::of(&stopped), &View::of(&playing())));
    }

    #[test]
    fn menu_ids_map_to_actions() {
        assert_eq!(Action::from_menu(SHOW), Some(Action::ToggleWindow));
        assert_eq!(Action::from_menu(QUIT), Some(Action::Quit));
        assert_eq!(
            Action::from_menu("fav:abc"),
            Some(Action::Favorite("abc".into()))
        );
        assert_eq!(Action::from_menu("something"), None);
    }
}
