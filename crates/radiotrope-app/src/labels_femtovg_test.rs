//! The header's labels in the real `App`, drawn by FemtoVG (the desktop
//! app's renderer) with no screen (FB17): every chip shows its whole text
//! and is as wide as that text, at every scale, in both themes, whatever
//! came before: other states, a hide to the tray and back, a new scale.
//!
//! Two checks that don't rest on each other:
//! - what is drawn: the status text's ink in its chip is as wide as the
//!   same text drawn alone with nothing to cut it (a text cut short or
//!   ending in "…" has less ink);
//! - what is laid out: the chips of a window that was hidden and shown
//!   again before its texts changed sit where they do in a window that
//!   never was.
//!
//! `cargo test -p radiotrope-app --bin radiotrope labels` runs the quick
//! set; the whole matrix, which also saves pictures of every state, is
//! `RADIOTROPE_LABEL_SHOTS=<dir> cargo test -p radiotrope-app --bin
//! radiotrope labels -- --ignored`. Without EGL the tests skip (and say
//! so), unless `RADIOTROPE_REQUIRE_FEMTOVG` is set, as in CI.

use std::path::PathBuf;

use super::*;
use crate::headless_femtovg::{self, FemtoPlatform, FemtoWindow};
use crate::StatsData;

slint::slint! {
    #[edition = "1.19"]
    // One line of a chip's text (Theme.font-sm, semibold) with nothing
    // around it that could cut it
    export component LabelProbe inherits Window {
        in property <string> label;
        background: #202020;
        Text {
            x: 20px;
            y: 10px;
            text: root.label;
            color: white;
            font-family: "Inter 18pt";
            font-size: 12px;
            font-weight: 600;
        }
    }
}

impl Canvas for FemtoWindow {
    fn resize(&self, size: LogicalSize) {
        slint::platform::WindowAdapter::set_size(self, size.into());
    }

    fn pixel_size(&self) -> slint::PhysicalSize {
        slint::platform::WindowAdapter::size(self)
    }

    fn paint_into(&self, pixels: &mut SharedPixelBuffer<Rgb8Pixel>) {
        let frame = self.snapshot().expect("FemtoVG draws the window");
        let mut out = SharedPixelBuffer::<Rgb8Pixel>::new(frame.width(), frame.height());
        for (to, from) in out.make_mut_slice().iter_mut().zip(frame.as_slice()) {
            *to = Rgb8Pixel {
                r: from.r,
                g: from.g,
                b: from.b,
            };
        }
        *pixels = out;
    }
}

type Shown = Staged<FemtoWindow>;

/// The scales the owner's displays and the View > Zoom choices give
const DISPLAY_SCALES: [f32; 6] = [1.0, 1.1, 1.25, 1.5, 1.75, 2.0];
/// Display scales times the zooms (90, 115, 130 and 150%) that give a
/// scale not listed above
const ZOOMED_SCALES: [f32; 7] = [0.9, 1.15, 1.3, 1.4375, 1.625, 1.875, 2.25];

/// The chips' row in the header, in logical pixels from the window's top:
/// a line through the pills above their text, and the lines their text is on
const PILL_ROW: f32 = 102.5;
const TEXT_ROWS: std::ops::Range<i32> = 104..117;
/// From a chip's left edge to its text, past the dot or the icon
const BEFORE_TEXT: f32 = 17.0;
/// A chip's padding, left and right together
const PADDING: f32 = 16.0;

#[derive(Clone, Copy)]
struct Setup {
    scale: f32,
    theme: &'static str,
    size: (f32, f32),
    long_name: bool,
}

impl Setup {
    fn name(&self) -> String {
        format!(
            "{}-{}x{}-{}{}",
            self.theme,
            self.size.0,
            self.size.1,
            self.scale,
            if self.long_name { "-long-name" } else { "" }
        )
    }

    fn options(&self) -> Options {
        Options {
            theme: Theme::Dark,
            scale: Some(self.scale),
            size: Size::Exact(self.size.0, self.size.1),
            ..Options::default()
        }
    }
}

