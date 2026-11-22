//! The favorites list's Find, Undo, reordering keys, row menu and the
//! second press while recording, in the real `App`, headless, with the
//! keyboard only.

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
    ui_keys(&staged.ui);
    settle(&time);
    (staged.ui, time)
}

/// The arrow keys' handler, as the live app sets it
fn ui_keys(ui: &App) {
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
}

/// Let change handlers run and the short transitions finish
fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

fn wait(time: &Cell<Duration>, millis: u64) {
    time.set(time.get() + Duration::from_millis(millis));
    slint::platform::update_timers_and_animations();
}

fn keys(ui: &App, time: &Cell<Duration>, keys: &[Key]) {
    for key in keys {
        press(ui, *key);
        settle(time);
    }
}

fn type_text(ui: &App, time: &Cell<Duration>, text: &str) {
    for letter in text.chars() {
        let text = SharedString::from(letter.to_string());
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    }
    settle(time);
}

/// Press `key` while `modifier` is held
fn with_modifier(ui: &App, time: &Cell<Duration>, modifier: Key, key: SharedString) {
    let modifier = SharedString::from(modifier);
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: modifier.clone(),
    });
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: key });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: modifier });
    settle(time);
}

/// The names of the favorites played, as they are
fn played(ui: &App) -> Rc<RefCell<Vec<String>>> {
    let played = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_play_favorite({
        let played = played.clone();
        move |station| played.borrow_mut().push(station.name.to_string())
    });
    played
}

#[test]
fn find_narrows_the_list_and_esc_brings_it_back() {
    use Key::{DownArrow, Escape, Home, Return, UpArrow};

    let (ui, time) = open("main-stopped");
    let played = played(&ui);
    let muted = Rc::new(Cell::new(0));
    ui.on_mute_clicked({
        let muted = muted.clone();
        move || muted.set(muted.get() + 1)
    });

    // "/" opens the field; what is typed there is text, not shortcuts
    // ("m" mutes elsewhere)
    type_text(&ui, &time, "/");
    type_text(&ui, &time, "dim");
    assert_eq!(muted.get(), 0);
    // Nothing matches: Enter plays nothing
    keys(&ui, &time, &[Return]);
    assert!(played.borrow().is_empty());

    // Esc closes the field and the keys are the window's again
    keys(&ui, &time, &[Escape]);
    type_text(&ui, &time, "m");
    assert_eq!(muted.get(), 1);

    // Enter plays the first match
    type_text(&ui, &time, "/");
    type_text(&ui, &time, "RADIO");
    keys(&ui, &time, &[Return]);
    assert_eq!(*played.borrow(), ["Radio Verde"]);

    // Down goes into the matches, and only they are there
    keys(&ui, &time, &[DownArrow, DownArrow, DownArrow, Return]);
    assert_eq!(played.borrow()[1..], ["Sundial Radio"]);
    keys(&ui, &time, &[Home, Return]);
    assert_eq!(played.borrow()[2..], ["Radio Verde"]);

    // Up on the first match goes back to the field: typing narrows more
    keys(&ui, &time, &[UpArrow]);
    type_text(&ui, &time, " v");
    assert_eq!(muted.get(), 1);
    keys(&ui, &time, &[Return]);
    assert_eq!(played.borrow()[3..], ["Radio Verde"]);

    // Esc from the list shows all of it again, the highlight where it was
    keys(&ui, &time, &[DownArrow, Escape, Return]);
    assert_eq!(played.borrow()[4..], ["Radio Verde"]);
    keys(&ui, &time, &[Home, Return]);
    assert_eq!(played.borrow()[5..], ["Harbour Jazz"]);
}

#[test]
fn alt_arrows_and_the_row_menu_move_a_favorite_one_place() {
    use Key::{DownArrow, End, Escape, Menu, Return};

    let (ui, time) = open("main-stopped");
    let moves = Rc::new(RefCell::new(Vec::<(i32, i32)>::new()));
    ui.on_reorder_favorites({
        let moves = moves.clone();
        move |from, to| moves.borrow_mut().push((from, to))
    });
    let removed = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_delete_favorite({
        let removed = removed.clone();
        move |station| removed.borrow_mut().push(station.name.to_string())
    });

    // No row is highlighted yet: nothing to move
    with_modifier(&ui, &time, Key::Alt, Key::UpArrow.into());
    assert!(moves.borrow().is_empty());

    // The second row, one up and one down
    keys(&ui, &time, &[DownArrow, DownArrow]);
    with_modifier(&ui, &time, Key::Alt, Key::UpArrow.into());
    with_modifier(&ui, &time, Key::Alt, Key::DownArrow.into());
    assert_eq!(*moves.borrow(), [(1, 0), (1, 2)]);

    // Its menu with the keys: Down from Edit is Move Up, the next Move Down
    keys(&ui, &time, &[Menu, DownArrow, Return]);
    assert_eq!(moves.borrow()[2..], [(1, 0)]);
    keys(&ui, &time, &[Menu, DownArrow, DownArrow, Return]);
    assert_eq!(moves.borrow()[3..], [(1, 2)]);

    // End is Remove, which asks for a second press; Esc closes the menu
    keys(&ui, &time, &[Menu, End, Return]);
    assert!(removed.borrow().is_empty());
    keys(&ui, &time, &[Escape, Menu, End, Return, Return]);
    assert_eq!(*removed.borrow(), ["North Coast FM"]);

    // On the first row Move Up can't be used: Down skips it
    keys(&ui, &time, &[Key::Home, Menu, DownArrow, Return]);
    assert_eq!(moves.borrow()[4..], [(0, 1)]);

    // While Find narrows the list nothing moves by one place
    type_text(&ui, &time, "/");
    type_text(&ui, &time, "radio");
    keys(&ui, &time, &[DownArrow, DownArrow]);
    with_modifier(&ui, &time, Key::Alt, Key::UpArrow.into());
    assert_eq!(moves.borrow().len(), 5);
}

