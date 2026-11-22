use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use radiotrope_library::data::favorites::FavoriteEdit;

use crate::favorites;
use crate::library::Station;
use crate::search::Request;
use crate::ui::View;

const PAGE: isize = 10;

pub enum Effect {
    None,
    Quit,
    Search(Request),
    CloseSearch,
    Edit(FavoriteEdit),
    Play,
    TogglePlayback,
    StepFavorite(isize),
    Volume(isize),
    Mute,
    Record,
}

pub struct Confirmation {
    pub id: String,
    pub name: String,
}

/// A key (or keys) and what it does: one table feeds the help screen, the
/// help line under the box, and the check against `--help`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// As the help screen shows it
    pub keys: &'static str,
    /// What it does, for the help screen
    pub what: &'static str,
    /// As the help line shows it: shorter, with its one word
    pub hint_keys: &'static str,
    pub hint: &'static str,
}

const fn key(keys: &'static str, what: &'static str, hint: (&'static str, &'static str)) -> Key {
    Key {
        keys,
        what,
        hint_keys: hint.0,
        hint: hint.1,
    }
}

pub const PLAY: Key = key("Enter", "Play the highlighted station", ("Enter", "play"));
pub const PLAY_STOP: Key = key("Space", "Play or stop", ("Space", "play"));
pub const NEXT: Key = key("n  p", "Next or previous favorite", ("n/p", "next"));
pub const VOLUME: Key = key("+  -", "Volume up or down", ("+-", "volume"));
pub const MUTE: Key = key("m", "Mute or unmute", ("m", "mute"));
pub const RECORD: Key = key("r", "Start or stop recording", ("r", "record"));
pub const MOVE: Key = key(
    "\u{2191} \u{2193}  j k",
    "Move the highlight",
    ("\u{2191}\u{2193}", "move"),
);
pub const PAGE_KEYS: Key = key(
    "PgUp PgDn Home End",
    "Move by a page, to the first or last",
    ("PgDn", "more"),
);
pub const FAVORITE: Key = key(
    "f",
    "Add the playing station, or remove the highlighted one",
    ("f", "add"),
);
pub const CONFIRM: Key = key(
    "y  Enter",
    "When asked, remove the favorite",
    ("Enter/y", "remove"),
);
pub const CANCEL: Key = key("n  Esc", "When asked, keep it", ("Esc/n", "cancel"));
pub const SEARCH: Key = key("/  Ctrl+F", "Search the station directory", ("/", "search"));
pub const SEARCH_GO: Key = key(
    "Enter",
    "Search (with no text: the most-played stations)",
    ("Enter", "search"),
);
pub const TO_RESULTS: Key = key(
    "\u{2193}  Tab",
    "From the query to the results, and Tab or \u{2191} back",
    ("Tab", "results"),
);
pub const RESULT_FAVORITE: Key = key("f", "Add or remove the highlighted result", ("f", "add"));
pub const LEAVE_SEARCH: Key = key("Esc", "Leave search (Esc never quits)", ("Esc", "back"));
pub const HELP: Key = key("?  F1", "Show all the keys", ("?", "help"));
pub const QUIT: Key = key("q  Ctrl+C  Ctrl+Q", "Quit", ("q", "quit"));

/// Every key, grouped as the help screen shows them
pub const KEY_GROUPS: &[(&str, &[Key])] = &[
    ("Playback", &[PLAY, PLAY_STOP, NEXT, VOLUME, MUTE, RECORD]),
    ("Favorites", &[MOVE, PAGE_KEYS, FAVORITE, CONFIRM, CANCEL]),
    (
        "Search",
        &[SEARCH, SEARCH_GO, TO_RESULTS, RESULT_FAVORITE, LEAVE_SEARCH],
    ),
    ("General", &[HELP, QUIT]),
];

