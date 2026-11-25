//! A station's streams with the keyboard only, in the real `App`,
//! headless: the list in Edit Favorite (pick the default, add, save) and a
//! search result's stream menu.

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

/// What Edit Favorite saved: (the default's address, every address)
type Saved = Rc<RefCell<Vec<(String, Vec<String>)>>>;

fn saves(ui: &App) -> Saved {
    let saved = Saved::default();
    ui.on_edit_favorite({
        let saved = saved.clone();
        move |station| {
            saved.borrow_mut().push((
                station.url.to_string(),
                station
                    .streams
                    .iter()
                    .map(|stream| stream.url.to_string())
                    .collect(),
            ));
            SharedString::new()
        }
    });
    saved
}

const MAIN: &str = "https://radio.example/stream/2";
const BACKUP: &str = "https://backup.radio.example/stream/2.mp3";
const RELAY: &str = "https://relay.radio.example/a-long-mount-name/stream-2-low.mp3";

#[test]
fn another_default_is_picked_and_saved_by_keyboard() {
    let (ui, time) = open("edit-favorite-streams");
    let saved = saves(&ui);

    // From the name: the first stream's radio button, its Move Down and
    // Remove (it can't move up), then the second stream's radio button
    press_times(&ui, &time, Key::Tab, 4);
    press(&ui, Key::Space);
    settle(&time);
    // Back to the name, where Enter saves
    press_times(&ui, &time, Key::Backtab, 4);
    press(&ui, Key::Return);
    settle(&time);

    assert_eq!(
        *saved.borrow(),
        [(
            BACKUP.to_string(),
            vec![MAIN.to_string(), BACKUP.to_string(), RELAY.to_string()]
        )]
    );
    assert!(!ui.get_dialog_open(), "saved: the dialog closes");
}

#[test]
fn a_stream_is_added_with_enter_and_a_second_enter_saves() {
    let (ui, time) = open("edit-favorite-streams");
    let saved = saves(&ui);

    // Past the three rows (a radio button, the moves it has, Remove) to
    // the field for a new address
    press_times(&ui, &time, Key::Tab, 11);
    type_text(&ui, &time, "https://new.example/live");
    press(&ui, Key::Return);
    settle(&time);
    assert!(saved.borrow().is_empty(), "Enter adds the address first");
    assert!(ui.get_dialog_open());

    // The field is empty again: Enter saves
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(
        *saved.borrow(),
        [(
            MAIN.to_string(),
            vec![
                MAIN.to_string(),
                BACKUP.to_string(),
                RELAY.to_string(),
                "https://new.example/live".to_string()
            ]
        )]
    );
}

#[test]
fn removing_the_default_makes_the_first_stream_the_default() {
    let (ui, time) = open("edit-favorite-streams");
    let saved = saves(&ui);

    // The first stream (the default): its radio button, Move Down, Remove
    press_times(&ui, &time, Key::Tab, 3);
    press(&ui, Key::Space);
    settle(&time);
    // The row is gone and its button with it: the field for a new
    // address has the focus, where Enter (with nothing typed) saves
    press(&ui, Key::Return);
    settle(&time);

    assert_eq!(
        *saved.borrow(),
        [(
            BACKUP.to_string(),
            vec![BACKUP.to_string(), RELAY.to_string()]
        )]
    );
}

#[test]
fn a_results_streams_open_with_the_menu_key_and_one_is_played() {
    let (ui, time) = open("search-results");
    let picked = Rc::new(RefCell::new(Vec::<(i32, i32)>::new()));
    let played = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_pick_browse_stream({
        let picked = picked.clone();
        let ui = ui.as_weak();
        move |row, stream| {
            picked.borrow_mut().push((row, stream));
            // As the window does: the row plays that stream from now on
            let ui = ui.unwrap();
            let model = ui.get_search_results();
            let item = model.row_data(row as usize).unwrap();
            model.set_row_data(row as usize, crate::browse_ui::with_stream(item, stream));
        }
    });
    ui.on_play_station({
        let played = played.clone();
        move |station| played.borrow_mut().push(station.url.to_string())
    });

    // Into the list, on its first row, which has three streams
    press(&ui, Key::DownArrow);
    settle(&time);
    press(&ui, Key::Menu);
    settle(&time);
    // The menu has the keys: the second stream, then the third, and Enter
    press_times(&ui, &time, Key::DownArrow, 3);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(*picked.borrow(), [(0, 2)]);
    assert_eq!(
        *played.borrow(),
        ["https://relay.radio.example/a-long-mount-name/stream-1-low.mp3"]
    );
    assert!(ui.get_dialog_open());

    // Esc closes an open menu and leaves the dialog; the list has the
    // keys again, and Enter plays the row with the stream picked
    press(&ui, Key::Menu);
    settle(&time);
    press(&ui, Key::Escape);
    settle(&time);
    assert!(ui.get_dialog_open());
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(picked.borrow().len(), 1);
    assert_eq!(played.borrow().len(), 2);
    assert_eq!(played.borrow()[0], played.borrow()[1]);

    // A row with one stream has no menu: the key does nothing
    press(&ui, Key::DownArrow);
    settle(&time);
    press(&ui, Key::Menu);
    settle(&time);
    press(&ui, Key::Return);
    settle(&time);
    assert_eq!(picked.borrow().len(), 1);
    assert_eq!(played.borrow().len(), 3, "Enter played the second row");
}