#[test]
fn while_recording_another_favorite_needs_a_second_press() {
    use Key::{DownArrow, Return};

    let (ui, time) = open("main-recording");
    let played = played(&ui);

    // The arrows start on the station playing, the first row: two more
    // is the third, another station
    keys(&ui, &time, &[DownArrow, DownArrow, DownArrow, Return]);
    assert!(played.borrow().is_empty());
    keys(&ui, &time, &[Return]);
    assert_eq!(*played.borrow(), ["Studio Canal"]);

    // The first press is forgotten after a while
    keys(&ui, &time, &[DownArrow, Return]);
    wait(&time, 6_000);
    keys(&ui, &time, &[Return]);
    assert_eq!(played.borrow().len(), 1);
    keys(&ui, &time, &[Return]);
    assert_eq!(played.borrow()[1..], ["Midnight Assembly"]);

    // The N and P keys ask the same way
    let url = ui.get_favorites_list().row_data(5).unwrap().url;
    assert!(!ui.invoke_confirm_switch(url.clone()));
    assert!(ui.invoke_confirm_switch(url));
    // The station recorded plays on without asking
    assert!(ui.invoke_confirm_switch(ui.get_station_url()));

    // Not recording: one press
    ui.set_is_recording(false);
    settle(&time);
    keys(&ui, &time, &[DownArrow, Return]);
    assert_eq!(played.borrow().len(), 3);
}

#[test]
fn undo_is_offered_for_a_while_and_taken_with_ctrl_z() {
    let (ui, time) = open("main-stopped");
    let undone = Rc::new(Cell::new(0));
    ui.on_undo_remove_favorite({
        let undone = undone.clone();
        move || undone.set(undone.get() + 1)
    });

    // Nothing was removed: Ctrl+Z does nothing
    with_modifier(&ui, &time, Key::Control, "z".into());
    assert_eq!(undone.get(), 0);

    ui.set_favorite_undo(1);
    settle(&time);
    with_modifier(&ui, &time, Key::Control, "z".into());
    assert_eq!(undone.get(), 1);

    // The offer goes away by itself; a second removal starts its time anew
    wait(&time, 4_000);
    ui.set_favorite_undo(2);
    settle(&time);
    wait(&time, 4_000);
    assert_eq!(ui.get_favorite_undo(), 2);
    wait(&time, 3_000);
    assert_eq!(ui.get_favorite_undo(), 0);
}

#[test]
fn while_recording_a_search_result_needs_a_second_press() {
    use Key::{DownArrow, Return};

    let (ui, time) = open("search-results");
    let picked = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_play_station({
        let picked = picked.clone();
        move |station| picked.borrow_mut().push(station.name.to_string())
    });
    let results = ui.get_search_results();
    let name = |row: usize| results.row_data(row).unwrap().name.to_string();
    ui.set_is_recording(true);
    settle(&time);

    // Down from the search field is the first result, the next the second
    keys(&ui, &time, &[DownArrow, DownArrow, Return]);
    assert!(picked.borrow().is_empty());
    keys(&ui, &time, &[Return]);
    assert_eq!(*picked.borrow(), [name(1)]);

    // The row just picked is the station now: again without asking
    keys(&ui, &time, &[Return]);
    assert_eq!(picked.borrow().len(), 2);

    // Another row, and the first press is forgotten after a while
    keys(&ui, &time, &[DownArrow, Return]);
    wait(&time, 6_000);
    keys(&ui, &time, &[Return]);
    assert_eq!(picked.borrow().len(), 2);
    keys(&ui, &time, &[Return]);
    assert_eq!(picked.borrow()[2..], [name(2)]);

    // Not recording: one press
    ui.set_is_recording(false);
    settle(&time);
    keys(&ui, &time, &[DownArrow, Return]);
    assert_eq!(picked.borrow()[3..], [name(3)]);
}

