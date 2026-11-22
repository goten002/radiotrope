//! View > Zoom's restart note and the favorites' stats bubble, in the real
//! `App`, headless: the note shows only while another zoom than the
//! window's is picked, and the bubble only for the pointer on the stats.

use super::*;

/// Let change handlers run and the short transitions finish
fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

/// The main window, stopped, on a platform of this test's own (Slint's
/// platform is set once per thread, and each test runs on its own)
fn open() -> (Staged, Rc<Cell<Duration>>) {
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
    settle(&time);
    (staged, time)
}

/// What the window shows now
fn frame(staged: &mut Staged) -> Vec<u8> {
    paint(&staged.window, &mut staged.pixels);
    staged.pixels.as_bytes().to_vec()
}

#[test]
fn the_zoom_note_shows_only_while_another_zoom_is_picked() {
    use Key::{DownArrow, End, Return, RightArrow, UpArrow, F10};

    let (mut staged, time) = open();
    let ui = staged.ui.clone_strong();
    let view = ui.global::<ViewOptions>();
    view.set_zoom(100);
    view.set_zoom_in_use(100);
    let press_keys = |ui: &App, list: &[Key]| {
        for key in list {
            press(ui, *key);
            settle(&time);
        }
    };
    // View is the third menu; Up from its last row (the System Tray group)
    // is Window, and Up again is Zoom. Right unfolds it: 90%, 100%, 115%…
    press_keys(
        &ui,
        &[F10, RightArrow, RightArrow, DownArrow, End, UpArrow, UpArrow],
    );
    press_keys(&ui, &[RightArrow, DownArrow, DownArrow]);
    assert_eq!(view.get_zoom_note(), "", "no note for the zoom in use");
    let before = frame(&mut staged);

    // The zoom in use again: nothing changes, nothing is said
    press_keys(&ui, &[Return]);
    assert_eq!(view.get_zoom(), 100);
    assert_eq!(view.get_zoom_note(), "");
    assert!(
        frame(&mut staged) == before,
        "picking the zoom in use changes nothing on screen"
    );

    // Another one: the note says what a restart does
    press_keys(&ui, &[DownArrow, Return]);
    assert_eq!(view.get_zoom(), 115);
    assert_eq!(view.get_zoom_note(), "Restart Radiotrope to apply 115%");
    assert!(frame(&mut staged) != before);

    // Back to the zoom in use: the change is off, and so is the note
    press_keys(&ui, &[UpArrow, Return]);
    assert_eq!(view.get_zoom(), 100);
    assert_eq!(view.get_zoom_note(), "", "picking it back cancels");
    assert!(frame(&mut staged) == before, "the menu is as it was");
    staged.ui.hide().expect("the window hides");
}

#[test]
fn the_stats_bubble_is_for_the_pointer_on_the_stats_only() {
    let (mut staged, time) = open();
    // Past the hover delay, then past the fade-in that its timer starts
    let wait = |millis: u64| {
        time.set(time.get() + Duration::from_millis(millis));
        settle(&time);
    };
    let hover = |ui: &App, x: f32, y: f32| {
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, y),
        });
        settle(&time);
    };

    // The arrow keys through the favorites: the highlight, no bubble, however
    // long they rest on a row
    for _ in 0..3 {
        press(&staged.ui, Key::DownArrow);
        settle(&time);
        let resting = frame(&mut staged);
        wait(1500);
        assert!(
            frame(&mut staged) == resting,
            "no bubble for the keyboard highlight"
        );
    }

    // The pointer on the row's name: the hover, no bubble
    hover(&staged.ui, 300.0, 275.0);
    let resting = frame(&mut staged);
    wait(1500);
    assert!(
        frame(&mut staged) == resting,
        "no bubble for the rest of the row"
    );

    // On the stats at the right of the row: the bubble, after a moment
    hover(&staged.ui, 750.0, 275.0);
    let resting = frame(&mut staged);
    wait(1500);
    assert!(
        frame(&mut staged) != resting,
        "the bubble for the pointer on the stats"
    );
    staged.ui.hide().expect("the window hides");
}
