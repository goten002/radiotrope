//! The lists with the keyboard only, in the real `App`, headless: each is
//! one Tab stop with an active row that the arrows move. Search Stations'
//! filters too: shown with their button, picked by typing or with the
//! arrows, and reset.

use std::cell::RefCell;

use super::*;

/// The window showing `shot`, on a platform of this test's own (Slint's
/// platform is set once per thread, and each test runs on its own)
fn open(shot: &str) -> (App, Rc<Cell<Duration>>) {
    let platform = HeadlessPlatform::default();
    let made = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform)).expect("no other Slint platform yet");
    let options = Options {
        theme: Theme::Dark,
        scale: Some(0.5),
        ..Options::default()
    };
    let shot = SHOTS
        .iter()
        .find(|candidate| candidate.name == shot)
        .expect("the shot exists");
    let staged = stage(&made, shot, "dark", &options).expect("the screen opens");
    settle(&time);
    (staged.ui, time)
}

/// Let change handlers run and the short transitions finish
fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

fn press_times(ui: &App, time: &Cell<Duration>, key: Key, times: usize) {
    for _ in 0..times {
        press(ui, key);
        settle(time);
    }
}

fn type_text(ui: &App, time: &Cell<Duration>, text: &str) {
    let text = SharedString::from(text);
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
    settle(time);
}

fn results(range: std::ops::Range<usize>) -> Vec<BrowseStation> {
    range
        .map(|index| BrowseStation {
            name: format!("Station {}", index + 1).into(),
            url: format!("https://radio.example/result/{}", index + 1).into(),
            country: "United Kingdom".into(),
            genres: "Jazz".into(),
            logo_url: "".into(),
            provider_id: format!("directory-{}", index + 1).into(),
            codec: "MP3".into(),
            bitrate: 128,
            is_favorite: false,
        })
        .collect()
}

#[test]
fn search_results_reach_row_40_the_next_page_and_the_star_by_keyboard() {
    let (ui, time) = open("search-results");
    let stations = Rc::new(VecModel::from(results(0..60)));
    ui.set_search_results(ModelRc::from(stations.clone()));
    ui.set_browse_logos(ModelRc::default());
    ui.set_has_more(true);
    settle(&time);

    let played = Rc::new(RefCell::new(Vec::<String>::new()));
    let starred = Rc::new(RefCell::new(Vec::<String>::new()));
    let pages = Rc::new(Cell::new(0));
    ui.on_play_station({
        let played = played.clone();
        move |station| played.borrow_mut().push(station.name.to_string())
    });
    ui.on_toggle_browse_favorite({
        let starred = starred.clone();
        move |station| starred.borrow_mut().push(station.name.to_string())
    });
    ui.on_load_more_stations({
        let pages = pages.clone();
        let stations = stations.clone();
        let ui = ui.as_weak();
        move || {
            pages.set(pages.get() + 1);
            for station in results(60..80) {
                stations.push(station);
            }
            let ui = ui.unwrap();
            ui.set_has_more(false);
            ui.set_search_loading_more(false);
        }
    });

    // The search field has the focus: Down goes into the list, on row 1
    press_times(&ui, &time, Key::DownArrow, 40);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(*played.borrow(), ["Station 40"]);
    assert_eq!(pages.get(), 0, "row 40 of 60 is not near the end yet");

    type_text(&ui, &time, "f");
    assert_eq!(*starred.borrow(), ["Station 40"]);

    // Page Down moves by less than a screen; End loads the next page
    press(&ui, Key::PageDown);
    settle(&time);
    press(&ui, Key::Return);
    settle(&time);
    let after_page = played.borrow()[1].clone();
    assert!(
        after_page != "Station 40" && after_page != "Station 60",
        "Page Down went to {after_page}"
    );

    press(&ui, Key::End);
    settle(&time);
    assert_eq!(pages.get(), 1, "the last row asks for the next page");
    press(&ui, Key::End);
    settle(&time);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(played.borrow()[2], "Station 80");

    // Home, then Up, goes back to the search field: Esc still closes
    press(&ui, Key::Home);
    settle(&time);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(played.borrow()[3], "Station 1");
    press(&ui, Key::UpArrow);
    settle(&time);
    type_text(&ui, &time, "f");
    assert_eq!(starred.borrow().len(), 1, "f is typed in the field again");
    press(&ui, Key::Escape);
    settle(&time);
    assert!(!ui.get_dialog_open());
}

