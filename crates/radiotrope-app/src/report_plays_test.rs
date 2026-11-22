//! Stations > Report Plays in the real `App`, headless: switching it on
//! asks first (only Allow turns it on), switching it off is at once.

use std::cell::RefCell;

use super::*;
use crate::ViewOptions;

fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

// One test only: Slint's platform is set once per thread
#[test]
fn report_plays_is_switched_on_only_by_allow() {
    use Key::{DownArrow, End, Escape, Return, RightArrow, Tab, F10};

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
        .find(|shot| shot.name == "main-stopped")
        .expect("the main window's shot");
    let staged = stage(&made, shot, "dark", &options).expect("the window opens");
    let ui = staged.ui;
    let view = ui.global::<ViewOptions>();
    let changes = Rc::new(RefCell::new(Vec::new()));
    view.on_report_plays_changed({
        let changes = changes.clone();
        move |on| changes.borrow_mut().push(on)
    });
    settle(&time);
    let press_keys = |list: &[Key]| {
        for key in list {
            press(&ui, *key);
            settle(&time);
        }
    };
    // F10, Down: the Open menu; Right: Stations, whose last row is
    // Report Plays
    let switch = [F10, DownArrow, RightArrow, End, Return];

    assert!(!view.get_report_plays(), "off by default");

    // Esc is Don't Allow
    press_keys(&switch);
    assert!(ui.get_dialog_open(), "switching it on asks first");
    assert!(!view.get_report_plays(), "and is off while asking");
    press_keys(&[Escape]);
    assert!(!ui.get_dialog_open());
    assert!(!view.get_report_plays(), "Esc leaves it off");

    // The answers after Learn More: Don't Allow, then Allow
    press_keys(&switch);
    press_keys(&[Tab, Tab, Return]);
    assert!(!ui.get_dialog_open());
    assert!(!view.get_report_plays(), "Don't Allow leaves it off");
    assert!(changes.borrow().is_empty(), "nothing was switched");

    press_keys(&switch);
    press_keys(&[Tab, Tab, Tab, Return]);
    assert!(!ui.get_dialog_open());
    assert!(view.get_report_plays(), "Allow turns it on");
    assert_eq!(*changes.borrow(), [true]);

    // Off needs no dialog
    press_keys(&switch);
    assert!(!ui.get_dialog_open(), "switching it off doesn't ask");
    assert!(!view.get_report_plays());
    assert_eq!(*changes.borrow(), [true, false]);
    ui.hide().expect("the window hides");
}
