//! The Accent Color dialog in a window so short that its body scrolls: a
//! drag in the colour square or the hue bar moves the picker to the end,
//! never the dialog's scroll.

use std::cell::RefCell;

use super::*;

/// Let change handlers run and the short transitions finish
fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

/// Drag with the left button from `from` to `to`, a few pixels at a time
fn drag(ui: &App, time: &Cell<Duration>, from: (f32, f32), to: (f32, f32)) {
    let window = ui.window();
    let button = slint::platform::PointerEventButton::Left;
    let at = |(x, y): (f32, f32)| slint::LogicalPosition::new(x, y);
    window.dispatch_event(WindowEvent::PointerMoved { position: at(from) });
    window.dispatch_event(WindowEvent::PointerPressed {
        position: at(from),
        button,
    });
    const STEPS: u16 = 20;
    for step in 1..=STEPS {
        let t = f32::from(step) / f32::from(STEPS);
        let position = at((from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t));
        time.set(time.get() + Duration::from_millis(16));
        slint::platform::update_timers_and_animations();
        window.dispatch_event(WindowEvent::PointerMoved { position });
    }
    window.dispatch_event(WindowEvent::PointerReleased {
        position: at(to),
        button,
    });
    settle(time);
}

// One test only: Slint's platform is set once per thread
#[test]
fn a_drag_in_the_picker_never_scrolls_the_dialog() {
    let platform = HeadlessPlatform::default();
    let made = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform)).expect("no other Slint platform yet");
    // So short that the dialog's body scrolls
    let options = Options {
        theme: Theme::Dark,
        scale: Some(0.5),
        size: Size::Exact(640.0, 400.0),
        ..Options::default()
    };
    let shot = SHOTS
        .iter()
        .find(|shot| shot.name == "accent-color")
        .expect("the Accent Color shot");
    let ui = stage(&made, shot, "dark", &options)
        .expect("the dialog opens")
        .ui;
    settle(&time);
    let picked = Rc::new(RefCell::new(Vec::<slint::Color>::new()));
    ui.on_accent_color_changed({
        let picked = picked.clone();
        move |color| picked.borrow_mut().push(color)
    });

    // The square spans x 140..500 and y 243..307 here: up from near its
    // bottom to near its top is from dark to bright
    drag(&ui, &time, (320.0, 300.0), (320.0, 248.0));
    let last = *picked.borrow().last().expect("a color was picked");
    let value = last.to_hsva().value;
    assert!(
        value > 0.85,
        "the drag ended near the top, got value {value}"
    );

    // Along the hue bar, drifting down off it: the hue follows to the end
    drag(&ui, &time, (190.0, 336.0), (420.0, 380.0));
    let last = picked.borrow().last().expect("a hue was picked").to_hsva();
    assert!(
        (280.0..296.0).contains(&last.hue),
        "the drag ended at hue 288 (x 420 on a 14..346 track), got {}",
        last.hue
    );
}
