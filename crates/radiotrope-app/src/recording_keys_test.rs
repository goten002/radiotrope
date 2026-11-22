//! Recording without a button in the bottom bar, in the real `App`,
//! headless: the R key and Tools > Start Recording start it, and the
//! header's REC chip stops it on a second press.

use super::*;

/// Let change handlers run and the short transitions finish
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

/// Where the REC chip's red dot is: the strongly red pixels in the header
/// (the top quarter of the window) nearest its left edge, the chip being
/// the leftmost red thing there
fn rec_dot(pixels: &SharedPixelBuffer<Rgb8Pixel>) -> Option<(f32, f32)> {
    let width = pixels.width() as usize;
    let header = pixels.height() as usize / 4;
    let red: Vec<(usize, usize)> = pixels.as_slice()[..width * header]
        .iter()
        .enumerate()
        .filter(|(_, pixel)| pixel.r > 180 && pixel.g < 120 && pixel.b < 120)
        .map(|(index, _)| (index % width, index / width))
        .collect();
    let left = red.iter().map(|&(x, _)| x).min()?;
    let dot: Vec<_> = red.iter().filter(|&&(x, _)| x < left + 8).collect();
    let count = dot.len() as f32;
    let x = dot.iter().map(|&&(x, _)| x as f32).sum::<f32>() / count;
    let y = dot.iter().map(|&&(_, y)| y as f32).sum::<f32>() / count;
    Some((x, y))
}

fn click(ui: &App, time: &Cell<Duration>, (x, y): (f32, f32)) {
    let window = ui.window();
    let position = slint::LogicalPosition::new(x, y);
    let button = slint::platform::PointerEventButton::Left;
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
    settle(time);
}

// One test only: Slint's platform is set once per thread
#[test]
fn recording_starts_from_r_and_the_tools_menu_and_stops_from_the_rec_chip() {
    use Key::{DownArrow, Return, RightArrow, F10};

    let platform = HeadlessPlatform::default();
    let made = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform)).expect("no other Slint platform yet");
    let options = Options {
        theme: Theme::Dark,
        scale: Some(1.0),
        ..Options::default()
    };
    let shot = SHOTS
        .iter()
        .find(|shot| shot.name == "main-playing")
        .expect("the main window's shot");
    let Staged {
        ui,
        window,
        mut pixels,
    } = stage(&made, shot, "dark", &options).expect("the window opens");
    settle(&time);
    let toggles = Rc::new(Cell::new(0));
    ui.on_toggle_recording({
        let toggles = toggles.clone();
        move || toggles.set(toggles.get() + 1)
    });

    // The R key
    let r = SharedString::from("r");
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: r.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: r });
    settle(&time);
    assert_eq!(toggles.get(), 1, "R starts a recording");

    // Tools > Start Recording, its fourth row: F10, Down opens the first
    // menu, Right three times is Tools
    keys(
        &ui,
        &time,
        &[
            F10, DownArrow, RightArrow, RightArrow, RightArrow, DownArrow, DownArrow, DownArrow,
            Return,
        ],
    );
    assert_eq!(toggles.get(), 2, "Tools > Start Recording");

    // Recording: the REC chip asks first and stops on the second press
    ui.set_is_recording(true);
    ui.set_recording_text("0:05".into());
    settle(&time);
    paint(&window, &mut pixels);
    let (x, y) = rec_dot(&pixels).expect("the REC chip shows in the header");
    let scale = ui.window().scale_factor();
    let at = (x / scale, y / scale);
    click(&ui, &time, at);
    assert_eq!(toggles.get(), 2, "one press on the REC chip only asks");
    click(&ui, &time, at);
    assert_eq!(toggles.get(), 3, "the second press stops the recording");
}
