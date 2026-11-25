//! The status chip under the station name, in the real `App`, headless
//! (FB12): whatever states came before, a hide to the tray, or a new
//! scale, the chip is as wide as its own text, the same as in a window that
//! opened in that state, and the running time never changes its width.

use super::*;
use crate::StatsData;

/// Let change handlers run and the short transitions finish
fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Ready,
    Connecting,
    Waiting,
    Playing(&'static str),
    Stopped,
    Error,
    Reconnecting,
}

/// The player's state as `player_ui` shows it. The play time stays at its
/// last value after playing ends, as there.
fn set(ui: &App, state: State) {
    ui.set_is_playing(matches!(state, State::Playing(_)));
    ui.set_is_loading(matches!(
        state,
        State::Connecting | State::Waiting | State::Reconnecting
    ));
    ui.set_is_error(state == State::Error);
    ui.set_status_text(
        match state {
            State::Ready => status::READY,
            State::Connecting => status::CONNECTING,
            State::Waiting => status::WAITING_FOR_DATA,
            State::Playing(_) => status::PLAYING,
            State::Stopped => status::STOPPED,
            State::Error => "Can't connect to radio.example",
            State::Reconnecting => status::RECONNECTING,
        }
        .into(),
    );
    if let State::Playing(time) = state {
        ui.global::<StatsData>().set_playtime(time.into());
    }
}

/// The chip's width in logical pixels: from its left edge to its right one,
/// the jumps in colour along a row through the pill above its text
fn chip_width(staged: &mut Staged, scale: f32) -> f32 {
    paint(&staged.window, &mut staged.pixels);
    let width = staged.pixels.width() as usize;
    let px = staged.pixels.as_slice();
    let row = (103.0 * scale) as usize * width;
    let jump = |a: Rgb8Pixel, b: Rgb8Pixel| {
        u32::from(a.r.abs_diff(b.r)) + u32::from(a.g.abs_diff(b.g)) + u32::from(a.b.abs_diff(b.b))
            > 12
    };
    // From the logo's right to the star's left
    let start = (100.0 * scale) as usize;
    let end = width - (140.0 * scale) as usize;
    let edges: Vec<usize> = (start..end)
        .filter(|&x| jump(px[row + x - 1], px[row + x]))
        .collect();
    let (Some(left), Some(right)) = (edges.first(), edges.last()) else {
        panic!("no chip in the header");
    };
    (right - left) as f32 / scale
}

const STATES: [State; 7] = [
    State::Ready,
    State::Connecting,
    State::Waiting,
    State::Playing("2:34"),
    State::Stopped,
    State::Error,
    State::Reconnecting,
];

/// The width a window that opened in `state` gives the chip
fn width_of(state: State, fresh: &[f32]) -> f32 {
    let state = match state {
        State::Playing(_) => State::Playing("2:34"),
        other => other,
    };
    fresh[STATES
        .iter()
        .position(|s| *s == state)
        .expect("a known state")]
}

/// The window as it first opens, at `scale`
fn open(made: &Cell<Option<Rc<MinimalSoftwareWindow>>>, scale: f32) -> Staged {
    let options = Options {
        theme: Theme::Dark,
        scale: Some(scale),
        ..Options::default()
    };
    let shot = SHOTS
        .iter()
        .find(|shot| shot.name == "main-stopped")
        .expect("the main window's shot");
    stage(made, shot, "dark", &options).expect("the window opens")
}

/// One window through `states`: each as wide as in a window new in it
fn walk(staged: &mut Staged, time: &Cell<Duration>, scale: f32, fresh: &[f32], states: &[State]) {
    for &state in states {
        set(&staged.ui, state);
        settle(time);
        let width = chip_width(staged, scale);
        let want = width_of(state, fresh);
        assert!(
            (width - want).abs() <= 1.0,
            "{state:?} at {scale}: the chip is {width}px wide, {want}px in a new window"
        );
    }
}

#[test]
fn the_status_chip_is_as_wide_as_its_own_text() {
    use State::*;

    let platform = HeadlessPlatform::default();
    let made = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform)).expect("no other Slint platform yet");

    // Each state in a window that opened in it
    let fresh = |scale: f32| -> Vec<f32> {
        STATES
            .iter()
            .map(|&state| {
                let mut staged = open(&made, scale);
                set(&staged.ui, state);
                settle(&time);
                let width = chip_width(&mut staged, scale);
                staged.ui.hide().expect("the window hides");
                width
            })
            .collect()
    };
    let at_100 = fresh(1.0);
    let at_125 = fresh(1.25);
    // Sized to the text: a longer text, a wider chip
    let w = |state| width_of(state, &at_100);
    assert!(w(Ready) < w(Stopped));
    assert!(w(Stopped) < w(Playing("2:34")));
    assert!(w(Playing("2:34")) < w(Connecting));
    assert!(w(Connecting) < w(Reconnecting));
    assert!(w(Reconnecting) < w(Waiting));
    assert!(w(Waiting) < w(Error));

    // One window through the sequences that showed it wrong
    let mut staged = open(&made, 1.0);
    let ui = staged.ui.clone_strong();
    walk(
        &mut staged,
        &time,
        1.0,
        &at_100,
        &[
            Connecting,
            Playing("0:01"),
            Playing("0:59"),
            Stopped,
            Playing("2:34"),
            Reconnecting,
            Waiting,
            Stopped,
            Error,
            Ready,
            Connecting,
            Playing("1:11"),
            Stopped,
        ],
    );

    // Hidden to the tray while it reconnects, shown again later. On Wayland
    // a hide destroys the window and FemtoVG dropped its text layouts
    // (FB17). The software renderer here keeps them whatever it is told, so
    // this only walks the states: labels_femtovg_test.rs hides and shows a
    // window that FemtoVG draws.
    set(&ui, Reconnecting);
    settle(&time);
    ui.hide().expect("the window hides");
    let adapter: Rc<dyn slint::platform::WindowAdapter> = staged.window.clone();
    adapter.renderer().set_window_adapter(&adapter);
    for state in [Playing("0:01"), Stopped, Connecting] {
        set(&ui, state);
        settle(&time);
    }
    ui.show().expect("the window shows");
    settle(&time);
    walk(
        &mut staged,
        &time,
        1.0,
        &at_100,
        &[
            Stopped,
            Playing("2:34"),
            Connecting,
            Playing("0:00"),
            Stopped,
            Ready,
        ],
    );

    // Another zoom while playing, then on through the states
    set(&ui, Playing("2:34"));
    settle(&time);
    ui.window()
        .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.25 });
    let size = slint::PhysicalSize::new(1000, 750);
    staged.window.set_size(size);
    staged.pixels = SharedPixelBuffer::new(size.width, size.height);
    settle(&time);
    walk(
        &mut staged,
        &time,
        1.25,
        &at_125,
        &[
            Playing("2:35"),
            Stopped,
            Connecting,
            Playing("0:01"),
            Reconnecting,
            Stopped,
        ],
    );
    staged.ui.hide().expect("the window hides");
}