/// The help line for what the screen shows now, most important first:
/// `(keys, word)` pairs. The line ends in `? help`, which [`help_line_end`]
/// gives, so a narrow window cuts from the middle.
pub fn help_line(
    view: &View,
    playing: Option<&Station>,
    active: bool,
) -> Vec<(&'static str, &'static str)> {
    let hint = |k: Key| (k.hint_keys, k.hint);
    if view.confirmation.is_some() {
        return vec![hint(CONFIRM), hint(CANCEL)];
    }
    let search = &view.search;
    if search.open {
        if search.editing {
            let mut line = vec![hint(SEARCH_GO)];
            if !search.rows.is_empty() || search.error.is_some() {
                line.push(hint(TO_RESULTS));
            }
            line.push(hint(LEAVE_SEARCH));
            return line;
        }
        if search.error.is_some() {
            return vec![("Enter", "retry"), ("Tab", "query"), hint(LEAVE_SEARCH)];
        }
        let mut line = vec![hint(PLAY)];
        if let Some(station) = search.selected() {
            let saved = favorites::find_match(&view.favorites, station).is_some();
            line.push(("f", if saved { "remove" } else { "add" }));
        }
        line.extend([("Tab", "query"), hint(LEAVE_SEARCH), hint(QUIT)]);
        if search.has_more() {
            line.push(hint(PAGE_KEYS));
        }
        return line;
    }
    let mut line = vec![
        hint(SEARCH),
        hint(PLAY),
        ("Space", if active { "stop" } else { "play" }),
        hint(QUIT),
        hint(VOLUME),
        hint(MUTE),
        hint(NEXT),
        hint(RECORD),
    ];
    let adds = playing.is_some_and(|s| favorites::find_match(&view.favorites, s).is_none());
    if adds {
        line.push(("f", "add"));
    } else if view.cursor().is_some() {
        line.push(("f", "remove"));
    }
    line
}

/// What every help line ends with
pub fn help_line_end() -> (&'static str, &'static str) {
    (HELP.hint_keys, HELP.hint)
}