#[test]
fn while_recording_a_network_stream_needs_a_second_press() {
    use Key::Return;

    let (ui, time) = open("network-stream-empty");
    let played = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_play_stream({
        let played = played.clone();
        move |url, _, _, _| played.borrow_mut().push(url.to_string())
    });
    ui.set_is_recording(true);
    settle(&time);

    // The first Play only says what a second one does
    type_text(&ui, &time, "one.test/live");
    keys(&ui, &time, &[Return]);
    assert!(played.borrow().is_empty());
    // Another address is asked about anew
    type_text(&ui, &time, "2");
    keys(&ui, &time, &[Return]);
    assert!(played.borrow().is_empty());
    // And the first press is forgotten after a while
    wait(&time, 6_000);
    keys(&ui, &time, &[Return]);
    assert!(played.borrow().is_empty());
    keys(&ui, &time, &[Return]);
    assert_eq!(*played.borrow(), ["http://one.test/live2"]);
}

#[test]
fn a_network_stream_plays_at_once_when_not_recording() {
    use Key::Return;

    // Not recording: one press
    let (ui, time) = open("network-stream-empty");
    let played = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_play_stream({
        let played = played.clone();
        move |url, _, _, _| played.borrow_mut().push(url.to_string())
    });
    type_text(&ui, &time, "one.test/live");
    keys(&ui, &time, &[Return]);
    assert_eq!(*played.borrow(), ["http://one.test/live"]);
}

#[test]
fn the_station_recorded_plays_from_network_stream_without_asking() {
    use Key::Return;

    // The form holds the station in the player, which is being recorded
    let (ui, time) = open("network-stream-filled");
    let played = Rc::new(RefCell::new(Vec::<String>::new()));
    ui.on_play_stream({
        let played = played.clone();
        move |url, _, _, _| played.borrow_mut().push(url.to_string())
    });
    ui.set_is_recording(true);
    settle(&time);
    keys(&ui, &time, &[Return]);
    assert_eq!(*played.borrow(), [ui.get_station_url().to_string()]);
}

/// A click with the left button at (`x`, `y`) in the window
fn click(ui: &App, time: &Cell<Duration>, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    let button = slint::platform::PointerEventButton::Left;
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    settle(time);
    ui.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
    settle(time);
}

#[test]
fn an_empty_find_hides_when_it_loses_the_focus() {
    let (ui, time) = open("main-stopped");
    let played = played(&ui);
    let muted = Rc::new(Cell::new(0));
    ui.on_mute_clicked({
        let muted = muted.clone();
        move || muted.set(muted.get() + 1)
    });

    // Open and empty, a click on a row plays that row (the field goes away
    // under the pointer, the click doesn't) and the field is gone: the
    // keys are the window's again
    type_text(&ui, &time, "/");
    click(&ui, &time, 300.0, 275.0);
    assert_eq!(
        *played.borrow(),
        ["North Coast FM"],
        "the row under the click"
    );
    type_text(&ui, &time, "m");
    assert_eq!(muted.get(), 1, "the empty field hid");

    // With text it stays when a match is clicked, still narrowing the list
    type_text(&ui, &time, "/");
    type_text(&ui, &time, "sundial");
    click(&ui, &time, 300.0, 220.0);
    assert_eq!(played.borrow()[1..], ["Sundial Radio"]);
    type_text(&ui, &time, "m");
    assert_eq!(muted.get(), 1, "the field with text stayed");

    // Emptied, Tab takes the focus out of it and it hides
    keys(&ui, &time, &[Key::Escape]);
    type_text(&ui, &time, "/");
    keys(&ui, &time, &[Key::Tab]);
    type_text(&ui, &time, "m");
    assert_eq!(muted.get(), 2, "Tab hid the empty field");
}

#[test]
fn the_scroll_bar_is_grabbed_beside_it_clear_of_the_resize_band() {
    let platform = HeadlessPlatform::default();
    let made = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform)).expect("no other Slint platform yet");
    // Short enough for the list to scroll
    let options = Options {
        theme: Theme::Dark,
        scale: Some(0.5),
        size: Size::Exact(640.0, 480.0),
        ..Options::default()
    };
    let shot = SHOTS
        .iter()
        .find(|candidate| candidate.name == "main-stopped")
        .expect("the shot exists");
    let ui = stage(&made, shot, "dark", &options)
        .expect("the screen opens")
        .ui;
    settle(&time);
    let played = played(&ui);
    let button = slint::platform::PointerEventButton::Left;
    let at = |x: f32, y: f32| slint::LogicalPosition::new(x, y);

    click(&ui, &time, 300.0, 300.0);
    // The bar is drawn at x 622..630, inside the 8 px band at the right
    // edge; its grab zone starts at 614. Down from the thumb (y 144..294), there.
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: at(616.0, 170.0),
    });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: at(616.0, 170.0),
        button,
    });
    for y in (180..=330).step_by(10) {
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: at(616.0, y as f32),
        });
    }
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: at(616.0, 330.0),
        button,
    });
    settle(&time);
    click(&ui, &time, 300.0, 300.0);
    let played = played.borrow();
    assert_eq!(played.len(), 2);
    assert_ne!(
        played[0], played[1],
        "the list scrolled under the same spot"
    );
}
