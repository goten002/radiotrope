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

/// What `key` does. `playing` is the station playing (or last played), for
/// `f` in the favorites list.
pub fn route(key: KeyEvent, view: &mut View, playing: Option<&Station>) -> Effect {
    let code = match key.code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    };
    let modifiers = key.modifiers - KeyModifiers::SHIFT;
    if modifiers == KeyModifiers::CONTROL {
        return match code {
            KeyCode::Char('c' | 'q') => Effect::Quit,
            KeyCode::Char('f') if view.confirmation.is_none() => {
                view.search.open();
                Effect::None
            }
            _ => Effect::None,
        };
    }
    if !modifiers.is_empty() {
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