/// What the status chip shows: the text, playing, loading, error, and the
/// time on the station
#[derive(Clone, Copy, PartialEq, Debug)]
struct Status {
    text: &'static str,
    playing: bool,
    loading: bool,
    error: bool,
    time: &'static str,
}

impl Status {
    const fn new(text: &'static str) -> Self {
        Self {
            text,
            playing: false,
            loading: false,
            error: false,
            time: "",
        }
    }

    const fn loading(text: &'static str) -> Self {
        Self {
            loading: true,
            ..Self::new(text)
        }
    }

    const fn error(text: &'static str) -> Self {
        Self {
            error: true,
            ..Self::new(text)
        }
    }

    const fn playing(time: &'static str) -> Self {
        Self {
            playing: true,
            time,
            ..Self::new(status::PLAYING)
        }
    }

    /// The chip's text
    fn label(&self) -> String {
        if self.playing {
            format!("{} {}", self.text, self.time)
        } else {
            self.text.to_string()
        }
    }

    fn show(&self, ui: &App) {
        ui.set_is_playing(self.playing);
        ui.set_is_loading(self.loading);
        ui.set_is_error(self.error);
        ui.set_status_text(self.text.into());
        if self.playing {
            ui.global::<StatsData>().set_playtime(self.time.into());
        }
    }
}

const READY: Status = Status::new(status::READY);
const CONNECTING: Status = Status::loading(status::CONNECTING);
const WAITING: Status = Status::loading(status::WAITING_FOR_DATA);
const RECONNECTING: Status = Status::loading(status::RECONNECTING);
const PLAYING: Status = Status::playing("2:34");
const PLAYING_LONG: Status = Status::playing("1:11:11");
const STOPPED: Status = Status::new(status::STOPPED);
const CANT_PLAY: Status = Status::error("Can't play this stream");
const CANT_CONNECT: Status = Status::error("Can't connect to radio.example");

const STATUSES: [Status; 9] = [
    READY,
    CONNECTING,
    WAITING,
    RECONNECTING,
    PLAYING,
    PLAYING_LONG,
    STOPPED,
    CANT_PLAY,
    CANT_CONNECT,
];

/// What happens to one window, in order
#[derive(Clone, Copy)]
enum Step {
    /// The player's state changes, and the chip is looked at
    Show(Status),
    /// The state changes while the window is hidden
    Unseen(Status),
    /// To the tray (on Wayland the window is destroyed)
    Hide,
    /// Back from the tray
    Unhide,
}

/// The sequences that showed the chip wrong, and then some
const WALK: [Step; 27] = [
    Step::Show(CONNECTING),
    Step::Show(PLAYING),
    Step::Show(STOPPED),
    Step::Show(CANT_PLAY),
    Step::Show(PLAYING),
    // Hidden while it plays, back, and a new station
    Step::Hide,
    Step::Unhide,
    Step::Show(CONNECTING),
    Step::Show(PLAYING),
    Step::Show(STOPPED),
    Step::Show(CANT_PLAY),
    Step::Show(READY),
    // Everything happens while it is in the tray
    Step::Show(PLAYING_LONG),
    Step::Hide,
    Step::Unseen(RECONNECTING),
    Step::Unseen(PLAYING),
    Step::Unseen(STOPPED),
    Step::Unhide,
    Step::Show(STOPPED),
    Step::Show(WAITING),
    Step::Show(CANT_CONNECT),
    Step::Hide,
    Step::Unseen(CONNECTING),
    Step::Unhide,
    Step::Show(CONNECTING),
    Step::Show(RECONNECTING),
    Step::Show(PLAYING),
];

/// The header's shots with other chips than the status: REC, the Sleep
/// Timer, the Scheduler's label, the notices and the save error
const CHIP_SHOTS: [&str; 14] = [
    "main-playing",
    "main-recording",
    "main-recording-confirm",
    "main-sleep-countdown",
    "main-schedule-countdown",
    "main-schedule-started",
    "main-alarm",
    "main-recording-saved",
    "main-recording-error",
    "main-notice-missed",
    "main-save-error",
    "main-chips",
    "main-chips-crowded",
    "main-error-long",
];