#[test]
fn the_list_is_one_tab_stop_in_search() {
    let (ui, time) = open("search-results");
    ui.set_search_results(ModelRc::from(Rc::new(VecModel::from(results(0..60)))));
    ui.set_browse_logos(ModelRc::default());
    settle(&time);
    let played = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_play_station({
        let played = played.clone();
        move |station| played.borrow_mut().push(station.name.to_string())
    });

    // From the field: its ✕, Filters, Search, the list (the filters are
    // hidden)
    press_times(&ui, &time, Key::Tab, 4);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(*played.borrow(), ["Station 1"]);
    // One more Tab leaves the list (for the close button): Enter closes
    press(&ui, Key::Tab);
    settle(&time);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(played.borrow().len(), 1);
    assert!(!ui.get_dialog_open());
}

/// Every search asked for, as (country code, genre slug)
fn searches(ui: &App) -> Rc<RefCell<Vec<(String, String)>>> {
    let searched = Rc::new(RefCell::new(Vec::new()));
    ui.on_search_stations({
        let searched = searched.clone();
        move |_, country, genre| {
            searched
                .borrow_mut()
                .push((country.to_string(), genre.to_string()))
        }
    });
    searched
}

#[test]
fn filters_are_shown_picked_and_reset_by_keyboard() {
    let (ui, time) = open("search-results");
    let searched = searches(&ui);
    let asked = |index: usize| searched.borrow()[index].clone();

    // From the search field: its ✕, then Filters. Enter shows the filters.
    press_times(&ui, &time, Key::Tab, 2);
    press(&ui, Key::Return);
    settle(&time);
    // Search, then Country: Enter opens its list, with the text box ready
    press_times(&ui, &time, Key::Tab, 2);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(ui.get_filter_kind(), "country");
    type_text(&ui, &time, "united k");
    let found: Vec<String> = ui
        .get_filter_entries()
        .iter()
        .map(|entry| entry.name.to_string())
        .collect();
    assert_eq!(found, ["United Kingdom"]);
    // Enter in the text box picks the first match
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(asked(0), ("GB".to_string(), String::new()));
    assert!(ui.get_dialog_open());

    // The focus is back on Country: its ✕, then Genre. Down opens the
    // list too; in it, Down goes from the text box to Any genre, on to
    // Ambient and to Blues
    press_times(&ui, &time, Key::Tab, 2);
    press(&ui, Key::DownArrow);
    settle(&time);
    assert_eq!(ui.get_filter_kind(), "genre");
    press_times(&ui, &time, Key::DownArrow, 3);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(asked(1), ("GB".to_string(), "blues".to_string()));

    // Genre's ✕, then Reset Filters
    press_times(&ui, &time, Key::Tab, 2);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(asked(2), (String::new(), String::new()));
    assert_eq!(searched.borrow().len(), 3);
}

#[test]
fn esc_closes_a_filter_list_and_leaves_the_dialog() {
    let (ui, time) = open("search-filters");
    let searched = searches(&ui);

    // From the search field: its ✕, Filters, Search, Country
    press_times(&ui, &time, Key::Tab, 4);
    press(&ui, Key::Return);
    settle(&time);
    type_text(&ui, &time, "fr");
    assert_eq!(ui.get_filter_entries().row_count(), 1);
    press(&ui, Key::Escape);
    settle(&time);
    assert!(ui.get_dialog_open(), "Esc closed the list only");
    assert!(searched.borrow().is_empty(), "nothing was picked");

    // Opened again, the list is whole: the text typed before is gone.
    // Up from the first row goes back to the text box, where Enter picks
    // nothing while nothing is typed
    press(&ui, Key::Return);
    settle(&time);
    assert!(ui.get_filter_entries().row_count() > 1);
    press_times(&ui, &time, Key::DownArrow, 1);
    press(&ui, Key::UpArrow);
    settle(&time);
    press(&ui, Key::Return);
    settle(&time);
    assert!(searched.borrow().is_empty());

    press(&ui, Key::Escape);
    settle(&time);
    assert!(ui.get_dialog_open());
    press(&ui, Key::Escape);
    settle(&time);
    assert!(!ui.get_dialog_open());
}

