//! Opens every screen of the snapshot tool in the real `App`, headless:
//! each dialog must count as open (the menus are then off and the window's
//! shortcuts wait) and Esc must close it again.

use super::*;

/// Known bugs, left to the package that owns the dialog. Remove a shot
/// here once it is fixed: the test fails while a fixed shot is listed.
///
/// - The pairing overlays: once one closes, nothing in the Remote Control
///   dialog has the keyboard focus, so Esc does nothing until a click.
const KNOWN_FAILURES: &[&str] = &["remote-pairing-code", "remote-pairing-qr"];

/// Let change handlers run and the short transitions finish
fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

// One test only: Slint's platform is set once per thread, and the window
// it makes is handed over through the platform
#[test]
fn every_dialog_opens_and_esc_closes_it() {
    let platform = HeadlessPlatform::default();
    let made = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform)).expect("no other Slint platform yet");
    // A quarter of the pixels to draw: the layout is the same
    let options = Options {
        theme: Theme::Dark,
        scale: Some(0.5),
        ..Options::default()
    };

    let mut failures = Vec::new();
    let mut known = Vec::new();
    for shot in SHOTS {
        let before = failures.len();
        let staged = stage(&made, shot, "dark", &options)
            .unwrap_or_else(|error| panic!("{}: {error}", shot.name));
        let ui = staged.ui;
        settle(&time);
        let open = ui.get_dialog_open();
        if open != shot.is_dialog() {
            failures.push(format!("{}: dialog-open is {open} once shown", shot.name));
        } else if open {
            let escapes = shot.escapes();
            for press_number in 1..=escapes {
                press(&ui, Key::Escape);
                settle(&time);
                let open = ui.get_dialog_open();
                if open != (press_number < escapes) {
                    failures.push(format!(
                        "{}: dialog-open is {open} after Esc {press_number} of {escapes}",
                        shot.name
                    ));
                    break;
                }
            }
        }
        ui.hide().expect("the window hides");
        if KNOWN_FAILURES.contains(&shot.name) {
            if failures.len() == before {
                failures.push(format!(
                    "{}: works now, take it off KNOWN_FAILURES",
                    shot.name
                ));
            } else {
                known.extend(failures.drain(before..));
            }
        }
    }
    for failure in &known {
        eprintln!("known bug: {failure}");
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