const LONG_NAME: &str = "Harbour Jazz and Blues: The Late Night Session from the North Atlantic";

/// Let change handlers run and the short transitions finish
fn settle(time: &Cell<Duration>) {
    slint::platform::update_timers_and_animations();
    time.set(time.get() + Duration::from_millis(350));
    slint::platform::update_timers_and_animations();
}

fn difference(a: Rgb8Pixel, b: Rgb8Pixel) -> u32 {
    u32::from(a.r.abs_diff(b.r)) + u32::from(a.g.abs_diff(b.g)) + u32::from(a.b.abs_diff(b.b))
}

/// The frame FemtoVG drew
struct Frame {
    pixels: SharedPixelBuffer<Rgb8Pixel>,
    scale: f32,
}

impl Frame {
    fn of(window: &mut Shown, scale: f32) -> Self {
        window.window.paint_into(&mut window.pixels);
        Self {
            pixels: window.pixels.clone(),
            scale,
        }
    }

    fn at(&self, x: usize, y: usize) -> Rgb8Pixel {
        self.pixels.as_slice()[y * self.pixels.width() as usize + x]
    }

    fn px(&self, logical: f32) -> usize {
        (logical * self.scale).round() as usize
    }

    /// The left and right edges of the pills in the chips' row, in pixels:
    /// the jumps in colour along a line above their text. The two lines of
    /// a chip's border count as one edge.
    fn chip_edges(&self) -> Vec<usize> {
        let row = self.px(PILL_ROW);
        // From the logo's right to the star's left
        let start = self.px(100.0);
        let end = self.pixels.width() as usize - self.px(140.0);
        let near = self.px(2.5).max(2);
        let mut edges: Vec<usize> = Vec::new();
        for x in start..end {
            if difference(self.at(x - 1, row), self.at(x, row)) > 12 {
                match edges.last_mut() {
                    Some(last) if x - *last <= near => *last = x,
                    _ => edges.push(x),
                }
            }
        }
        edges
    }

    /// The first and the last column with text in them between `left` and
    /// `right`, in the rows `rows` (pixels): those that differ from the
    /// background at least a third as much as the text's own colour does
    fn ink(
        &self,
        left: usize,
        right: usize,
        rows: std::ops::Range<usize>,
        background: Rgb8Pixel,
    ) -> Option<(usize, usize)> {
        let strongest = rows
            .clone()
            .flat_map(|y| (left..right).map(move |x| (x, y)))
            .map(|(x, y)| difference(self.at(x, y), background))
            .max()?;
        if strongest < 60 {
            return None;
        }
        let inked = |x: usize| {
            rows.clone()
                .any(|y| difference(self.at(x, y), background) * 3 >= strongest)
        };
        let first = (left..right).find(|&x| inked(x))?;
        let last = (left..right).rev().find(|&x| inked(x))?;
        Some((first, last))
    }

    /// How wide the text of the chip between `left` and `right` is drawn,
    /// in logical pixels
    fn chip_ink(&self, left: usize, right: usize) -> Option<f32> {
        let rows = self.px(TEXT_ROWS.start as f32)..self.px(TEXT_ROWS.end as f32);
        let background = self.at(left + self.px(4.0), self.px(PILL_ROW));
        let (first, last) = self.ink(
            left + self.px(BEFORE_TEXT),
            right - self.px(2.0),
            rows,
            background,
        )?;
        Some((last - first + 1) as f32 / self.scale)
    }

    /// The header, for a person to look at
    fn save(&self, dir: Option<&PathBuf>, name: &str) {
        let Some(dir) = dir else { return };
        let width = self.pixels.width();
        let height = (self.px(190.0) as u32).min(self.pixels.height());
        let bytes = &self.pixels.as_bytes()[..(width * height * 3) as usize];
        image::save_buffer(
            dir.join(format!("{name}.png")),
            bytes,
            width,
            height,
            image::ColorType::Rgb8,
        )
        .expect("the picture is saved");
    }
}