#[test]
fn a_filter_list_reaches_the_40th_country_by_keyboard() {
    let (ui, time) = open("search-filters");
    ui.on_load_filter_entries({
        let ui = ui.as_weak();
        move |kind| {
            crate::browse_ui::keep_filter_entries(
                &kind,
                (0..60)
                    .map(|index| FilterEntry {
                        name: format!("Country {:02}", index + 1).into(),
                        code: format!("C{}", index + 1).into(),
                        station_count: 10,
                        flag: Image::default(),
                    })
                    .collect(),
            );
            crate::browse_ui::show_filter_entries(&ui.unwrap());
        }
    });
    let searched = searches(&ui);

    press_times(&ui, &time, Key::Tab, 4);
    press(&ui, Key::Return);
    settle(&time);
    // The text box has the focus: Down goes to "Any country", then 40
    // more to the 40th country
    press_times(&ui, &time, Key::DownArrow, 41);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(*searched.borrow(), [("C40".to_string(), String::new())]);

    // Page Down and End move in the list too
    press(&ui, Key::Return);
    settle(&time);
    press(&ui, Key::DownArrow);
    settle(&time);
    press(&ui, Key::End);
    settle(&time);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(searched.borrow()[1].0, "C60");
}

#[test]
fn undo_is_offered_in_search_and_taken_with_ctrl_z() {
    let (ui, time) = open("search-results");
    let undone = Rc::new(Cell::new(0));
    ui.on_undo_remove_favorite({
        let undone = undone.clone();
        let ui = ui.as_weak();
        move || {
            undone.set(undone.get() + 1);
            ui.unwrap().set_favorite_undo(0);
        }
    });
    let ctrl_z = || {
        let control = SharedString::from(Key::Control);
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: control.clone(),
        });
        type_text(&ui, &time, "z");
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text: control });
        settle(&time);
    };

    // Nothing was removed: Ctrl+Z is the search field's own
    ctrl_z();
    assert_eq!(undone.get(), 0);

    // A favorite was un-starred: the offer shows here, and Ctrl+Z takes it
    ui.set_favorite_undo(1);
    settle(&time);
    ctrl_z();
    assert_eq!(undone.get(), 1);

    // Offered again, its Undo button is a Tab stop after the list: the
    // field's ✕, Filters, Search, the list, Undo
    ui.set_favorite_undo(2);
    settle(&time);
    press_times(&ui, &time, Key::Tab, 5);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(undone.get(), 2);
    // The offer went away with the focus in it: Esc still closes
    press(&ui, Key::Escape);
    settle(&time);
    assert!(!ui.get_dialog_open());
}

#[test]
fn favorites_are_one_tab_stop_on_the_highlighted_row() {
    let (ui, time) = open("main-stopped");
    ui.on_locate_favorite(|model, highlighted, playing, action, page| {
        crate::shortcuts::favorite_highlight(
            model.iter().map(|station| station.url),
            highlighted.as_str(),
            playing.as_str(),
            action,
            page.max(0) as usize,
        )
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1)
    });
    let played = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_play_favorite({
        let played = played.clone();
        move |station| played.borrow_mut().push(station.name.to_string())
    });
    let names: Vec<String> = ui
        .get_favorites_list()
        .iter()
        .map(|station| station.name.to_string())
        .collect();

    // Arrows from anywhere in the window highlight a row and the list
    // takes the focus, so the next arrow and Enter are the list's own
    press_times(&ui, &time, Key::DownArrow, 3);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(*played.borrow(), [names[2].clone()]);

    // Away and back by Tab: the list is one stop, on the highlighted row,
    // and Space plays it like any focused button
    press(&ui, Key::Tab);
    settle(&time);
    press(&ui, Key::Backtab);
    settle(&time);
    type_text(&ui, &time, " ");
    assert_eq!(*played.borrow(), [names[2].clone(), names[2].clone()]);
    press(&ui, Key::End);
    settle(&time);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(played.borrow()[2], *names.last().unwrap());

    // With a dialog open nothing in the list takes keys
    ui.invoke_snapshot_open("about".into(), "".into());
    settle(&time);
    assert!(ui.get_dialog_open());
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(played.borrow().len(), 3);
}
