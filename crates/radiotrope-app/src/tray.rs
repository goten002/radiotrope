//! The system tray icon (desktop builds)
//!
//! Windows shows it in the notification area, Linux desktops through the
//! StatusNotifierItem D-Bus service (KDE, XFCE, Cinnamon, and GNOME with
//! the AppIndicator extension). The menu is the `tray-icon` crate's, kept
//! in step with the window by a timer: everything it does goes through the
//! window's own callbacks, so it acts like the window's controls do.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model};
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use radiotrope_app::config::ui::{TRAY_FAVORITES, TRAY_REFRESH, TRAY_TEXT_CHARS};

use crate::{App, Defaults};

/// Our name on the D-Bus tray, which KDE keeps the icon's settings under
const TRAY_ID: &str = "radiotrope";

const SHOW: &str = "show";
const PLAY: &str = "play";
const MUTE: &str = "mute";
const RECORD: &str = "record";
const QUIT: &str = "quit";
const FAVORITE: &str = "fav:";

/// What a click in the tray asks for
#[derive(Debug, Clone, PartialEq)]
enum Action {
    ToggleWindow,
    PlayStop,
    Mute,
    Record,
    Favorite(String),
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
            _ => Action::Favorite(id.strip_prefix(FAVORITE)?.to_string()),
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
    playing: bool,
    loading: bool,
    muted: bool,
    recording: bool,
    can_record: bool,
    /// (id, name, url), in the window's order
    favorites: Vec<(String, String, String)>,
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
    /// (id, label, ticked)
    favorites: Vec<(String, String, bool)>,
}

impl View {
    fn of(p: &Player) -> Self {
        let active = p.playing || p.loading;
        let station = cut(&p.station);
        let song = cut(&p.song);
        let header = match (active, song.is_empty()) {
            (false, _) => "Not playing".to_string(),
            (true, _) if p.loading => format!("{station} · Starting…"),
            (true, true) => station.clone(),
            (true, false) => format!("{station} · {song}"),
        };
        let tooltip = match (active, song.is_empty()) {
            (false, _) => "Radiotrope".to_string(),
            (true, true) => format!("Radiotrope\n{station}"),
            (true, false) => format!("Radiotrope\n{station}\n{song}"),
        };
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
    record: MenuItem,
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
                record,
            },
            shown: None,
        })
    }

    /// Bring the menu and tooltip up to `view`, changing only what differs
    fn show(&mut self, view: View) {
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
        // Set every time: a click ticks or unticks it on its own
        items.mute.set_checked(view.muted);
        if old
            .is_none_or(|o| o.record_label != view.record_label || o.can_record != view.can_record)
        {
            items.record.set_text(view.record_label);
            items.record.set_enabled(view.can_record);
        }
        let same_list = old.is_some_and(|o| {
            o.favorites.len() == view.favorites.len()
                && o.favorites
                    .iter()
                    .zip(&view.favorites)
                    .all(|(a, b)| a.0 == b.0 && a.1 == b.1)
        });
        if !same_list {
            for item in items.favorite_items.drain(..) {
                let _ = items.favorites.remove(&item);
            }
            if view.favorites.is_empty() {
                let none = CheckMenuItem::new("No favorites yet", false, false, None);
                let _ = items.favorites.append(&none);
                items.favorite_items.push(none);
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
        }
        for (item, (_, _, ticked)) in items.favorite_items.iter().zip(&view.favorites) {
            item.set_checked(*ticked);
        }
        self.shown = Some(view);
    }
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
    static TIMER: slint::Timer = slint::Timer::default();
    /// The last left click on the icon, to take a double click as one
    static LAST_CLICK: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static QUITTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Start the tray: the icon while View > Show Tray Icon is on, and the
/// clicks on it. Call once, on the UI thread.
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

    let ui_weak = ui.as_weak();
    TIMER.with(|timer| {
        timer.start(slint::TimerMode::Repeated, TRAY_REFRESH, move || {
            if let Some(ui) = ui_weak.upgrade() {
                refresh(&ui);
            }
        })
    });
    refresh(ui);
}

/// Bring the icon and its menu in step with the window
fn refresh(ui: &App) {
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
            tray.show(View::of(&player(ui)));
        }
    });
    // A hidden window with no tray to bring it back would leave the radio
    // playing with no way to reach it
    let window = ui.window();
    if !window.is_visible() && !has_tray(ui) && !QUITTING.get() {
        show_window(ui);
    }
}

fn player(ui: &App) -> Player {
    let window = ui.window();
    let favorites = ui.get_favorites_list();
    Player {
        window_shown: window.is_visible() && !window.is_minimized(),
        station: ui.get_station_name().to_string(),
        station_url: ui.get_station_url().to_string(),
        song: ui.get_now_playing_title().to_string(),
        playing: ui.get_is_playing(),
        loading: ui.get_is_loading(),
        muted: ui.get_is_muted(),
        recording: ui.get_is_recording(),
        can_record: ui.get_can_record(),
        favorites: favorites
            .iter()
            .map(|f| (f.id.to_string(), f.name.to_string(), f.url.to_string()))
            .collect(),
    }
}

fn act(ui: &App, action: Action, click: bool) {
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
            let window = ui.window();
            if window.is_visible() && !window.is_minimized() {
                let _ = ui.hide();
            } else {
                show_window(ui);
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
        Action::Quit => quit(),
    }
    // Ticks set by the click itself are put right at once
    refresh(ui);
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

        pub fn watch(changed: impl Fn(bool) + Send + 'static) {
            let spawned = std::thread::Builder::new()
                .name("tray-host".into())
                .spawn(move || {
                    let Ok(conn) = Connection::session() else {
                        changed(false);
                        return;
                    };
                    let mut last = None;
                    loop {
                        let now = check(&conn);
                        if last != Some(now) {
                            last = Some(now);
                            changed(now);
                        }
                        std::thread::sleep(TRAY_HOST_CHECK);
                    }
                });
            if let Err(e) = spawned {
                eprintln!("Failed to start the tray check: {e}");
            }
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
            playing: true,
            can_record: true,
            favorites: vec![
                ("a".into(), "Fly 104".into(), "http://fly/104".into()),
                ("b".into(), "R&B Hits".into(), "http://rnb".into()),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn playing_shows_station_song_and_stop() {
        let v = View::of(&playing());
        assert_eq!(v.header, "Fly 104 · Dua Lipa - Houdini");
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
            playing: false,
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
        let p = Player {
            playing: false,
            loading: true,
            song: String::new(),
            ..playing()
        };
        assert_eq!(View::of(&p).header, "Fly 104 · Starting…");
        assert_eq!(View::of(&p).play_label, "Stop");
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