/// Draws a text alone, to know how wide FemtoVG draws all of it
struct Probe {
    ui: LabelProbe,
    window: Rc<FemtoWindow>,
    scale: f32,
}

impl Probe {
    fn new(made: &Cell<Option<Rc<FemtoWindow>>>, scale: f32) -> Self {
        let ui = LabelProbe::new().expect("the probe's window");
        let window = made.take().expect("Slint made a window");
        ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        window.resize(LogicalSize::new(700.0, 40.0));
        ui.show().expect("the probe shows");
        Self { ui, window, scale }
    }

    /// The width of `text`'s ink in logical pixels
    fn ink(&self, text: &str) -> f32 {
        self.ui.set_label(text.into());
        let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(1, 1);
        self.window.paint_into(&mut pixels);
        let frame = Frame {
            pixels,
            scale: self.scale,
        };
        let background = frame.at(2, 2);
        let rows = 0..frame.pixels.height() as usize;
        let (first, last) = frame
            .ink(0, frame.pixels.width() as usize, rows, background)
            .unwrap_or_else(|| panic!("nothing drawn for {text:?}"));
        (last - first + 1) as f32 / self.scale
    }
}

struct Check<'a> {
    made: &'a Cell<Option<Rc<FemtoWindow>>>,
    time: &'a Cell<Duration>,
    setup: Setup,
    shots: Option<PathBuf>,
    failures: Vec<String>,
}

