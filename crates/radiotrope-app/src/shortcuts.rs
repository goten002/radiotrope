//! Window-local desktop key bindings and their display labels.

use crate::{App, DesktopShortcuts, ShortcutAction, ShortcutRow};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::rc::Rc;

use ShortcutAction::*;

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
    group: &'static str,
    description: &'static str,
    label: &'static str,
    keys: Keys,
    control: bool,
}

impl Binding {
    const fn new(
        action: ShortcutAction,
        group: &'static str,
        description: &'static str,
        label: &'static str,
        keys: Keys,
    ) -> Self {
        Self {
            action,
            group,
            description,
            label,
            keys,
            control: false,
        }
    }

    const fn control(
        action: ShortcutAction,
        group: &'static str,
        description: &'static str,
        label: &'static str,
        key: &'static str,
    ) -> Self {
        Self {
            control: true,
            ..Self::new(action, group, description, label, Keys::Text(key))
        }
    }
}

const BINDINGS: &[Binding] = &[
    Binding::new(
        PlayStop,
        "Playback",
        "Play or stop",
        "Space",
        Keys::Text(" "),
    ),
    Binding::new(Mute, "Playback", "Mute or unmute", "M", Keys::Text("m")),
    Binding::new(VolumeUp, "Playback", "Volume up", "+ / =", Keys::Text("+=")),
    Binding::new(VolumeDown, "Playback", "Volume down", "-", Keys::Text("-")),
    Binding::new(
        Record,
        "Playback",
        "Start or stop recording",
        "R",
        Keys::Text("r"),
    ),
    Binding::new(
        NextFavorite,
        "Favorites",
        "Play next favorite",
        "N",
        Keys::Text("n"),
    ),
    Binding::new(
        PreviousFavorite,
        "Favorites",
        "Play previous favorite",
        "P",
        Keys::Text("p"),
    ),
    Binding::new(
        ToggleFavorite,
        "Favorites",
        "Add or remove loaded favorite",
        "F",
        Keys::Text("f"),
    ),
    Binding::control(Search, "Stations", "Search stations", "Ctrl+F", "f"),
    Binding::control(
        NetworkStream,
        "Stations",
        "Play a stream address",
        "Ctrl+O",
        "o",
    ),
    Binding::control(CloseWindow, "Window", "Close the window", "Ctrl+W", "w"),
    Binding::control(Quit, "Window", "Quit Radiotrope", "Ctrl+Q", "q"),
    Binding::new(
        Help,
        "Window",
        "Show keyboard shortcuts",
        "F1",
        Keys::Named(slint::platform::Key::F1),
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

fn match_key(text: &str, modifiers: Modifiers, repeat: bool) -> ShortcutAction {
    if modifiers.alt || modifiers.meta || modifiers.alt_graph {
        return Unhandled;
    }
    let mut chars = text.chars();
    let Some(key) = chars.next().filter(|_| chars.next().is_none()) else {
        return Unhandled;
    };
    let key = key.to_ascii_lowercase();
    BINDINGS
        .iter()
        .find(|binding| {
            binding.control == modifiers.control
                // On many layouts, Shift is needed to produce '+'.
                && (!modifiers.shift || key == '+')
                && (!repeat || matches!(binding.action, VolumeUp | VolumeDown))
                && binding.keys.matches(key)
        })
        .map_or(Unhandled, |binding| binding.action)
}

fn label(action: ShortcutAction) -> &'static str {
    BINDINGS
        .iter()
        .find(|binding| binding.action == action)
        .map_or("", |binding| binding.label)
}

pub fn install(ui: &App) {
    if !cfg!(all(feature = "desktop", not(feature = "embedded"))) {
        return;
    }

    let shortcuts = ui.global::<DesktopShortcuts>();
    shortcuts.on_match_key(|event| {
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
        )
    });
    shortcuts.on_label(|action| label(action).into());

    let mut rows = Vec::new();
    for group in ["Playback", "Favorites", "Stations", "Window", "Dialogs"] {
        let mut heading = group;
        let mut add = |description: &str, keys: &str| {
            rows.push(ShortcutRow {
                heading: std::mem::take(&mut heading).into(),
                description: description.into(),
                keys: keys.into(),
            });
        };
        for binding in BINDINGS.iter().filter(|binding| binding.group == group) {
            add(binding.description, binding.label);
        }
        match group {
            "Playback" => add(
                "Adjust focused volume slider or EQ fader",
                "Arrows / Page Up / Page Down / Home / End",
            ),
            "Favorites" => add("Open focused favorite’s actions", "Shift+F10 / Menu key"),
            "Dialogs" => {
                add("Move focus forward or backward", "Tab / Shift+Tab");
                add("Activate focused button or row", "Enter / Space");
                add("Close dialog or popup", "Esc");
                add(
                    "Scroll this dialog",
                    "Up / Down / Page Up / Page Down / Home / End",
                );
            }
            _ => {}
        }
    }

    shortcuts.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
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

#[cfg(test)]
mod tests {
    use super::*;

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
            (CloseWindow, "Ctrl+W"),
            (Quit, "Ctrl+Q"),
            (Help, "F1"),
        ] {
            assert_eq!(label(action), expected);
        }
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