/// What `key` does. `playing` is the station playing (or last played), for
/// `f` in the favorites list.
pub fn route(key: KeyEvent, view: &mut View, playing: Option<&Station>) -> Effect {
    let code = match key.code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    };
    let modifiers = key.modifiers - KeyModifiers::SHIFT;
    // An edit's error stays until the next key (or a few seconds)
    view.dismiss_edit_error();
    if modifiers == KeyModifiers::CONTROL {
        return match code {
            KeyCode::Char('c' | 'q') => Effect::Quit,
            KeyCode::Char('f') if view.confirmation.is_none() && view.help.is_none() => {
                view.search.open();
                Effect::None
            }
            _ => Effect::None,
        };
    }
    if !modifiers.is_empty() {
        return Effect::None;
    }

    // The help screen: arrows scroll it, any other key closes it
    if let Some(scroll) = view.help.as_mut() {
        match code {
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(PAGE as u16),
            KeyCode::PageDown => *scroll = scroll.saturating_add(PAGE as u16),
            KeyCode::Home => *scroll = 0,
            _ => view.help = None,
        }
        return Effect::None;
    }
    // `?` is a letter in the query; F1 works everywhere
    let typing = view.search.open && view.search.editing && view.confirmation.is_none();
    if code == KeyCode::F(1) || (code == KeyCode::Char('?') && !typing) {
        view.help = Some(0);
        return Effect::None;
    }

    if view.confirmation.is_some() {
        return match code {
            KeyCode::Enter | KeyCode::Char('y') => {
                let confirmation = view.confirmation.take().unwrap();
                view.begin_edit(FavoriteEdit::Remove(confirmation.id))
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                view.confirmation = None;
                Effect::None
            }
            KeyCode::Char('q') => Effect::Quit,
            _ => Effect::None,
        };
    }

    let search = &mut view.search;
    if search.open {
        match code {
            KeyCode::Esc => {
                search.close();
                return Effect::CloseSearch;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                // With no results there is nothing to move to, and letters
                // typed there would be commands (`q` quits)
                if !search.editing || !search.rows.is_empty() || search.error.is_some() {
                    search.editing = !search.editing;
                }
                return Effect::None;
            }
            // Down from the query moves into the results, Up from the
            // first result back to the query
            KeyCode::Down | KeyCode::PageDown if search.editing && !search.rows.is_empty() => {
                search.editing = false;
                if search.list.selected().is_none() {
                    search.list.select(Some(0));
                }
                return Effect::None;
            }
            KeyCode::Up if !search.editing && search.list.selected().unwrap_or(0) == 0 => {
                search.editing = true;
                return Effect::None;
            }
            _ => {}
        }
        if search.editing {
            match key.code {
                KeyCode::Char(c) if !c.is_control() => search.insert(c),
                KeyCode::Left => search.caret = search.previous_boundary(),
                KeyCode::Right => search.caret = search.next_boundary(),
                KeyCode::Home => search.caret = 0,
                KeyCode::End => search.caret = search.query.len(),
                KeyCode::Backspace => search.delete(search.previous_boundary(), search.caret),
                KeyCode::Delete => search.delete(search.caret, search.next_boundary()),
                KeyCode::Enter => return Effect::Search(search.submit()),
                _ => {}
            }
            return Effect::None;
        }
    }

    let delta = match code {
        KeyCode::Up | KeyCode::Char('k') => Some(-1),
        KeyCode::Down | KeyCode::Char('j') => Some(1),
        KeyCode::PageUp => Some(-PAGE),
        KeyCode::PageDown => Some(PAGE),
        KeyCode::Home => Some(isize::MIN / 2),
        KeyCode::End => Some(isize::MAX / 2),
        _ => None,
    };
    if let Some(delta) = delta {
        if search.open {
            let page_at_end =
                matches!(code, KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown);
            return search
                .move_cursor(delta, page_at_end)
                .map_or(Effect::None, Effect::Search);
        }
        view.move_cursor(delta);
        return Effect::None;
    }

    match code {
        // Esc never quits: it is "back" (out of search), and one press too
        // many would stop the music and end a recording
        KeyCode::Char('q') => Effect::Quit,
        KeyCode::Char('/') => {
            search.open();
            Effect::None
        }
        KeyCode::Enter => search.retry().map_or(Effect::Play, Effect::Search),
        KeyCode::Char(' ') => Effect::TogglePlayback,
        KeyCode::Char('n') => Effect::StepFavorite(1),
        KeyCode::Char('p') => Effect::StepFavorite(-1),
        KeyCode::Char('+' | '=') => Effect::Volume(1),
        KeyCode::Char('-' | '_') => Effect::Volume(-1),
        KeyCode::Char('m') => Effect::Mute,
        KeyCode::Char('r') => Effect::Record,
        KeyCode::Char('f') if !view.saving_favorites => toggle_favorite(view, playing),
        _ => Effect::None,
    }
}