impl Check<'_> {
    /// A new window with `shot` in it
    fn open(&self, shot: &str) -> Shown {
        let shot = SHOTS
            .iter()
            .find(|candidate| candidate.name == shot)
            .unwrap_or_else(|| panic!("no shot {shot}"));
        let window = stage(self.made, shot, self.setup.theme, &self.setup.options())
            .expect("the window opens");
        if self.setup.long_name {
            window.ui.set_station_name(LONG_NAME.into());
        }
        settle(self.time);
        window
    }

    fn fail(&mut self, what: String) {
        self.failures.push(format!("{}: {what}", self.setup.name()));
    }

    /// The status chip of `frame` shows all of `status`, as wide as `fresh`
    /// (a window that opened in that state) has it
    fn status_chip(
        &mut self,
        frame: &Frame,
        status: Status,
        probe: &Probe,
        fresh: Option<&[usize]>,
        step: &str,
    ) {
        let label = status.label();
        let edges = frame.chip_edges();
        let &[left, right, ..] = edges.as_slice() else {
            self.fail(format!("{step}: no status chip for {label:?}"));
            return;
        };
        let whole = probe.ink(&label);
        // Two device pixels: where a glyph's edge falls on the pixel grid
        let slack = 2.0 / frame.scale;
        match frame.chip_ink(left, right) {
            Some(drawn) if (drawn - whole).abs() <= slack => {}
            drawn => self.fail(format!(
                "{step}: {label:?} is drawn {drawn:?}px wide, {whole}px when whole"
            )),
        }
        // As wide as its text: the padding and the dot. Only the running
        // time keeps more, for digits wider than the ones it shows ("1" is
        // some 4px narrower than "4" at this size). The ink is a few
        // pixels narrower than the room a text takes.
        let width = (right - left) as f32 / frame.scale;
        let least = PADDING + 10.0 + whole;
        let digits = status.time.chars().filter(char::is_ascii_digit).count();
        let most = least + 5.0 + 4.0 * digits as f32;
        if width < least - slack || width > most {
            self.fail(format!(
                "{step}: the chip of {label:?} is {width}px wide, its text needs {least}px"
            ));
        }
        if let Some(&[fresh_left, fresh_right, ..]) = fresh {
            if left.abs_diff(fresh_left) > 1 || right.abs_diff(fresh_right) > 1 {
                self.fail(format!(
                    "{step}: the chip of {label:?} is at {left}..{right}, \
                     {fresh_left}..{fresh_right} in a new window"
                ));
            }
        }
    }

    /// Every state in a window that opened in it, then one window through
    /// [`WALK`], then a new scale in that window
    fn status(&mut self, next_scale: f32) {
        let scale = self.setup.scale;
        let probe = Probe::new(self.made, scale);
        let mut fresh = Vec::new();
        for status in STATUSES {
            let mut window = self.open("main-stopped");
            status.show(&window.ui);
            settle(self.time);
            let frame = Frame::of(&mut window, scale);
            frame.save(
                self.shots.as_ref(),
                &format!("{}-status-{}", self.setup.name(), fresh.len()),
            );
            self.status_chip(&frame, status, &probe, None, "new window");
            fresh.push(frame.chip_edges());
            window.ui.hide().expect("the window hides");
        }
        let fresh_of = |status: Status| {
            let index = STATUSES
                .iter()
                .position(|known| *known == status)
                .expect("a known status");
            fresh[index].as_slice()
        };

        let mut window = self.open("main-stopped");
        let ui = window.ui.clone_strong();
        for (index, step) in WALK.into_iter().enumerate() {
            match step {
                Step::Show(status) => {
                    status.show(&ui);
                    settle(self.time);
                    let frame = Frame::of(&mut window, scale);
                    frame.save(
                        self.shots.as_ref(),
                        &format!("{}-walk-{index:02}", self.setup.name()),
                    );
                    let step = format!("step {index}");
                    self.status_chip(&frame, status, &probe, Some(fresh_of(status)), &step);
                }
                Step::Unseen(status) => {
                    status.show(&ui);
                    settle(self.time);
                }
                Step::Hide => window.window.suspend().expect("the renderer suspends"),
                Step::Unhide => window.window.resume().expect("the renderer resumes"),
            }
        }

        // The display's scale changes under the window (another monitor,
        // or Wayland telling the scale after the window is up)
        let probe = Probe::new(self.made, next_scale);
        ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: next_scale,
        });
        window
            .window
            .resize(LogicalSize::new(self.setup.size.0, self.setup.size.1));
        settle(self.time);
        for status in [PLAYING, CONNECTING, STOPPED, CANT_PLAY, PLAYING_LONG] {
            status.show(&ui);
            settle(self.time);
            let frame = Frame::of(&mut window, next_scale);
            let step = format!("scale {scale} to {next_scale}");
            self.status_chip(&frame, status, &probe, None, &step);
        }
        ui.hide().expect("the window hides");
    }

    /// Each of [`CHIP_SHOTS`]: the chips of a window hidden and shown again
    /// before its texts change are where they are in one that never was
    fn chips(&mut self) {
        let scale = self.setup.scale;
        for shot in CHIP_SHOTS {
            let mut reference = self.open(shot);
            new_texts(&reference.ui);
            settle(self.time);
            let expected = Frame::of(&mut reference, scale);
            reference.ui.hide().expect("the window hides");

            let mut window = self.open(shot);
            let before = Frame::of(&mut window, scale);
            before.save(
                self.shots.as_ref(),
                &format!("{}-{shot}", self.setup.name()),
            );
            window.window.suspend().expect("the renderer suspends");
            window.window.resume().expect("the renderer resumes");
            new_texts(&window.ui);
            settle(self.time);
            let after = Frame::of(&mut window, scale);
            after.save(
                self.shots.as_ref(),
                &format!("{}-{shot}-new-texts", self.setup.name()),
            );
            window.ui.hide().expect("the window hides");

            let (want, got) = (expected.chip_edges(), after.chip_edges());
            let same =
                want.len() == got.len() && want.iter().zip(&got).all(|(a, b)| a.abs_diff(*b) <= 1);
            if !same {
                self.fail(format!(
                    "{shot}: after a hide and new texts the chips' edges are {got:?}, \
                     {want:?} in a window that was never hidden"
                ));
            }
            if before.chip_edges() == got {
                self.fail(format!(
                    "{shot}: the chips did not move with their new texts ({got:?})"
                ));
            }
        }
    }
}

