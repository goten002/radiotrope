//! The visualizer: the timer that feeds it the engine's levels

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use radiotrope::audio::AudioAnalysis;
use radiotrope_app::visual;
use radiotrope_app::visual::{gate, LevelSmoother};

use crate::{App, VizData};

/// Start the visualization timer, about 30 frames a second. It runs only
/// while there is something to show (see `viz_wanted`): it stops itself,
/// and the poll or `live_views_changed` start it again.
pub(crate) fn start_timer(
    ui: &App,
    analysis_rx: crossbeam_channel::Receiver<Arc<Mutex<AudioAnalysis>>>,
) -> std::rc::Rc<slint::Timer> {
    let viz_timer = std::rc::Rc::new(slint::Timer::default());
    {
        let mut analysis: Option<Arc<Mutex<AudioAnalysis>>> = None;
        let ui_weak = ui.as_weak();
        let bands = radiotrope::config::audio::SPECTRUM_BANDS;
        // Pre-allocate the model once; update in place each tick
        let spectrum_model = std::rc::Rc::new(VecModel::from(vec![0.0f32; bands]));
        let viz = ui.global::<VizData>();
        viz.set_spectrum(ModelRc::from(spectrum_model.clone()));
        let mut spectrum_smooth = LevelSmoother::new(
            bands,
            visual::SPECTRUM_RISE_SECS,
            visual::SPECTRUM_FALL_SECS,
        );
        let mut vu_smooth = LevelSmoother::new(2, visual::VU_RISE_SECS, visual::VU_FALL_SECS);
        // Dot Matrix columns and peaks
        let extras = VizExtras::new(&viz);
        let mut peak_hold = visual::PeakHold::new(visual::MATRIX_COLUMNS);
        let mut shown_mode = slint::SharedString::default();
        let mut gated = vec![0.0f32; bands];
        let mut idle = true;
        let mut last_frame = Instant::now();
        let this = std::rc::Rc::downgrade(&viz_timer);
        viz_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(33),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let viz = ui.global::<VizData>();
                // Seconds since the last frame, capped so a stall doesn't
                // make the bars jump
                let now = Instant::now();
                let dt = now.duration_since(last_frame).as_secs_f32().min(0.1);
                last_frame = now;
                // Nothing to show: zero out once, and stop until wanted
                if !viz_wanted(&ui) {
                    if !idle {
                        idle = true;
                        viz.set_active(false);
                        spectrum_smooth.reset();
                        vu_smooth.reset();
                        gated.fill(0.0);
                        show_viz_frame(&viz, &[0.0, 0.0], &gated, &spectrum_model);
                        peak_hold.reset();
                        extras.clear();
                    }
                    if let Some(timer) = this.upgrade() {
                        timer.stop();
                    }
                    return;
                }
                if analysis.is_none() {
                    analysis = analysis_rx.try_recv().ok();
                }
                let Some(analysis) = &analysis else { return };
                if idle {
                    idle = false;
                    viz.set_active(true);
                }
                // try_lock: skip this tick if engine/analyzer holds the lock
                let Ok(mut a) = analysis.try_lock() else {
                    return;
                };
                // On screen: keep the analyzer working it out (it stops
                // soon after nobody shows it)
                a.mark_shown();
                let (vu_l, vu_r, spectrum) = (a.vu_left, a.vu_right, a.spectrum);
                drop(a);
                for (g, &level) in gated.iter_mut().zip(spectrum.iter()) {
                    *g = gate(level);
                }
                let vu = vu_smooth.update(&[vu_l, vu_r], dt).to_vec();
                let smooth = spectrum_smooth.update(&gated, dt);
                show_viz_frame(&viz, &vu, smooth, &spectrum_model);
                // The Dot Matrix data is only worked out while it shows,
                // starting fresh each time it is picked
                let mode = ui.get_viz_mode();
                if mode != shown_mode {
                    peak_hold.reset();
                    extras.clear();
                    shown_mode = mode.clone();
                }
                if mode == "dots" {
                    let cols = visual::column_levels(smooth, visual::MATRIX_COLUMNS);
                    let peaks = peak_hold.update(&cols, dt).to_vec();
                    extras.show_columns(&cols, &peaks);
                }
            },
        );
    }
    viz_timer
}

/// Whether the visualizer has something to show: a station playing, the
/// visualizer on, and the window on screen
pub(crate) fn viz_wanted(ui: &App) -> bool {
    let window = ui.window();
    ui.get_is_playing() && ui.get_show_visualizer() && window.is_visible() && !window.is_minimized()
}

/// Push one visualizer frame to the UI (levels already gated and smoothed).
/// The model is updated in place.
fn show_viz_frame(viz: &VizData, vu: &[f32], spectrum: &[f32], spectrum_model: &VecModel<f32>) {
    // Only the bands that changed: every write makes the visualizer update
    // (Wave redraws on any write), and in silence nothing changes
    set_levels(spectrum_model, spectrum);
    viz.set_vu_left(vu[0]);
    viz.set_vu_right(vu[1]);
    viz.set_has_signal(spectrum.iter().any(|&level| level > 0.01));
}

/// Models behind the Dot Matrix mode, updated in place
struct VizExtras {
    pub(crate) columns: std::rc::Rc<VecModel<f32>>,
    pub(crate) peaks: std::rc::Rc<VecModel<f32>>,
}

impl VizExtras {
    pub(crate) fn new(viz: &VizData) -> Self {
        let model = |n: usize| std::rc::Rc::new(VecModel::from(vec![0.0f32; n]));
        let extras = Self {
            columns: model(visual::MATRIX_COLUMNS),
            peaks: model(visual::MATRIX_COLUMNS),
        };
        viz.set_columns(ModelRc::from(extras.columns.clone()));
        viz.set_peaks(ModelRc::from(extras.peaks.clone()));
        extras
    }

    fn show_columns(&self, columns: &[f32], peaks: &[f32]) {
        set_levels(&self.columns, columns);
        set_levels(&self.peaks, peaks);
    }

    /// Clear the columns and peaks (playback stopped)
    pub(crate) fn clear(&self) {
        for model in [&self.columns, &self.peaks] {
            for i in 0..model.row_count() {
                model.set_row_data(i, 0.0);
            }
        }
    }
}

/// Write `levels` into `model` row by row, skipping unchanged rows
fn set_levels(model: &VecModel<f32>, levels: &[f32]) {
    for (i, &level) in levels.iter().enumerate().take(model.row_count()) {
        if model.row_data(i) != Some(level) {
            model.set_row_data(i, level);
        }
    }
}