/// `f`: in the favorites list, add the station playing when it is not a
/// favorite yet (as `F` in the app), or else ask to remove the highlighted
/// row. In search, add or remove the highlighted result.
fn toggle_favorite(view: &mut View, playing: Option<&Station>) -> Effect {
    if !view.search.open {
        if let Some(station) =
            playing.filter(|s| favorites::find_match(&view.favorites, s).is_none())
        {
            let favorite = favorites::new_favorite(station);
            return view.begin_edit(FavoriteEdit::Add(Box::new(favorite)));
        }
    }
    let Some(station) = view.highlighted() else {
        return Effect::None;
    };
    if let Some(favorite) = favorites::find_match(&view.favorites, station) {
        view.confirmation = Some(Confirmation {
            id: favorite.id(),
            name: favorite.name.clone(),
        });
        Effect::None
    } else {
        let favorite = favorites::new_favorite(station);
        view.begin_edit(FavoriteEdit::Add(Box::new(favorite)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Station;
    use crate::search::Reply;
    use crate::ui::{ColorMode, Palette, DEFAULT_ACCENT};
    use radiotrope_library::providers::SearchResults;

    fn key(code: KeyCode, view: &mut View) -> Effect {
        route(KeyEvent::new(code, KeyModifiers::NONE), view, None)
    }

    fn key_playing(code: KeyCode, view: &mut View, playing: &Station) -> Effect {
        route(KeyEvent::new(code, KeyModifiers::NONE), view, Some(playing))
    }

    fn view() -> View {
        View::new(
            vec![
                Station::new("Saved", "https://saved.test"),
                Station::new("Other", "https://other.test"),
            ],
            Palette::new(ColorMode::None, DEFAULT_ACCENT),
            DEFAULT_ACCENT,
        )
    }

    #[test]
    fn query_keys_are_text_before_shortcuts_and_control_keys_go_first() {
        let mut view = view();
        assert!(matches!(
            route(
                KeyEvent::new(
                    KeyCode::Char('F'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ),
                &mut view,
                None
            ),
            Effect::None
        ));
        assert!(view.search.open && view.search.editing);
        for c in ['Q', '/', 'f', 'R', ' ', '界', 'a', '\u{301}'] {
            assert!(matches!(key(KeyCode::Char(c), &mut view), Effect::None));
        }
        assert_eq!(view.search.query, "Q/fR 界a\u{301}");
        key(KeyCode::Backspace, &mut view);
        key(KeyCode::Left, &mut view);
        key(KeyCode::Delete, &mut view);
        assert_eq!(view.search.query, "Q/fR ");
        let Effect::Search(request) = key(KeyCode::Enter, &mut view) else {
            panic!("expected search")
        };
        view.search.apply(Reply {
            request,
            result: Ok(SearchResults {
                stations: vec![Station::new("News", "https://news.test")],
                total: None,
                has_more: false,
            }),
        });
        key(KeyCode::Tab, &mut view);
        assert!(matches!(key(KeyCode::Char('R'), &mut view), Effect::Record));
        assert!(matches!(
            route(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT),
                &mut view,
                None
            ),
            Effect::None
        ));
        assert!(matches!(key(KeyCode::Char('Q'), &mut view), Effect::Quit));
        for c in ['C', 'q'] {
            assert!(matches!(
                route(
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
                    &mut view,
                    None
                ),
                Effect::Quit
            ));
        }
    }

    #[test]
    fn search_keeps_favorite_cursor_and_prioritizes_retry_over_play() {
        let mut view = view();
        key(KeyCode::End, &mut view);
        *view.list.offset_mut() = 1;
        key(KeyCode::Char('/'), &mut view);
        let Effect::Search(request) = key(KeyCode::Enter, &mut view) else {
            panic!("expected search")
        };
        view.search.apply(Reply {
            request,
            result: Err("offline".into()),
        });
        key(KeyCode::Tab, &mut view);
        let Effect::Search(request) = key(KeyCode::Enter, &mut view) else {
            panic!("expected retry")
        };
        view.search.apply(Reply {
            request,
            result: Ok(SearchResults {
                stations: vec![Station::new("News", "https://news.test")],
                total: None,
                has_more: false,
            }),
        });
        assert!(matches!(key(KeyCode::Enter, &mut view), Effect::Play));
        assert!(matches!(
            key(KeyCode::Char('N'), &mut view),
            Effect::StepFavorite(1)
        ));
        assert!(matches!(key(KeyCode::Esc, &mut view), Effect::CloseSearch));
        assert_eq!(view.cursor(), Some(1));
        assert_eq!(view.list.offset(), 1);
        // Esc never quits
        assert!(matches!(key(KeyCode::Esc, &mut view), Effect::None));
        assert!(!view.search.open);
    }

    #[test]
    fn removal_is_explicit_and_save_errors_keep_the_visible_favorite() {
        let mut view = view();
        for cancel in [KeyCode::Esc, KeyCode::Char('N')] {
            assert!(matches!(key(KeyCode::Char('F'), &mut view), Effect::None));
            assert_eq!(view.confirmation.as_ref().unwrap().name, "Saved");
            assert!(matches!(key(cancel, &mut view), Effect::None));
            assert!(view.confirmation.is_none());
        }
        key(KeyCode::Char('F'), &mut view);
        assert!(matches!(
            key(KeyCode::Char('Y'), &mut view),
            Effect::Edit(FavoriteEdit::Remove(_))
        ));
        assert!(view.saving_favorites);
        view.apply_favorites(crate::favorites::Reply::Edited(Err("read-only".into())));
        assert!(!view.saving_favorites);
        assert_eq!(view.favorites[0].name, "Saved");
        assert_eq!(view.favorites_error.as_deref(), Some("read-only"));
    }

    fn results(view: &mut View, stations: Vec<Station>) {
        key(KeyCode::Char('/'), view);
        let Effect::Search(request) = key(KeyCode::Enter, view) else {
            panic!("expected search")
        };
        view.search.apply(Reply {
            request,
            result: Ok(SearchResults {
                stations,
                total: None,
                has_more: false,
            }),
        });
    }

    #[test]
    fn f_adds_the_playing_station_when_it_is_not_a_favorite() {
        let mut view = view();
        let from_url = Station::new("", "http://me:pw@live.test/stream");
        let Effect::Edit(FavoriteEdit::Add(favorite)) =
            key_playing(KeyCode::Char('f'), &mut view, &from_url)
        else {
            panic!("expected the playing station to be added")
        };
        assert_eq!(favorite.url(), from_url.url);
        assert_eq!(favorite.name(), "live.test");
        assert!(view.confirmation.is_none() && view.saving_favorites);
        // One edit at a time
        assert!(matches!(
            key_playing(KeyCode::Char('f'), &mut view, &from_url),
            Effect::None
        ));
        view.apply_favorites(crate::favorites::Reply::Edited(Err("busy".into())));

        // Playing a favorite: `f` asks about the highlighted row
        let saved = view.favorites[0].clone();
        key(KeyCode::Down, &mut view);
        key_playing(KeyCode::Char('f'), &mut view, &saved);
        assert_eq!(view.confirmation.take().unwrap().name, "Other");

        // Nothing played and no favorites: nothing to do
        let mut empty = View::new(
            vec![],
            Palette::new(ColorMode::None, DEFAULT_ACCENT),
            DEFAULT_ACCENT,
        );
        assert!(matches!(key(KeyCode::Char('f'), &mut empty), Effect::None));
        assert!(empty.confirmation.is_none() && !empty.saving_favorites);
    }

    #[test]
    fn f_in_search_acts_on_the_highlighted_result() {
        let mut view = view();
        results(&mut view, vec![Station::new("News", "https://news.test")]);
        key(KeyCode::Tab, &mut view);
        let playing = Station::new("", "https://elsewhere.test");
        let Effect::Edit(FavoriteEdit::Add(favorite)) =
            key_playing(KeyCode::Char('f'), &mut view, &playing)
        else {
            panic!("expected the result to be added")
        };
        assert_eq!(favorite.name(), "News");
    }

    #[test]
    fn down_and_up_move_between_the_query_and_the_results() {
        let mut view = view();
        key(KeyCode::Char('/'), &mut view);
        // No results yet: Down stays in the query
        assert!(matches!(key(KeyCode::Down, &mut view), Effect::None));
        assert!(view.search.editing);
        results(
            &mut view,
            vec![
                Station::new("A", "https://a.test"),
                Station::new("B", "https://b.test"),
            ],
        );
        assert!(view.search.editing);
        key(KeyCode::Down, &mut view);
        assert!(!view.search.editing);
        assert_eq!(view.search.list.selected(), Some(0));
        key(KeyCode::Down, &mut view);
        assert_eq!(view.search.list.selected(), Some(1));
        key(KeyCode::Up, &mut view);
        assert!(!view.search.editing);
        assert_eq!(view.search.list.selected(), Some(0));
        key(KeyCode::Up, &mut view);
        assert!(view.search.editing);
        key(KeyCode::PageDown, &mut view);
        assert!(!view.search.editing);
        assert!(matches!(key(KeyCode::Enter, &mut view), Effect::Play));
    }

    #[test]
    fn tab_with_nothing_to_move_to_keeps_typing_in_the_query() {
        let mut view = view();
        key(KeyCode::Char('/'), &mut view);
        key(KeyCode::Tab, &mut view);
        assert!(view.search.editing);
        // So `q` is a letter, not quit
        assert!(matches!(key(KeyCode::Char('q'), &mut view), Effect::None));
        assert_eq!(view.search.query, "q");

        // An error to retry: Tab moves to Enter retry, and back
        let Effect::Search(request) = key(KeyCode::Enter, &mut view) else {
            panic!("expected search")
        };
        view.search.apply(Reply {
            request,
            result: Err("offline".into()),
        });
        key(KeyCode::Tab, &mut view);
        assert!(!view.search.editing);
        key(KeyCode::Tab, &mut view);
        assert!(view.search.editing);
    }

    fn line(view: &View, playing: Option<&Station>, active: bool) -> Vec<String> {
        help_line(view, playing, active)
            .into_iter()
            .map(|(k, w)| format!("{k} {w}"))
            .collect()
    }

    #[test]
    fn the_help_line_leads_with_the_main_keys() {
        let mut view = view();
        let first = line(&view, None, false);
        assert_eq!(
            first[..4],
            ["/ search", "Enter play", "Space play", "q quit"]
        );
        assert_eq!(first.last().unwrap(), "f remove");
        assert_eq!(line(&view, None, true)[2], "Space stop");
        let elsewhere = Station::new("", "https://elsewhere.test");
        assert_eq!(line(&view, Some(&elsewhere), true).last().unwrap(), "f add");

        // Search: PgDn more only when there is more
        results(&mut view, vec![Station::new("News", "https://news.test")]);
        assert_eq!(
            line(&view, None, false),
            ["Enter search", "Tab results", "Esc back"]
        );
        key(KeyCode::Tab, &mut view);
        let results_line = line(&view, None, false);
        assert!(results_line.contains(&"f add".to_string()));
        assert!(!results_line.iter().any(|k| k.starts_with("PgDn")));
    }

    #[test]
    fn question_mark_and_f1_open_the_help_and_any_key_closes_it() {
        let mut view = view();
        key(KeyCode::Char('?'), &mut view);
        assert_eq!(view.help, Some(0));
        key(KeyCode::Down, &mut view);
        assert_eq!(view.help, Some(1));
        // The closing key does nothing else
        assert!(matches!(key(KeyCode::Char('q'), &mut view), Effect::None));
        assert_eq!(view.help, None);

        // In the query `?` is a letter, F1 still opens the help
        key(KeyCode::Char('/'), &mut view);
        key(KeyCode::Char('?'), &mut view);
        assert_eq!(view.search.query, "?");
        assert_eq!(view.help, None);
        key(KeyCode::F(1), &mut view);
        assert_eq!(view.help, Some(0));
        key(KeyCode::Esc, &mut view);
        assert!(view.help.is_none() && view.search.open);
    }

    /// `--help` lists the main keys as the help screen has them
    #[test]
    fn help_keys_match_the_command_line_help() {
        let after_help = crate::args::AFTER_HELP;
        for k in [PLAY, PLAY_STOP, NEXT, VOLUME, MUTE, RECORD, HELP] {
            assert!(
                after_help
                    .lines()
                    .any(|l| l.trim_start().starts_with(k.keys) && l.contains(k.what)),
                "--help has no line for {}: {}",
                k.keys,
                k.what
            );
        }
        for (keys, first_words) in [("/", "Search the station directory"), ("q", "Quit")] {
            assert!(after_help.contains(&format!("  {keys}")) && after_help.contains(first_words));
        }
    }

    #[test]
    fn an_edit_error_clears_on_the_next_key() {
        let mut view = view();
        view.apply_favorites(crate::favorites::Reply::Edited(Err("read-only".into())));
        assert!(view.favorites_error.is_some());
        key(KeyCode::Down, &mut view);
        assert!(view.favorites_error.is_none());
    }

    #[test]
    fn esc_never_quits() {
        let mut view = view();
        assert!(matches!(key(KeyCode::Esc, &mut view), Effect::None));
        view.confirmation = Some(Confirmation {
            id: "x".into(),
            name: "Saved".into(),
        });
        assert!(matches!(key(KeyCode::Esc, &mut view), Effect::None));
        assert!(view.confirmation.is_none());
        assert!(matches!(key(KeyCode::Char('q'), &mut view), Effect::Quit));
    }
}
