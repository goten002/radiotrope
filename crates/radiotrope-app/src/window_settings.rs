//! The window's settings: shown at startup, changed from the Equalizer,
//! Filters and Accent Color dialogs, and saved to settings.json

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, VecModel};

use radiotrope::audio::{HissLevel, LevelTarget};
use radiotrope::error::RadioError;
use radiotrope_app::data::settings::{Settings, Theme as ThemeSetting};
use radiotrope_app::data::types::Station;
use radiotrope_app::error::AppError;

use crate::app::delayed_save::{DelayedSave, SaveErrors, Saved, SAVE_RETRY};
use crate::app::state::AppSnapshot;
use crate::context::{lock, AppContext};
use crate::{app, App, EqPresetGroup, HexColor, VizStyle};
#[cfg(feature = "desktop")]
use crate::{tray, window_frame};

/// Show the saved settings in the window: volume, equalizer, filters,
/// accent, theme, visualizer, tray and frame choices, and the window size
pub(crate) fn apply(ui: &App, settings: &radiotrope_app::data::settings::Settings) {
    // Apply initial settings to UI
    ui.set_volume(settings.volume);
    ui.set_is_muted(settings.muted);
    ui.set_eq_enabled(settings.eq_enabled);
    ui.set_eq_preset_name(settings.eq_preset_name.as_deref().unwrap_or("").into());
    ui.set_eq_preamp(settings.eq_preamp);
    ui.set_eq_band0(settings.eq_gains[0]);
    ui.set_eq_band1(settings.eq_gains[1]);
    ui.set_eq_band2(settings.eq_gains[2]);
    ui.set_eq_band3(settings.eq_gains[3]);
    ui.set_eq_band4(settings.eq_gains[4]);
    ui.set_eq_band5(settings.eq_gains[5]);
    ui.set_eq_band6(settings.eq_gains[6]);
    ui.set_eq_band7(settings.eq_gains[7]);
    ui.set_eq_band8(settings.eq_gains[8]);
    ui.set_eq_band9(settings.eq_gains[9]);
    show_filters(
        ui,
        &FilterSettings {
            match_volume: settings.match_volume,
            target: LevelTarget::parse(&settings.match_level).unwrap_or_default(),
            hiss: HissLevel::parse(&settings.hiss_reducer).unwrap_or_default(),
            hum_filter: settings.hum_filter,
            rumble_filter: settings.rumble_filter,
            smooth_switching: settings.smooth_switching,
        },
    );
    show_eq_presets(ui);

    // Apply saved accent color
    if let Some((r, g, b)) = settings.accent_color_rgb() {
        ui.set_accent_color(slint::Color::from_rgb_u8(r, g, b));
    }

    ui.set_touch_scroll(cfg!(feature = "embedded"));

    // Apply saved theme and viz mode
    // The Pi can't ask an OS for its theme, so System is dark there
    let theme = match settings.theme {
        ThemeSetting::System if cfg!(feature = "embedded") => ThemeSetting::Dark,
        theme => theme,
    };
    ui.set_theme_mode(theme.name().into());
    // Modes that no longer exist (the old curve, Waterfall) fall back to Wave
    let viz_mode = match settings.viz_mode.as_str() {
        m @ ("wave" | "spectrum" | "mirror" | "dots" | "vu" | "hbars") => m,
        _ => "wave",
    };
    ui.set_viz_mode(viz_mode.into());
    ui.global::<VizStyle>()
        .set_palette(settings.viz_palette.as_str().into());
    ui.set_show_station_stats(settings.show_station_stats);
    ui.set_show_visualizer(settings.show_visualizer);
    ui.set_panel_gradient(settings.panel_gradient);
    // Zoom and the play reports
    crate::view_options::setup(ui, settings);
    #[cfg(feature = "desktop")]
    {
        ui.set_tray_icon(settings.tray_icon);
        ui.set_close_to_tray(settings.close_to_tray);
        ui.set_start_in_tray(settings.start_in_tray);
        ui.set_hide_on_minimize(settings.hide_on_minimize);
    }
    // The Pi build is full screen with no frame to replace
    if cfg!(not(feature = "embedded")) {
        ui.set_custom_frame(settings.custom_title_bar);
        ui.set_rounded_corners(settings.rounded_corners);
        // Windows 11 rounds the window itself
        ui.set_draw_corners(cfg!(not(target_os = "windows")));
        #[cfg(feature = "desktop")]
        {
            // Corners (and the focus note for the tray) once the system
            // window exists, and again when the choice or the frame changes
            let ui_weak = ui.as_weak();
            let apply_corners = move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let corners = window_frame::set_corners(
                        ui.window(),
                        ui.get_custom_frame(),
                        ui.get_rounded_corners(),
                    );
                    ui.set_corners_available(corners.available);
                    ui.set_draw_corners(corners.drawn);
                    ui.set_corners_hint(corners.hint.into());
                }
            };
            window_frame::watch_window(ui.window(), apply_corners.clone());
            ui.on_rounded_corners_changed(apply_corners.clone());
            let ui_weak = ui.as_weak();
            ui.on_minimize_window(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    // Wayland never says a window was minimized, so our
                    // own button hides to the tray itself
                    if tray::hides_on_minimize(&ui) {
                        let _ = ui.hide();
                    } else {
                        window_frame::minimize(ui.window());
                    }
                }
            });
            // Ctrl+W hides the window and never quits (Ctrl+Q does): to the
            // tray when there is an icon to bring it back, else minimized
            let ui_weak = ui.as_weak();
            ui.on_hide_window(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    if tray::has_tray(&ui) {
                        let _ = ui.hide();
                    } else {
                        window_frame::minimize(ui.window());
                    }
                }
            });
            let ui_weak = ui.as_weak();
            ui.on_switch_title_bar(move |custom| {
                if let Some(ui) = ui_weak.upgrade() {
                    window_frame::set_system_frame(ui.window(), !custom);
                    ui.set_custom_frame(custom);
                    apply_corners();
                }
            });
            let ui_weak = ui.as_weak();
            ui.on_window_theme_changed(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    window_frame::set_title_bar_theme(ui.window(), &ui.get_theme_mode());
                }
            });
            // The saved choice, once the window exists
            if theme != ThemeSetting::System {
                let ui_weak = ui.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.invoke_window_theme_changed();
                    }
                });
            }
            let ui_weak = ui.as_weak();
            ui.on_show_system_menu(move |x, y, pressed| {
                ui_weak
                    .upgrade()
                    .is_some_and(|ui| window_frame::show_system_menu(ui.window(), x, y, pressed))
            });
        }
    }

    // Apply saved window size
    if let (Some(w), Some(h)) = (settings.window_width, settings.window_height) {
        ui.window()
            .set_size(slint::LogicalSize::new(w as f32, h as f32));
    }
}

