//! Window-local desktop key bindings, their menu labels and the rows of the
//! Keyboard Shortcuts dialog.

use crate::{App, DesktopShortcuts, KeyCombo, ShortcutAction, ShortcutGroup, ShortcutRow};
use slint::platform::Key;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::rc::Rc;

use ShortcutAction::*;

/// The Control key as the platform names it (Slint reports Command as
/// Control on macOS).
const CTRL: &str = if cfg!(target_os = "macos") {
    "⌘"
} else {
    "Ctrl"
};

/// The ways to press a shortcut, each a list of keycap labels pressed together.
type Caps = &'static [&'static [&'static str]];

enum Keys {
    Text(&'static str),
    Named(slint::platform::Key),
}

impl Keys {
    fn matches(&self, key: char) -> bool {
        match self {
            Self::Text(keys) => keys.contains(key),
            Self::Named(named) => char::from(*named) == key,
        }
    }
}

struct Binding {
    action: ShortcutAction,
    /// The menu hint, e.g. "Ctrl+F"
    label: &'static str,
    caps: Caps,
    keys: Keys,
    control: bool,
}

impl Binding {
    const fn new(action: ShortcutAction, label: &'static str, caps: Caps, keys: Keys) -> Self {
        Self {
            action,
            label,
            caps,
            keys,
            control: false,
        }
    }

    const fn control(
        action: ShortcutAction,
        label: &'static str,
        caps: Caps,
        key: &'static str,
    ) -> Self {
        Self {
            control: true,
            ..Self::new(action, label, caps, Keys::Text(key))
        }
    }
}

const BINDINGS: &[Binding] = &[
    Binding::new(PlayStop, "Space", &[&["Space"]], Keys::Text(" ")),
    Binding::new(Mute, "M", &[&["M"]], Keys::Text("m")),
    Binding::new(VolumeUp, "+ / =", &[&["+"]], Keys::Text("+=")),
    Binding::new(VolumeDown, "-", &[&["−"]], Keys::Text("-")),
    Binding::new(Record, "R", &[&["R"]], Keys::Text("r")),
    Binding::new(NextFavorite, "N", &[&["N"]], Keys::Text("n")),
    Binding::new(PreviousFavorite, "P", &[&["P"]], Keys::Text("p")),
    Binding::new(ToggleFavorite, "F", &[&["F"]], Keys::Text("f")),
    Binding::new(FindFavorites, "/", &[&["/"]], Keys::Text("/")),
    Binding::control(UndoRemove, "Ctrl+Z", &[&[CTRL, "Z"]], "z"),
    Binding::control(Search, "Ctrl+F", &[&[CTRL, "F"]], "f"),
    Binding::control(NetworkStream, "Ctrl+O", &[&[CTRL, "O"]], "o"),
    Binding::control(HideWindow, "Ctrl+W", &[&[CTRL, "W"]], "w"),
    Binding::control(Quit, "Ctrl+Q", &[&[CTRL, "Q"]], "q"),
    Binding::new(
        Help,
        "F1",
        &[&["F1"]],
        Keys::Named(slint::platform::Key::F1),
    ),
];

// Keys for the highlighted favorite: the key, its label and its keycaps
const FAVORITE_KEYS: &[(ShortcutAction, Key, &str, Caps)] = &[
    (FavoriteUp, Key::UpArrow, "↑ / ↓", &[&["↑"]]),
    (FavoriteDown, Key::DownArrow, "↑ / ↓", &[&["↓"]]),
    (FavoriteFirst, Key::Home, "Home / End", &[&["Home"]]),
    (FavoriteLast, Key::End, "Home / End", &[&["End"]]),
    (
        FavoritePageUp,
        Key::PageUp,
        "Page Up / Page Down",
        &[&["Page Up"]],
    ),
    (
        FavoritePageDown,
        Key::PageDown,
        "Page Up / Page Down",
        &[&["Page Down"]],
    ),
    (FavoritePlay, Key::Return, "Enter", &[&["Enter"]]),
    (
        FavoriteMenu,
        Key::Menu,
        "Shift+F10 / Menu key",
        &[&["Shift", "F10"], &["Menu"]],
    ),
    (FavoriteClear, Key::Escape, "Esc", &[&["Esc"]]),
];

// Alt with an arrow moves the highlighted favorite one place
const MOVE_KEYS: &[(ShortcutAction, Key, &str, Caps)] = &[
    (FavoriteMoveUp, Key::UpArrow, "Alt+↑", &[&["Alt", "↑"]]),
    (FavoriteMoveDown, Key::DownArrow, "Alt+↓", &[&["Alt", "↓"]]),
];

/// The keys of a dialog row: shortcuts from the tables above, or keys that
/// work wherever the focus is.
enum RowKeys {
    Actions(&'static [ShortcutAction]),
    Caps(Caps),
}

struct Row {
    description: &'static str,
    keys: RowKeys,
}

const fn row(description: &'static str, actions: &'static [ShortcutAction]) -> Row {
    Row {
        description,
        keys: RowKeys::Actions(actions),
    }
}

const fn keys(description: &'static str, caps: Caps) -> Row {
    Row {
        description,
        keys: RowKeys::Caps(caps),
    }
}

/// The Keyboard Shortcuts dialog, one card per group
const GROUPS: &[(&str, &[Row])] = &[
    (
        "Playback",
        &[
            row("Play or stop", &[PlayStop]),
            row("Mute or unmute", &[Mute]),
            row("Volume up / down", &[VolumeUp, VolumeDown]),
            row("Start or stop recording", &[Record]),
        ],
    ),
    (
        "Favorites",
        &[
            row(
                "Next / previous favorite",
                &[NextFavorite, PreviousFavorite],
            ),
            row("Add or remove the current station", &[ToggleFavorite]),
            row("Undo removing a favorite", &[UndoRemove]),
            row("Find in favorites", &[FindFavorites]),
            row("Move through favorites", &[FavoriteUp, FavoriteDown]),
            row("Play the highlighted favorite", &[FavoritePlay]),
            row(
                "Move the highlighted favorite",
                &[FavoriteMoveUp, FavoriteMoveDown],
            ),
        ],
    ),
    (
        "Stations",
        &[
            row("Search stations", &[Search]),
            row("Open network stream", &[NetworkStream]),
        ],
    ),
    (
        "Window",
        &[
            keys("Go to the menu bar", &[&["F10"]]),
            row("Hide the window", &[HideWindow]),
            row("Quit Radiotrope", &[Quit]),
            row("Show keyboard shortcuts", &[Help]),
        ],
    ),
    (
        "In lists and dialogs",
        &[
            keys("Move focus forward or back", &[&["Tab"], &["Shift", "Tab"]]),
            keys(
                "Activate the focused button or row",
                &[&["Enter"], &["Space"]],
            ),
            row("Open a favorite's actions", &[FavoriteMenu]),
            keys(
                "Adjust the volume or an EQ fader",
                &[&["←"], &["↓"], &["↑"], &["→"]],
            ),
            row("Move by a page", &[FavoritePageUp, FavoritePageDown]),
            row("Jump to the start or end", &[FavoriteFirst, FavoriteLast]),
            row("Close a dialog or clear the highlight", &[FavoriteClear]),
        ],
    ),
];

#[derive(Default)]
struct Modifiers {
    control: bool,
    alt: bool,
    meta: bool,
    shift: bool,
    alt_graph: bool,
}

fn match_key(text: &str, modifiers: Modifiers, repeat: bool, desktop: bool) -> ShortcutAction {
    if modifiers.meta || modifiers.alt_graph {
        return Unhandled;
    }
    let mut chars = text.chars();
    let Some(key) = chars.next().filter(|_| chars.next().is_none()) else {
        return Unhandled;
    };
    let key = key.to_ascii_lowercase();
    if modifiers.alt {
        if modifiers.control || modifiers.shift {
            return Unhandled;
        }
        return MOVE_KEYS
            .iter()
            .find(|(_, named, _, _)| char::from(*named) == key)
            .map_or(Unhandled, |&(action, _, _, _)| action);
    }
    if !modifiers.control {
        if modifiers.shift && key == char::from(Key::F10) && !repeat {
            return FavoriteMenu;
        }
        if !modifiers.shift {
            if let Some(&(action, _, _, _)) = FAVORITE_KEYS
                .iter()
                .find(|(_, named, _, _)| char::from(*named) == key)
            {
                if !repeat
                    || matches!(
                        action,
                        FavoriteUp
                            | FavoriteDown
                            | FavoriteFirst
                            | FavoriteLast
                            | FavoritePageUp
                            | FavoritePageDown
                    )
                {
                    return action;
                }
            }
        }
    }
    if !desktop {
        return Unhandled;
    }
    BINDINGS
        .iter()
        .find(|binding| {
            binding.control == modifiers.control
                // On many layouts, Shift is needed to produce '+' or '/'.
                && (!modifiers.shift || key == '+' || key == '/')
                && (!repeat || matches!(binding.action, VolumeUp | VolumeDown))
                && binding.keys.matches(key)
        })
        .map_or(Unhandled, |binding| binding.action)
}

fn label(action: ShortcutAction) -> &'static str {
    BINDINGS
        .iter()
        .find(|binding| binding.action == action)
        .map(|binding| binding.label)
        .or_else(|| {
            FAVORITE_KEYS
                .iter()
                .chain(MOVE_KEYS)
                .find(|(id, _, _, _)| *id == action)
                .map(|(_, _, label, _)| *label)
        })
        .unwrap_or("")
}

fn caps(action: ShortcutAction) -> Caps {
    BINDINGS
        .iter()
        .find(|binding| binding.action == action)
        .map(|binding| binding.caps)
        .or_else(|| {
            FAVORITE_KEYS
                .iter()
                .chain(MOVE_KEYS)
                .find(|(id, _, _, _)| *id == action)
                .map(|(_, _, _, caps)| *caps)
        })
        .unwrap_or(&[])
}

/// A keycap's name for a screen reader
fn spoken_key(cap: &str) -> &str {
    match cap {
        "Ctrl" => "Control",
        "⌘" => "Command",
        "Esc" => "Escape",
        "↑" => "Up arrow",
        "↓" => "Down arrow",
        "←" => "Left arrow",
        "→" => "Right arrow",
        "+" => "Plus",
        "−" => "Minus",
        "/" => "Slash",
        _ => cap,
    }
}

struct DialogRow {
    description: &'static str,
    alternatives: Vec<&'static [&'static str]>,
    separator: &'static str,
    spoken: String,
}

impl DialogRow {
    fn new(row: &Row) -> Self {
        let alternatives: Vec<_> = match row.keys {
            RowKeys::Actions(actions) => actions.iter().flat_map(|&a| caps(a)).copied().collect(),
            RowKeys::Caps(caps) => caps.to_vec(),
        };
        // Combinations side by side would read as one: put "or" between them
        let combined = alternatives.iter().any(|combo| combo.len() > 1);
        let separator = if combined { "or" } else { "" };
        let spoken = alternatives
            .iter()
            .map(|combo| {
                combo
                    .iter()
                    .map(|cap| spoken_key(cap))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(if combined { " or " } else { ", " });
        Self {
            description: row.description,
            alternatives,
            separator,
            spoken: format!("{}: {spoken}", row.description),
        }
    }
}

fn dialog_groups() -> impl Iterator<Item = (&'static str, Vec<DialogRow>)> {
    GROUPS
        .iter()
        .map(|&(title, rows)| (title, rows.iter().map(DialogRow::new).collect()))
}

fn model<T: Clone + 'static>(items: impl IntoIterator<Item = T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from_iter(items)))
}

pub fn install(ui: &App) {
    let desktop = cfg!(all(feature = "desktop", not(feature = "embedded")));
    let shortcuts = ui.global::<DesktopShortcuts>();
    shortcuts.on_match_key(move |event| {
        match_key(
            event.text.as_str(),
            Modifiers {
                control: event.modifiers.control,
                alt: event.modifiers.alt,
                meta: event.modifiers.meta,
                shift: event.modifiers.shift,
                #[cfg(feature = "desktop")]
                alt_graph: crate::window_frame::alt_graph_held(),
                #[cfg(not(feature = "desktop"))]
                alt_graph: false,
            },
            event.repeat,
            desktop,
        )
    });
    if !desktop {
        return;
    }
    shortcuts.on_label(|action| label(action).into());

    shortcuts.set_groups(model(dialog_groups().map(|(title, rows)| ShortcutGroup {
        title: title.into(),
        rows: model(rows.into_iter().map(|row| ShortcutRow {
            description: row.description.into(),
            alternatives: model(row.alternatives.into_iter().map(|combo| KeyCombo {
                keys: model(combo.iter().map(|&cap| SharedString::from(cap))),
            })),
            separator: row.separator.into(),
            spoken: row.spoken.into(),
        })),
    })));
    shortcuts.set_enabled(true);
}

/// Step in display order, using the first favorite if there is no current match.
pub fn favorite_step(
    mut urls: impl ExactSizeIterator<Item = impl AsRef<str>>,
    playing_url: Option<&str>,
    forward: bool,
) -> Option<usize> {
    let count = urls.len();
    if count == 0 {
        return None;
    }
    let current = playing_url.and_then(|url| urls.position(|favorite| favorite.as_ref() == url));
    Some(match current {
        Some(index) if forward => (index + 1) % count,
        Some(index) if index > 0 => index - 1,
        Some(_) => count - 1,
        None => 0,
    })
}

/// Resolve the highlight by URL so a new model can reorder or remove rows.
pub fn favorite_highlight(
    mut urls: impl ExactSizeIterator<Item = impl AsRef<str>>,
    highlighted_url: &str,
    playing_url: &str,
    action: ShortcutAction,
    page: usize,
) -> Option<usize> {
    let last = urls.len().checked_sub(1)?;
    if action == FavoriteFirst {
        return Some(0);
    }
    if action == FavoriteLast {
        return Some(last);
    }

    let mut playing = None;
    let highlighted = urls.by_ref().enumerate().find_map(|(index, url)| {
        let url = url.as_ref();
        if !playing_url.is_empty() && url == playing_url {
            playing = Some(index);
        }
        (!highlighted_url.is_empty() && url == highlighted_url).then_some(index)
    });
    if action == Unhandled {
        return highlighted;
    }
    // If the highlighted station disappeared, begin at the playing row (or row zero).
    let Some(index) = highlighted else {
        return Some(playing.unwrap_or(0));
    };
    Some(match action {
        FavoriteUp => index.saturating_sub(1),
        FavoriteDown => (index + 1).min(last),
        FavoritePageUp => index.saturating_sub(page.max(1)),
        FavoritePageDown => index.saturating_add(page.max(1)).min(last),
        _ => index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlight(
        urls: &[&str],
        selected: &str,
        playing: &str,
        action: ShortcutAction,
        page: usize,
    ) -> Option<usize> {
        favorite_highlight(urls.iter().copied(), selected, playing, action, page)
    }

    #[test]
    fn favorite_highlight_clamps_at_list_edges() {
        let urls = ["a", "b", "c", "d"];
        assert_eq!(highlight(&[], "", "", FavoriteDown, 2), None);
        assert_eq!(highlight(&["a"], "a", "", FavoriteDown, 2), Some(0));
        assert_eq!(highlight(&urls, "a", "", FavoriteUp, 2), Some(0));
        assert_eq!(highlight(&urls, "d", "", FavoriteDown, 2), Some(3));
        assert_eq!(highlight(&urls, "c", "", FavoriteFirst, 2), Some(0));
        assert_eq!(highlight(&urls, "a", "", FavoriteLast, 2), Some(3));
    }

    #[test]
    fn favorite_highlight_moves_by_a_page() {
        let urls = ["a", "b", "c", "d", "e"];
        assert_eq!(highlight(&urls, "b", "", FavoritePageDown, 2), Some(3));
        assert_eq!(highlight(&urls, "d", "", FavoritePageUp, 2), Some(1));
        assert_eq!(highlight(&urls, "d", "", FavoritePageDown, 2), Some(4));
        assert_eq!(highlight(&urls, "b", "", FavoritePageUp, 2), Some(0));
    }

    #[test]
    fn favorite_highlight_starts_at_playing_row_or_first() {
        let urls = ["a", "b", "c"];
        for action in [FavoriteUp, FavoriteDown] {
            assert_eq!(highlight(&urls, "", "b", action, 2), Some(1));
            assert_eq!(highlight(&urls, "", "other", action, 2), Some(0));
            assert_eq!(highlight(&urls, "", "", action, 2), Some(0));
        }
    }

    #[test]
    fn favorite_highlight_follows_its_url_across_model_changes() {
        for (urls, expected) in [
            (&["a", "b", "c"][..], Some(1)),
            (&["b", "a", "c"][..], Some(0)),
            (&["new", "b", "a", "c"][..], Some(1)),
            (&["b", "a"][..], Some(0)),
            (&["a", "c"][..], None),
        ] {
            assert_eq!(highlight(urls, "b", "", Unhandled, 0), expected);
        }
        assert_eq!(highlight(&["a", "c"], "b", "c", FavoriteDown, 2), Some(1));
    }

    #[test]
    fn kiosk_keeps_favorite_navigation() {
        for &(action, key, _, _) in FAVORITE_KEYS {
            assert_eq!(
                super::match_key(
                    &char::from(key).to_string(),
                    Modifiers::default(),
                    false,
                    false
                ),
                action
            );
        }
        assert_eq!(
            super::match_key("m", Modifiers::default(), false, false),
            Unhandled
        );
    }

    fn match_key(text: &str, modifiers: Modifiers, repeat: bool) -> ShortcutAction {
        super::match_key(text, modifiers, repeat, true)
    }

    #[test]
    fn exact_modifiers() {
        assert_eq!(match_key("f", Modifiers::default(), false), ToggleFavorite);
        assert_eq!(
            match_key(
                "f",
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                false
            ),
            Search
        );
        assert_eq!(
            match_key(
                "f",
                Modifiers {
                    control: true,
                    shift: true,
                    ..Modifiers::default()
                },
                false
            ),
            Unhandled
        );
        for mods in [
            Modifiers {
                alt: true,
                ..Modifiers::default()
            },
            Modifiers {
                meta: true,
                ..Modifiers::default()
            },
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            Modifiers {
                control: true,
                alt: true,
                ..Modifiers::default()
            },
            Modifiers {
                control: true,
                meta: true,
                ..Modifiers::default()
            },
            Modifiers {
                alt_graph: true,
                ..Modifiers::default()
            },
        ] {
            assert_eq!(match_key("f", mods, false), Unhandled);
        }
        assert_eq!(
            match_key(
                "+",
                Modifiers {
                    alt_graph: true,
                    ..Modifiers::default()
                },
                false
            ),
            Unhandled
        );
    }

    fn keys(binding: &Binding) -> String {
        match binding.keys {
            Keys::Text(keys) => keys.into(),
            Keys::Named(key) => char::from(key).into(),
        }
    }

    #[test]
    fn punctuation_and_repeat() {
        // Winit supplies the same logical text for keypad + and -.
        for (key, action) in [("+", VolumeUp), ("=", VolumeUp), ("-", VolumeDown)] {
            assert_eq!(match_key(key, Modifiers::default(), true), action);
        }
        let shifted = || Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(match_key("+", shifted(), false), VolumeUp);
        assert_eq!(match_key("=", shifted(), false), Unhandled);
        assert_eq!(match_key("-", shifted(), false), Unhandled);
        for binding in BINDINGS
            .iter()
            .filter(|b| !matches!(b.action, VolumeUp | VolumeDown))
        {
            let modifiers = Modifiers {
                control: binding.control,
                ..Modifiers::default()
            };
            assert_eq!(match_key(&keys(binding), modifiers, true), Unhandled);
        }
    }

    #[test]
    fn matches_upper_and_lowercase() {
        for binding in BINDINGS {
            for key in keys(binding).chars() {
                for spelling in [key, key.to_ascii_uppercase()] {
                    let modifiers = Modifiers {
                        control: binding.control,
                        ..Modifiers::default()
                    };
                    assert_eq!(
                        match_key(&spelling.to_string(), modifiers, false),
                        binding.action
                    );
                }
            }
        }
        assert_eq!(match_key("ff", Modifiers::default(), false), Unhandled);
    }

    #[test]
    fn labels() {
        for (action, expected) in [
            (PlayStop, "Space"),
            (Mute, "M"),
            (VolumeUp, "+ / ="),
            (VolumeDown, "-"),
            (Record, "R"),
            (NextFavorite, "N"),
            (PreviousFavorite, "P"),
            (ToggleFavorite, "F"),
            (Search, "Ctrl+F"),
            (NetworkStream, "Ctrl+O"),
            (HideWindow, "Ctrl+W"),
            (Quit, "Ctrl+Q"),
            (Help, "F1"),
            (FindFavorites, "/"),
            (UndoRemove, "Ctrl+Z"),
            (FavoriteMoveUp, "Alt+↑"),
            (FavoriteMoveDown, "Alt+↓"),
        ] {
            assert_eq!(label(action), expected);
        }
    }

    fn actions() -> impl Iterator<Item = ShortcutAction> {
        BINDINGS
            .iter()
            .map(|binding| binding.action)
            .chain(FAVORITE_KEYS.iter().map(|&(action, _, _, _)| action))
            .chain(MOVE_KEYS.iter().map(|&(action, _, _, _)| action))
    }

    #[test]
    fn every_action_has_keycaps() {
        for action in actions() {
            let caps = caps(action);
            assert!(!caps.is_empty(), "{action:?} has no keycaps");
            for combo in caps {
                assert!(!combo.is_empty(), "{action:?} has an empty combination");
                assert!(combo.iter().all(|cap| !cap.is_empty()));
            }
        }
    }

    #[test]
    fn dialog_shows_every_action_once() {
        for action in actions() {
            let shown = GROUPS
                .iter()
                .flat_map(|(_, rows)| rows.iter())
                .filter(|row| matches!(row.keys, RowKeys::Actions(actions) if actions.contains(&action)))
                .count();
            assert_eq!(shown, 1, "{action:?} is shown {shown} times");
        }
        for (title, rows) in dialog_groups() {
            assert!(!rows.is_empty(), "{title} has no rows");
            for row in rows {
                assert!(
                    !row.alternatives.is_empty(),
                    "{} has no keys",
                    row.description
                );
            }
        }
    }

    fn dialog_row(description: &str) -> DialogRow {
        dialog_groups()
            .flat_map(|(_, rows)| rows)
            .find(|row| row.description == description)
            .unwrap()
    }

    #[test]
    fn dialog_rows() {
        let row = dialog_row("Volume up / down");
        assert_eq!(row.alternatives, [&["+"][..], &["−"][..]]);
        assert_eq!(row.separator, "");
        assert_eq!(row.spoken, "Volume up / down: Plus, Minus");

        let row = dialog_row("Open a favorite's actions");
        assert_eq!(row.alternatives, [&["Shift", "F10"][..], &["Menu"][..]]);
        assert_eq!(row.separator, "or");
        assert_eq!(row.spoken, "Open a favorite's actions: Shift F10 or Menu");

        assert_eq!(
            dialog_row("Move through favorites").spoken,
            "Move through favorites: Up arrow, Down arrow"
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            dialog_row("Search stations").spoken,
            "Search stations: Control F"
        );
    }

    #[test]
    fn find_undo_and_move_rows() {
        let row = dialog_row("Find in favorites");
        assert_eq!(row.alternatives, [&["/"][..]]);
        assert_eq!(row.spoken, "Find in favorites: Slash");

        let row = dialog_row("Move the highlighted favorite");
        assert_eq!(row.alternatives, [&["Alt", "↑"][..], &["Alt", "↓"][..]]);
        assert_eq!(row.separator, "or");
        assert_eq!(
            row.spoken,
            "Move the highlighted favorite: Alt Up arrow or Alt Down arrow"
        );

        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            dialog_row("Undo removing a favorite").spoken,
            "Undo removing a favorite: Control Z"
        );
        assert_eq!(
            dialog_row("Go to the menu bar").spoken,
            "Go to the menu bar: F10"
        );
    }

    #[test]
    fn alt_arrows_move_the_highlighted_favorite() {
        let alt = || Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        let up = char::from(Key::UpArrow).to_string();
        let down = char::from(Key::DownArrow).to_string();
        // Held down, it keeps moving; and the kiosk has it too
        for repeat in [false, true] {
            assert_eq!(match_key(&up, alt(), repeat), FavoriteMoveUp);
            assert_eq!(match_key(&down, alt(), repeat), FavoriteMoveDown);
        }
        assert_eq!(super::match_key(&up, alt(), false, false), FavoriteMoveUp);
        // Plain arrows still move the highlight, and nothing else takes Alt
        assert_eq!(match_key(&up, Modifiers::default(), false), FavoriteUp);
        assert_eq!(
            match_key(
                &up,
                Modifiers {
                    control: true,
                    ..alt()
                },
                false
            ),
            Unhandled
        );
        assert_eq!(
            match_key(&char::from(Key::Home).to_string(), alt(), false),
            Unhandled
        );
    }

    #[test]
    fn slash_finds_with_or_without_shift() {
        assert_eq!(match_key("/", Modifiers::default(), false), FindFavorites);
        // Layouts that type "/" with Shift (Shift+7)
        assert_eq!(
            match_key(
                "/",
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                false
            ),
            FindFavorites
        );
        // Not on the kiosk, which has no letter shortcuts
        assert_eq!(
            super::match_key("/", Modifiers::default(), false, false),
            Unhandled
        );
    }

    #[test]
    fn favorite_stepping() {
        let urls = ["one", "two", "three"];
        for forward in [false, true] {
            assert_eq!(
                favorite_step(std::iter::empty::<&str>(), Some("one"), forward),
                None
            );
            assert_eq!(
                favorite_step(["one"].into_iter(), Some("one"), forward),
                Some(0)
            );
            assert_eq!(favorite_step(urls.into_iter(), None, forward), Some(0));
            assert_eq!(
                favorite_step(urls.into_iter(), Some("other"), forward),
                Some(0)
            );
        }
        assert_eq!(favorite_step(urls.into_iter(), Some("two"), true), Some(2));
        assert_eq!(favorite_step(urls.into_iter(), Some("two"), false), Some(0));
        assert_eq!(
            favorite_step(urls.into_iter(), Some("three"), true),
            Some(0)
        );
        assert_eq!(favorite_step(urls.into_iter(), Some("one"), false), Some(2));
    }
}
