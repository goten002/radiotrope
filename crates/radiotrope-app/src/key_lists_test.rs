//! The three lists with the keyboard only, in the real `App`, headless:
//! each is one Tab stop with an active row that the arrows move.

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

    // From the field: its ✕, Search, Country, its ✕, Genre, its ✕, Reset
    // Filters, the list
    press_times(&ui, &time, Key::Tab, 8);
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

#[test]
fn the_picker_reaches_the_40th_country_by_keyboard() {
    let (ui, time) = open("picker-country");
    ui.set_filter_entries(model((0..60).map(|index| FilterEntry {
        name: format!("Country {}", index + 1).into(),
        code: format!("C{}", index + 1).into(),
        station_count: 10,
        flag: Image::default(),
    })));
    settle(&time);
    let searched = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_search_stations({
        let searched = searched.clone();
        move |_, country, _| searched.borrow_mut().push(country.to_string())
    });

    // The filter field has the focus: Down goes to "Any country", then
    // 40 more to the 40th country
    press_times(&ui, &time, Key::DownArrow, 41);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(*searched.borrow(), ["C40"]);
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