/// Wire the Equalizer, Filters and Accent Color dialogs
pub(crate) fn setup(ui: &App, ctx: &AppContext) {
    let (ui_tx, shared_state) = (&ctx.tx, &ctx.state);
    // EQ callbacks
    {
        let tx = ui_tx.clone();
        ui.on_eq_band_changed(move |band, gain| {
            tx.send(app::state::AppCommand::SetEqBand {
                band: band as usize,
                gain_db: gain,
            });
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_eq_preamp_changed(move |val| {
            tx.send(app::state::AppCommand::SetEqPreamp(val));
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_eq_preset_selected(move |name| {
            tx.send(app::state::AppCommand::SetEqPreset(name.to_string()));
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_eq_enabled_toggled(move |val| {
            tx.send(app::state::AppCommand::SetEqEnabled(val));
        });
    }

    // Filters callbacks
    {
        let tx = ui_tx.clone();
        ui.on_match_volume_toggled(move |on| {
            tx.send(app::state::AppCommand::SetMatchVolume(on));
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_level_target_changed(move |id| {
            if let Some(target) = LevelTarget::parse(&id) {
                tx.send(app::state::AppCommand::SetLevelTarget(target));
            }
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_hiss_reducer_changed(move |id| {
            if let Some(level) = HissLevel::parse(&id) {
                tx.send(app::state::AppCommand::SetHissReducer(level));
            }
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_hum_filter_toggled(move |on| {
            tx.send(app::state::AppCommand::SetHumFilter(on));
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_rumble_filter_toggled(move |on| {
            tx.send(app::state::AppCommand::SetRumbleFilter(on));
        });
    }
    {
        let tx = ui_tx.clone();
        ui.on_smooth_switching_toggled(move |on| {
            tx.send(app::state::AppCommand::SetSmoothSwitching(on));
        });
    }

    // Accent color callbacks
    setup_hex_color(ui);
    {
        let state = shared_state.clone();
        ui.on_accent_color_changed(move |color| {
            let hex = radiotrope_app::hex_color::format_hex_color(
                color.red(),
                color.green(),
                color.blue(),
            );
            let mut s = lock(&state);
            s.accent_color = Some(hex);
        });
    }
}

/// The Accent Color dialog's hex field: format, check and parse in Rust
pub(crate) fn setup_hex_color(ui: &App) {
    use radiotrope_app::hex_color::{format_hex_color, parse_hex_color};
    let hex = ui.global::<HexColor>();
    hex.on_to_hex(|c| format_hex_color(c.red(), c.green(), c.blue()).into());
    hex.on_is_valid(|text| parse_hex_color(&text).is_some());
    hex.on_to_color(|text| {
        let (r, g, b) = parse_hex_color(&text).unwrap_or_default();
        slint::Color::from_rgb_u8(r, g, b)
    });
}

/// The equalizer's preset menu, from the engine's preset list: the presets
/// without a group (Flat) on their own, then a column per group, in the
/// list's order
pub(crate) fn show_eq_presets(ui: &App) {
    let mut top: Vec<slint::SharedString> = Vec::new();
    let mut groups: Vec<(&str, Vec<slint::SharedString>)> = Vec::new();
    for preset in radiotrope::audio::PRESETS {
        match preset.group.title() {
            None => top.push(preset.name.into()),
            Some(title) => match groups.iter_mut().find(|(t, _)| *t == title) {
                Some((_, names)) => names.push(preset.name.into()),
                None => groups.push((title, vec![preset.name.into()])),
            },
        }
    }
    let groups: Vec<EqPresetGroup> = groups
        .into_iter()
        .map(|(title, names)| EqPresetGroup {
            title: title.into(),
            names: ModelRc::new(VecModel::from(names)),
        })
        .collect();
    ui.set_eq_top_presets(ModelRc::new(VecModel::from(top)));
    ui.set_eq_preset_groups(ModelRc::new(VecModel::from(groups)));
}

/// Persist current app state to settings.json (a moment later, off this
/// thread: see [`write_settings`])
pub(crate) fn save_settings(shared_state: &Arc<Mutex<AppSnapshot>>, ui: &App) {
    // A copy: the controller and agents aren't kept waiting on the state
    let s = lock(shared_state).clone();
    write_settings(&window_settings(&s, ui));
}

/// [`save_settings`] at exit: written before this returns
pub(crate) fn save_settings_at_exit(shared_state: &Arc<Mutex<AppSnapshot>>, ui: &App) {
    save_settings(shared_state, ui);
    settings_saver().flush();
}

/// Change the saved settings: read the file, make `change`, write it back,
/// on this thread. Returns the settings with the change. A file that can't
/// be read is never written over (the change then lasts for this run), and
/// a failed save is logged and shown by [`SaveErrors`].
pub(crate) fn update_settings(change: impl FnOnce(&mut Settings)) -> Settings {
    let (settings, saved) = Settings::update(change);
    SaveErrors::shared().set(SETTINGS, saved.err().map(|e| save_reason(&e)));
    settings
}

/// Why a save failed, in plain words: "permission denied", without the
/// system's "(os error 13)"
pub(crate) fn save_reason(e: &AppError) -> String {
    if let AppError::Engine(RadioError::Io(e)) = e {
        return radiotrope_app::text::io_reason(e);
    }
    let mut text = e.to_string();
    if let Some(at) = text.rfind(" (os error ") {
        let end = text[at..].find(')').map_or(text.len(), |end| at + end + 1);
        text.replace_range(at..end, "");
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => text,
    }
}

/// What the window keeps in settings.json, as it is now
#[derive(Clone, PartialEq)]
pub(crate) struct WindowSettings {
    pub(crate) volume: f32,
    pub(crate) muted: bool,
    pub(crate) eq_gains: [f32; 10],
    pub(crate) eq_preamp: f32,
    pub(crate) eq_preamp_moved: bool,
    pub(crate) eq_enabled: bool,
    pub(crate) eq_preset_name: Option<String>,
    pub(crate) match_volume: bool,
    pub(crate) match_level: LevelTarget,
    pub(crate) hiss: HissLevel,
    pub(crate) hum_filter: bool,
    pub(crate) rumble_filter: bool,
    pub(crate) smooth_switching: bool,
    pub(crate) accent_color: Option<String>,
    pub(crate) theme: ThemeSetting,
    pub(crate) viz_mode: String,
    pub(crate) viz_palette: String,
    pub(crate) show_station_stats: bool,
    pub(crate) show_visualizer: bool,
    pub(crate) panel_gradient: bool,
    /// View > Zoom in percent, for the next start
    pub(crate) zoom: u32,
    pub(crate) report_plays: bool,
    /// Custom title bar and rounded corners (not on the Pi)
    pub(crate) frame: Option<(bool, bool)>,
    /// Tray icon, close to tray, start in tray, minimize to tray (desktop)
    pub(crate) tray: Option<(bool, bool, bool, bool)>,
    pub(crate) last_station: Option<Station>,
    window_size: Option<(u32, u32)>,
}

pub(crate) fn window_settings(s: &AppSnapshot, ui: &App) -> WindowSettings {
    let last_station = s
        .station_url
        .as_ref()
        .filter(|url| !url.is_empty())
        .map(|url| {
            let name = s
                .station_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| radiotrope_app::data::types::name_from_url(url));
            // With its other streams, so the next run can fall back to them
            let mut station = Station::new(&name, url);
            let streams = std::iter::once(url.clone())
                .chain(crate::player_ui::backups_of(url))
                .map(radiotrope_app::data::types::Stream::new)
                .collect();
            station.set_streams(streams, url);
            let logo_url = ui.get_station_logo_url().to_string();
            if !logo_url.is_empty() {
                station = station.with_logo(&logo_url);
            }
            station.country = Some(ui.get_station_country().to_string()).filter(|c| !c.is_empty());
            station
        });

    // Saved in logical pixels, the unit the size is restored in at startup.
    // The window reports physical pixels, so saving those made the window
    // grow by the display scale (e.g. 125%) on every launch. A maximized
    // window keeps the size it had before it was maximized. With another
    // zoom picked for the next start, the size is saved in that start's
    // pixels, so the window is as large on screen as it is now.
    let zoom = crate::view_options::zoom_choice(ui);
    let window = ui.window();
    let window_size = (!window.is_maximized())
        .then(|| window.size().to_logical(window.scale_factor()))
        .filter(|size| size.width >= 1.0 && size.height >= 1.0)
        .map(|size| (size.width.round() as u32, size.height.round() as u32))
        .map(|size| crate::view_options::size_at_zoom(size, zoom));

    WindowSettings {
        volume: s.volume,
        muted: s.is_muted,
        eq_gains: s.eq_gains,
        eq_preamp: s.eq_preamp,
        eq_preamp_moved: s.eq_preamp_moved,
        eq_enabled: s.eq_enabled,
        eq_preset_name: s.eq_preset_name.clone(),
        match_volume: s.match_volume,
        match_level: s.level_target,
        hiss: s.hiss,
        hum_filter: s.hum_filter,
        rumble_filter: s.rumble_filter,
        smooth_switching: s.smooth_switching,
        accent_color: s.accent_color.clone(),
        // The choice, not the colours it gives: an OS switch saves nothing
        theme: ThemeSetting::from_name(&ui.get_theme_mode()).unwrap_or_default(),
        viz_mode: ui.get_viz_mode().to_string(),
        viz_palette: ui.global::<VizStyle>().get_palette().to_string(),
        show_station_stats: ui.get_show_station_stats(),
        show_visualizer: ui.get_show_visualizer(),
        panel_gradient: ui.get_panel_gradient(),
        zoom,
        report_plays: crate::view_options::report_plays_choice(ui),
        frame: cfg!(not(feature = "embedded"))
            .then(|| (ui.get_custom_frame(), ui.get_rounded_corners())),
        tray: cfg!(feature = "desktop").then(|| {
            (
                ui.get_tray_icon(),
                ui.get_close_to_tray(),
                ui.get_start_in_tray(),
                ui.get_hide_on_minimize(),
            )
        }),
        last_station,
        window_size,
    }
}

/// What [`SaveErrors`] calls a save of the settings
const SETTINGS: &str = "settings";

/// What [`write_settings`] was last given and the saving thread hasn't
/// written yet
static UNSAVED: Mutex<Option<WindowSettings>> = Mutex::new(None);

/// The thread that writes [`UNSAVED`], started when first needed
fn settings_saver() -> &'static DelayedSave {
    static SAVER: OnceLock<DelayedSave> = OnceLock::new();
    SAVER.get_or_init(|| {
        DelayedSave::start_reporting(
            "settings-save",
            Duration::ZERO,
            SETTINGS,
            SaveErrors::shared().clone(),
            SAVE_RETRY,
            write_unsaved,
        )
    })
}

/// Write [`UNSAVED`] into settings.json. After a failure it stays there
/// for the next try.
fn write_unsaved() -> Result<Saved, String> {
    let Some(w) = lock(&UNSAVED).clone() else {
        return Ok(Saved::All);
    };
    let (_, saved) = Settings::update(|settings| w.apply_to(settings));
    saved.map_err(|e| save_reason(&e))?;
    let mut unsaved = lock(&UNSAVED);
    if unsaved.as_ref() == Some(&w) {
        *unsaved = None;
        Ok(Saved::All)
    } else {
        Ok(Saved::More)
    }
}

/// Write `w` into settings.json, keeping what the file has besides. The
/// file is read and written on a thread of its own: this one (the
/// window's) only hands `w` over.
pub(crate) fn write_settings(w: &WindowSettings) {
    *lock(&UNSAVED) = Some(w.clone());
    settings_saver().request_now();
}

impl WindowSettings {
    /// Put these into `settings`, which keeps what it has besides
    fn apply_to(&self, settings: &mut Settings) {
        let w = self;
        settings.volume = w.volume;
        settings.muted = w.muted;
        settings.eq_gains = w.eq_gains;
        settings.eq_preamp = w.eq_preamp;
        settings.eq_preamp_moved = w.eq_preamp_moved;
        settings.eq_enabled = w.eq_enabled;
        settings.eq_preset_name = w.eq_preset_name.clone();
        settings.match_volume = w.match_volume;
        settings.match_level = w.match_level.as_str().to_string();
        settings.hiss_reducer = w.hiss.as_str().to_string();
        settings.hum_filter = w.hum_filter;
        settings.rumble_filter = w.rumble_filter;
        settings.smooth_switching = w.smooth_switching;
        settings.accent_color = w.accent_color.clone();
        settings.theme = w.theme;
        settings.viz_mode = w.viz_mode.clone();
        settings.viz_palette = w.viz_palette.clone();
        settings.show_station_stats = w.show_station_stats;
        settings.show_visualizer = w.show_visualizer;
        settings.panel_gradient = w.panel_gradient;
        settings.zoom = w.zoom;
        settings.report_plays = w.report_plays;
        if let Some((custom_title_bar, rounded_corners)) = w.frame {
            settings.custom_title_bar = custom_title_bar;
            settings.rounded_corners = rounded_corners;
        }
        if let Some((tray_icon, close_to_tray, start_in_tray, hide_on_minimize)) = w.tray {
            settings.tray_icon = tray_icon;
            settings.close_to_tray = close_to_tray;
            settings.start_in_tray = start_in_tray;
            settings.hide_on_minimize = hide_on_minimize;
        }
        if let Some(station) = &w.last_station {
            settings.last_station = Some(station.clone());
        }
        if let Some((width, height)) = w.window_size {
            settings.window_width = Some(width);
            settings.window_height = Some(height);
        }
    }
}

/// The filters as the Filters dialog shows them
pub(crate) struct FilterSettings {
    pub(crate) match_volume: bool,
    pub(crate) target: LevelTarget,
    pub(crate) hiss: HissLevel,
    pub(crate) hum_filter: bool,
    pub(crate) rumble_filter: bool,
    pub(crate) smooth_switching: bool,
}

impl FilterSettings {
    pub(crate) fn of(s: &AppSnapshot) -> Self {
        Self {
            match_volume: s.match_volume,
            target: s.level_target,
            hiss: s.hiss,
            hum_filter: s.hum_filter,
            rumble_filter: s.rumble_filter,
            smooth_switching: s.smooth_switching,
        }
    }
}

/// The filters into the window
pub(crate) fn show_filters(ui: &App, f: &FilterSettings) {
    ui.set_match_volume(f.match_volume);
    ui.set_level_target(f.target.as_str().into());
    ui.set_hiss_reducer(f.hiss.as_str().into());
    ui.set_hum_filter(f.hum_filter);
    ui.set_rumble_filter(f.rumble_filter);
    ui.set_smooth_switching(f.smooth_switching);
}
