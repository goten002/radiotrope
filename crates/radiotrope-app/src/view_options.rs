//! View > Zoom and Stations > Report Plays
//!
//! The zoom is the window's scale: Slint takes one for the whole run, so
//! it is set before the window is made and a new choice applies at the
//! next start.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use slint::ComponentHandle;

use radiotrope_app::data::settings::{nearest_zoom, Settings};

use crate::{App, ViewOptions};

/// Slint's own setting for the window's scale, read when the window is made
const SCALE_VAR: &str = "SLINT_SCALE_FACTOR";

/// The zoom this run started with, in percent
static ZOOM_IN_USE: AtomicU32 = AtomicU32::new(100);

/// Plays of directory stations are reported (the menu's switch)
static REPORT_PLAYS: AtomicBool = AtomicBool::new(true);

/// How often the display's own scale is looked at (see [`watch_display_scale`])
#[cfg(feature = "desktop")]
const SCALE_CHECK: std::time::Duration = std::time::Duration::from_secs(10);

/// The scale to make the window with for a zoom of `percent`: the
/// display's own scale times the zoom. `None` at 100%, where the window
/// follows the display as it always did.
///
/// `set` is a scale the user set themselves (`SLINT_SCALE_FACTOR`), which
/// stands in for the display's; `seen` is the display's scale as the last
/// run saw it (it can't be asked for before there is a window).
fn window_scale(percent: u32, set: Option<f32>, seen: Option<f32>) -> Option<f32> {
    if percent == 100 {
        return None;
    }
    let display = set
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .or(seen)
        .unwrap_or(1.0);
    Some(display * percent as f32 / 100.0)
}

/// Apply the saved zoom to the window about to be made. Call before the
/// window is created and before other threads are started (it sets an
/// environment variable).
pub(crate) fn apply_zoom() {
    let settings = Settings::load().unwrap_or_default();
    let percent = settings.zoom_percent();
    let set = std::env::var(SCALE_VAR)
        .ok()
        .and_then(|scale| scale.parse::<f32>().ok());
    let Some(scale) = window_scale(percent, set, settings.known_display_scale()) else {
        return;
    };
    ZOOM_IN_USE.store(percent, Ordering::Relaxed);
    std::env::set_var(SCALE_VAR, format!("{scale}"));
}

/// The zoom this run's window was made with, in percent
pub(crate) fn zoom_in_use() -> u32 {
    ZOOM_IN_USE.load(Ordering::Relaxed)
}

/// A window size in this run's logical pixels, in those of the next start:
/// with another zoom picked the window keeps its size on screen
pub(crate) fn size_at_zoom(size: (u32, u32), chosen: u32) -> (u32, u32) {
    let in_use = zoom_in_use();
    if chosen == in_use || chosen == 0 {
        return size;
    }
    let convert = |n: u32| {
        ((n as f32) * in_use as f32 / chosen as f32)
            .round()
            .max(1.0) as u32
    };
    (convert(size.0), convert(size.1))
}

/// Whether a play of a directory station is reported to the directory
pub(crate) fn report_plays() -> bool {
    REPORT_PLAYS.load(Ordering::Relaxed)
}

pub(crate) fn set_report_plays(on: bool) {
    REPORT_PLAYS.store(on, Ordering::Relaxed);
}

/// Show the saved choices in the window, and keep them in step with it
pub(crate) fn setup(ui: &App, settings: &Settings) {
    let options = ui.global::<ViewOptions>();
    options.set_zoom(settings.zoom_percent() as i32);
    options.set_zoom_in_use(zoom_in_use() as i32);

    set_report_plays(settings.report_plays);
    options.set_report_plays(settings.report_plays);
    options.on_report_plays_changed(set_report_plays);

    #[cfg(feature = "desktop")]
    watch_display_scale(ui, settings.known_display_scale());
}

/// The zoom picked in the menu, in percent
pub(crate) fn zoom_choice(ui: &App) -> u32 {
    nearest_zoom(ui.global::<ViewOptions>().get_zoom().max(0) as u32)
}

/// The menu's Report Plays switch
pub(crate) fn report_plays_choice(ui: &App) -> bool {
    ui.global::<ViewOptions>().get_report_plays()
}

/// Keep the display's own scale in settings.json for the next start's
/// zoom. It is looked at now and then, not once: Wayland tells the scale
/// after the window is up, a window started in the tray has none yet, and
/// the window can move to another display.
#[cfg(feature = "desktop")]
fn watch_display_scale(ui: &App, saved: Option<f32>) {
    use slint::winit_030::WinitWindowAccessor;

    let ui_weak = ui.as_weak();
    let mut known = saved;
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, SCALE_CHECK, move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        // winit's scale is the display's, whatever scale Slint draws at
        let Some(scale) = ui
            .window()
            .with_winit_window(|window| window.scale_factor() as f32)
            .filter(|scale| scale.is_finite() && *scale > 0.0)
        else {
            return;
        };
        if known.is_some_and(|known| (known - scale).abs() < 0.005) {
            return;
        }
        known = Some(scale);
        crate::window_settings::update_settings(|settings| settings.display_scale = Some(scale));
    });
    // Runs for as long as the app does
    std::mem::forget(timer);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zoom_multiplies_the_displays_scale() {
        assert_eq!(
            window_scale(100, None, Some(2.0)),
            None,
            "100% is left alone"
        );
        assert_eq!(window_scale(100, Some(1.5), None), None);
        assert_eq!(window_scale(150, None, None), Some(1.5));
        assert_eq!(window_scale(150, None, Some(2.0)), Some(3.0));
        assert_eq!(window_scale(90, None, Some(1.25)), Some(1.125));
        // A scale the user set stands in for the display's
        assert_eq!(window_scale(130, Some(2.0), Some(1.0)), Some(2.6));
        assert_eq!(window_scale(130, Some(0.0), Some(1.0)), Some(1.3));
    }

    #[test]
    fn a_window_keeps_its_size_on_screen_when_the_zoom_changes() {
        // This run is at 100%
        assert_eq!(size_at_zoom((800, 600), 100), (800, 600));
        assert_eq!(size_at_zoom((800, 600), 150), (533, 400));
        assert_eq!(size_at_zoom((900, 600), 90), (1000, 667));
    }
}