/// Other texts, longer or shorter, in every chip the header shows
fn new_texts(ui: &App) {
    if ui.get_is_playing() {
        ui.global::<StatsData>().set_playtime("1:24:18".into());
    } else if ui.get_is_error() {
        ui.set_status_text("Can't play this stream".into());
    }
    if ui.get_is_recording() {
        ui.set_recording_text("1:02:36".into());
        ui.set_recording_size("112.4 MB".into());
    }
    if !ui.get_sleep_left().is_empty() {
        ui.set_sleep_left("1:18:42".into());
    }
    if !ui.get_schedule_label().is_empty() {
        let label = ui.get_schedule_label();
        ui.set_schedule_label(if label.starts_with("Record in") {
            "Record in 1:23:45".into()
        } else {
            slint::format!("{label} Tonight")
        });
    }
    if !ui.get_recording_notice().is_empty() {
        ui.set_recording_notice(
            match ui.get_recording_notice_kind().as_str() {
                "saved" => "Saved Dawn - 07-00.mp3",
                "scheduler" => "Missed 19:30: Record North Coast FM",
                _ => "Recording failed: disk full",
            }
            .into(),
        );
    }
    if !ui.get_save_error().is_empty() {
        ui.set_save_error("Can't save settings".into());
    }
}

/// Run `setups`, each to the next one's scale at its end, and fail with
/// everything that was wrong
fn run(setups: &[Setup]) {
    if let Err(error) = headless_femtovg::available() {
        if std::env::var_os("RADIOTROPE_REQUIRE_FEMTOVG").is_some() {
            panic!("FemtoVG can't draw here: {error}");
        }
        eprintln!("skipped: no FemtoVG without a display ({error})");
        return;
    }
    let platform = FemtoPlatform::default();
    let made = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform)).expect("no other Slint platform yet");
    let shots = std::env::var_os("RADIOTROPE_LABEL_SHOTS").map(PathBuf::from);
    if let Some(dir) = &shots {
        std::fs::create_dir_all(dir).expect("the pictures' folder");
    }

    let mut failures = Vec::new();
    for (index, &setup) in setups.iter().enumerate() {
        let next_scale = setups[(index + 1) % setups.len()].scale;
        let mut check = Check {
            made: &made,
            time: &time,
            setup,
            shots: shots.clone(),
            failures: Vec::new(),
        };
        check.status(next_scale);
        check.chips();
        failures.append(&mut check.failures);
    }
    assert!(
        failures.is_empty(),
        "{} label problems:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

const SMALL: (f32, f32) = (640.0, 480.0);
const LARGE: (f32, f32) = (1200.0, 800.0);

/// Every display scale, in the smallest window, where the least fits
#[test]
fn labels_are_whole_at_every_display_scale() {
    let setups: Vec<Setup> = DISPLAY_SCALES
        .into_iter()
        .map(|scale| Setup {
            scale,
            theme: "dark",
            size: SMALL,
            long_name: false,
        })
        .collect();
    run(&setups);
}

/// The scales the zooms give, the light theme, a large window and a long
/// station name
#[test]
fn labels_are_whole_with_zoom_themes_and_sizes() {
    let mut setups: Vec<Setup> = [0.9, 1.15, 1.3]
        .into_iter()
        .map(|scale| Setup {
            scale,
            theme: "light",
            size: SMALL,
            long_name: true,
        })
        .collect();
    setups.extend([1.0, 1.25].map(|scale| Setup {
        scale,
        theme: "light",
        size: LARGE,
        long_name: false,
    }));
    setups.push(Setup {
        scale: 1.625,
        theme: "dark",
        size: LARGE,
        long_name: true,
    });
    run(&setups);
}

/// Everything: every scale, both themes, both sizes, short and long names.
/// Slow; run it before a change to the chips or to the renderer is merged.
#[test]
#[ignore = "the whole matrix takes minutes; see the module's comment"]
fn labels_are_whole_in_the_whole_matrix() {
    let mut setups = Vec::new();
    for scale in DISPLAY_SCALES.into_iter().chain(ZOOMED_SCALES) {
        for theme in ["dark", "light"] {
            for size in [SMALL, LARGE] {
                setups.push(Setup {
                    scale,
                    theme,
                    size,
                    long_name: theme == "light",
                });
            }
        }
    }
    run(&setups);
}
