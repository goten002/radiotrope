//! The menu bar with the keyboard only, in the real `App`, headless: F10
//! reaches the titles, the arrow keys walk the menus and their groups, and
//! Space or Enter runs the row that has the focus.

use super::*;

/// Let change handlers run (a row takes the focus in one) and the short
/// transitions finish
fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

fn keys(ui: &App, time: &Cell<Duration>, keys: &[Key]) {
    for key in keys {
        press(ui, *key);
        settle(time);
    }
}

// One test only: Slint's platform is set once per thread
#[test]
fn the_menus_work_with_the_keyboard_only() {
    use Key::{DownArrow, End, Escape, Home, LeftArrow, Return, RightArrow, UpArrow, F10};

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
        .find(|shot| shot.name == "main-playing")
        .expect("the main window's shot");
    let staged = stage(&made, shot, "dark", &options).expect("the window opens");
    let ui = staged.ui;
    settle(&time);
    let press_keys = |list: &[Key]| keys(&ui, &time, list);

    // F10, Down: the Open menu with its first row; Right three times: the
    // Tools menu; its third row is Equalizer
    press_keys(&[
        F10, DownArrow, RightArrow, RightArrow, RightArrow, DownArrow, DownArrow,
    ]);
    assert!(!ui.get_dialog_open(), "nothing opens before Enter");
    press_keys(&[Return]);
    assert!(ui.get_dialog_open(), "Enter on Tools > Equalizer opens it");
    press_keys(&[Escape]);
    assert!(!ui.get_dialog_open(), "Esc closes the Equalizer");

    // Along the titles: F10, Right, Right is View. Its first row is the
    // theme, and Enter there moves to the next one.
    press_keys(&[F10, RightArrow, RightArrow, DownArrow]);
    assert_eq!(ui.get_theme_mode(), "dark");
    press_keys(&[Return]);
    assert_eq!(ui.get_theme_mode(), "light", "Enter on View > Theme");

    // Five rows down is the Visualizer Style group: Right unfolds it, its
    // second style is Spectrum
    assert_eq!(ui.get_viz_mode(), "wave");
    press_keys(&[DownArrow, DownArrow, DownArrow, DownArrow, DownArrow]);
    press_keys(&[RightArrow, DownArrow, DownArrow, Return]);
    assert_eq!(ui.get_viz_mode(), "spectrum", "View > Visualizer Style");
    // Left folds the group and goes back to its row; Enter unfolds it
    // again, and the first style under it is Wave
    press_keys(&[LeftArrow, Return, DownArrow, Return]);
    assert_eq!(ui.get_viz_mode(), "wave", "the group row after Left");

    // Home is the theme row again; Up from there goes round to the last
    // row, the folded System Tray group, and the one above it is Window
    press_keys(&[Home, Return]);
    assert_eq!(ui.get_theme_mode(), "system", "Home is the first row");
    press_keys(&[UpArrow, UpArrow, RightArrow, End, UpArrow]);
    assert!(!ui.get_keep_on_top());
    press_keys(&[Return]);
    assert!(ui.get_keep_on_top(), "View > Window > Always on Top");

    // Left folds the Window group and stops on its row (slot 18); Left
    // again is Stations, whose first row opens the search. The View menu's
    // own first row must not take the focus back on the way.
    press_keys(&[LeftArrow, LeftArrow]);
    assert!(!ui.get_dialog_open());
    press_keys(&[Return]);
    assert!(ui.get_dialog_open(), "Stations > Search Stations");
    ui.hide().expect("the window hides");
}
