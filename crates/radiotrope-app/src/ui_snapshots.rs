//! `--ui-snapshots`: every screen of the window, drawn headless in both
//! themes on a fixed clock, with no audio or network. The same staging opens
//! each screen for the dialog test at the end of this file.

use std::cell::Cell;
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{Key, WindowEvent};
use slint::{
    Color, ComponentHandle, Image, LogicalSize, Model, ModelRc, Rgb8Pixel, SharedPixelBuffer,
    SharedString, VecModel,
};

use radiotrope::audio::health::{FailureReason, HealthState};
use radiotrope_app::config::remote::PORT as REMOTE_PORT;
use radiotrope_app::data::settings::McpAuth;
use radiotrope_app::text::{status, MISSING};

use crate::headless::HeadlessPlatform;
use crate::mcp::setup::{self, AgentApp};
use crate::{
    flag_image, remote_ui::qr_image, show_eq_presets, AboutInfo, AgentRow, App, BrowseStation,
    Defaults, FavoriteStation, FilterEntry, RemoteDevice, ScheduleDraft, ScheduleRow, VizData,
};

#[derive(clap::ValueEnum, Clone, Copy, Default)]
pub enum Theme {
    Dark,
    Light,
    #[default]
    Both,
}

pub fn parse_scale(value: &str) -> Result<f32, String> {
    value
        .parse::<f32>()
        .ok()
        .filter(|scale| (0.25..=8.0).contains(scale))
        .ok_or_else(|| "scale must be a number between 0.25 and 8".into())
}

/// The window size the screens are drawn at
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub enum Size {
    /// The window's size on first start
    #[default]
    Default,
    /// The smallest the window can be made
    Min,
    /// Width and height in logical pixels
    Exact(f32, f32),
}

/// `--size`: `WxH` in logical pixels, or `min` for the smallest window
pub fn parse_size(value: &str) -> Result<Size, String> {
    if value.eq_ignore_ascii_case("min") {
        return Ok(Size::Min);
    }
    let pixels = |part: &str| {
        part.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| (64..=8192).contains(n))
    };
    value
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some(Size::Exact(pixels(w)? as f32, pixels(h)? as f32)))
        .ok_or_else(|| "size must be WIDTHxHEIGHT (each 64 to 8192), or min".into())
}

/// How `--ui-snapshots` draws
#[derive(Default)]
pub struct Options {
    pub theme: Theme,
    pub scale: Option<f32>,
    pub size: Size,
    /// Only the shots whose names start with one of these (comma-separated)
    pub only: Option<String>,
}

struct Shot {
    name: &'static str,
    screen: &'static str,
    state: &'static str,
    description: &'static str,
}

impl Shot {
    const fn new(
        name: &'static str,
        screen: &'static str,
        state: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            name,
            screen,
            state,
            description,
        }
    }

    /// Shots that show a dialog over the window
    #[cfg(test)]
    fn is_dialog(&self) -> bool {
        DIALOG_SCREENS.contains(&self.screen)
    }

    /// How many presses of Esc close the dialog. Each one first closes
    /// what is on top: a dropdown, then the Scheduler's form (back to its
    /// list), a pairing overlay, or a filter picker (back to Search).
    #[cfg(test)]
    fn escapes(&self) -> usize {
        let dropdown = matches!(
            self.state,
            "presets" | "bitrate" | "agent" | "interface" | "auth" | "station" | "date" | "volume"
        );
        let nested = match self.screen {
            "picker" => true,
            "remote" => matches!(self.state, "code" | "qr"),
            "schedule" => !matches!(self.state, "" | "empty" | "remove"),
            _ => false,
        };
        1 + usize::from(dropdown) + usize::from(nested)
    }
}

const SHOTS: &[Shot] = &[
    Shot::new(
        "main-stopped",
        "main",
        "stopped",
        "Playback stopped with eight favorites.",
    ),
    Shot::new(
        "main-loading",
        "main",
        "loading",
        "Connecting to the selected station.",
    ),
    Shot::new(
        "main-playing",
        "main",
        "",
        "Playing station with codec, now-playing metadata and favorite row stats.",
    ),
    Shot::new(
        "main-error",
        "main",
        "error",
        "Selected station failed to connect.",
    ),
    Shot::new(
        "main-recording",
        "main",
        "recording",
        "Recording indicator, duration and size.",
    ),
    Shot::new(
        "main-recording-confirm",
        "main",
        "recording-confirm",
        "REC chip after one press: a second press stops the recording.",
    ),
    Shot::new(
        "main-viz-name",
        "main",
        "viz-name",
        "Visualizer tile showing its style name after a click or on hover.",
    ),
    Shot::new(
        "main-sleep-countdown",
        "main",
        "sleep",
        "Active sleep timer in the playback panel.",
    ),
    Shot::new(
        "main-schedule-countdown",
        "main",
        "schedule",
        "Upcoming scheduled recording in the playback panel.",
    ),
    Shot::new("main-empty", "main", "empty", "Empty favorites list."),
    Shot::new(
        "main-no-row-stats",
        "main",
        "no-stats",
        "Favorites with per-station stats hidden.",
    ),
    Shot::new(
        "main-recording-saved",
        "main",
        "recording-saved",
        "Playback panel notice after a recording is saved.",
    ),
    Shot::new(
        "main-recording-error",
        "main",
        "recording-error",
        "Playback panel notice after a recording failure.",
    ),
    Shot::new(
        "main-notice-error",
        "main",
        "notice-error",
        "A failed recording's notice: it stays until it is closed.",
    ),
    Shot::new(
        "main-notice-missed",
        "main",
        "notice-missed",
        "A missed Scheduler entry's notice: it stays until it is closed.",
    ),
    Shot::new(
        "main-notice-bubble",
        "main",
        "chip-bubble",
        "Full text of a cut-off notice in a bubble, with the pointer on it.",
    ),
    Shot::new(
        "main-save-error",
        "main",
        "save-error",
        "Favorites that can't be saved: a red chip until a save works.",
    ),
    Shot::new(
        "main-schedule-started",
        "main",
        "schedule-started",
        "Dismissible notice after scheduled playback starts.",
    ),
    Shot::new(
        "main-alarm",
        "main",
        "alarm",
        "Scheduler fallback alarm in the playback panel.",
    ),
    Shot::new(
        "main-muted",
        "main",
        "muted",
        "Playing stream with volume muted.",
    ),
    Shot::new(
        "main-volume-bubble",
        "main",
        "volume-bubble",
        "Volume percentage bubble above the slider.",
    ),
    Shot::new(
        "main-title-bubble",
        "main",
        "title-bubble",
        "Full now-playing title above a truncated track name.",
    ),
    Shot::new(
        "main-title-copied",
        "main",
        "title-copied",
        "Clipboard confirmation after copying the current track title.",
    ),
    Shot::new(
        "main-fade-in",
        "main",
        "fade-in",
        "Volume slider while the sound fades in after Play.",
    ),
    Shot::new(
        "main-long-names",
        "main",
        "long-names",
        "Long station names in the header and the favorites list.",
    ),
    Shot::new(
        "main-accent-purple",
        "main",
        "accent-purple",
        "Playing with the Purple accent instead of the default orange.",
    ),
    Shot::new(
        "main-accent-white",
        "main",
        "accent-white",
        "Playing with a custom white accent.",
    ),
    Shot::new(
        "main-system-title-bar",
        "main",
        "system-frame",
        "Radiotrope’s own title bar off; the system draws its bar outside the snapshot.",
    ),
    Shot::new(
        "main-spectrum",
        "visualizer",
        "spectrum",
        "Spectrum visualizer.",
    ),
    Shot::new(
        "main-mirror",
        "visualizer",
        "mirror",
        "Mirrored spectrum visualizer.",
    ),
    Shot::new(
        "main-dot-matrix",
        "visualizer",
        "dots",
        "Dot Matrix visualizer with peak dots.",
    ),
    Shot::new("main-vu", "visualizer", "vu", "Vertical stereo VU meters."),
    Shot::new(
        "main-horizontal-vu",
        "visualizer",
        "hbars",
        "Horizontal stereo VU meters.",
    ),
    Shot::new(
        "menu-open",
        "menu",
        "open",
        "Open menu with sources and import choices.",
    ),
    Shot::new("menu-stations", "menu", "stations", "Stations menu."),
    Shot::new(
        "menu-view",
        "menu",
        "view",
        "View menu with the inline Theme selector.",
    ),
    Shot::new(
        "menu-theme",
        "view-group",
        "theme",
        "Theme selector in the View menu, with the current theme selected.",
    ),
    Shot::new(
        "menu-window-options",
        "view-group",
        "window",
        "Window options expanded in the View menu.",
    ),
    Shot::new(
        "menu-system-tray",
        "view-group",
        "tray",
        "System Tray options expanded in the View menu.",
    ),
    Shot::new("menu-tools", "menu", "tools", "Tools menu."),
    Shot::new(
        "menu-recordings",
        "tools-group",
        "recordings",
        "Recordings options expanded in the Tools menu.",
    ),
    Shot::new(
        "menu-connections",
        "tools-group",
        "connections",
        "Connections options expanded in the Tools menu.",
    ),
    Shot::new("menu-help", "menu", "help", "Help menu."),
    Shot::new(
        "menu-window",
        "menu",
        "window",
        "Radiotrope’s fallback title-bar context menu.",
    ),
    Shot::new(
        "favorite-menu",
        "favorite-menu",
        "open",
        "A favorite’s context menu.",
    ),
    Shot::new(
        "favorite-reset-confirm",
        "favorite-menu",
        "reset",
        "Press-again confirmation for resetting a favorite’s stats.",
    ),
    Shot::new(
        "favorite-remove-confirm",
        "favorite-menu",
        "remove",
        "Press-again confirmation for removing a favorite.",
    ),
    Shot::new(
        "favorite-stats",
        "favorite-stats",
        "",
        "Favorite listening-time and play-count bubble.",
    ),
    Shot::new(
        "agents-bubble",
        "bubble",
        "agents",
        "Connected and recently seen AI agents.",
    ),
    Shot::new(
        "listeners-bubble",
        "bubble",
        "listeners",
        "Phones listening to the current stream.",
    ),
    Shot::new(
        "search-results",
        "search",
        "filtered",
        "Station results with country and genre chips selected.",
    ),
    Shot::new(
        "search-loading",
        "search",
        "loading",
        "Station directory query in progress.",
    ),
    Shot::new(
        "search-error",
        "search",
        "error",
        "Station directory failure with retry.",
    ),
    Shot::new(
        "search-empty",
        "search",
        "empty",
        "Query without matching stations.",
    ),
    Shot::new(
        "search-filtered-empty",
        "search",
        "filtered",
        "Query without matches under the selected filters.",
    ),
    Shot::new(
        "search-next-page-loading",
        "search",
        "next-loading",
        "Loaded results while another page is requested.",
    ),
    Shot::new(
        "search-next-page-error",
        "search",
        "next-error",
        "Results retained after another page fails, with a retry link.",
    ),
    Shot::new(
        "search-row-loading",
        "search",
        "row-loading",
        "A result row connecting after it was picked.",
    ),
    Shot::new(
        "search-row-failed",
        "search",
        "row-failed",
        "A result row whose station failed to connect.",
    ),
    Shot::new(
        "search-keyboard-row",
        "search",
        "keyboard",
        "The results list with the keyboard: Down from the search field, the second row active.",
    ),
    Shot::new(
        "picker-country",
        "picker",
        "country",
        "Country filter options with flags and counts.",
    ),
    Shot::new(
        "picker-genre",
        "picker",
        "genre",
        "Genre filter options and counts.",
    ),
    Shot::new(
        "picker-keyboard-row",
        "picker",
        "country-keys",
        "The country list with the keyboard: Down from the filter field, the third row active.",
    ),
    Shot::new(
        "picker-loading",
        "picker",
        "loading",
        "Country filter entries loading.",
    ),
    Shot::new(
        "picker-error",
        "picker",
        "error",
        "Country filter directory failure with retry.",
    ),
    Shot::new(
        "picker-no-match",
        "picker",
        "no-match",
        "Country filter text that matches no country.",
    ),
    Shot::new(
        "network-stream-empty",
        "network-stream",
        "",
        "Empty custom stream form.",
    ),
    Shot::new(
        "network-stream-filled",
        "network-stream",
        "filled",
        "Custom stream form ready to play.",
    ),
    Shot::new(
        "edit-favorite",
        "edit-favorite",
        "",
        "Existing favorite values in the edit form.",
    ),
    Shot::new(
        "edit-favorite-remove",
        "edit-favorite",
        "remove",
        "Remove confirmation in the edit form.",
    ),
    Shot::new(
        "edit-favorite-error",
        "edit-favorite",
        "error",
        "Save refused, with the reason under the URL.",
    ),
    Shot::new(
        "import-choice",
        "import",
        "",
        "Import Favorites add-or-replace choice.",
    ),
    Shot::new(
        "import-result",
        "import",
        "result",
        "Successful favorites import summary.",
    ),
    Shot::new(
        "import-error",
        "import",
        "error",
        "Favorites import failed with an explanation.",
    ),
    Shot::new(
        "equalizer",
        "equalizer",
        "",
        "Enabled ten-band equalizer with adjusted gains.",
    ),
    Shot::new(
        "equalizer-presets",
        "equalizer",
        "presets",
        "Grouped equalizer preset menu.",
    ),
    Shot::new(
        "filters",
        "filters",
        "",
        "Audio filters and their current values.",
    ),
    Shot::new(
        "filters-info",
        "filters",
        "info",
        "Volume Leveler information bubble.",
    ),
    Shot::new(
        "recording-settings",
        "recording",
        "",
        "Recording folder, format, bitrate and EQ setting.",
    ),
    Shot::new(
        "recording-bitrates",
        "recording",
        "bitrate",
        "Recording bitrate dropdown.",
    ),
    Shot::new(
        "recording-folder-error",
        "recording",
        "folder-error",
        "A recording folder that can't be used, with the reason.",
    ),
    Shot::new(
        "sleep-idle",
        "sleep",
        "",
        "Sleep Timer with a duration ready to start.",
    ),
    Shot::new(
        "sleep-running",
        "sleep",
        "running",
        "Sleep Timer showing the remaining time.",
    ),
    Shot::new(
        "sleep-nothing-playing",
        "sleep",
        "",
        "Sleep Timer with nothing playing: Start waits for a station.",
    ),
    Shot::new(
        "scheduler",
        "schedule",
        "",
        "Scheduled playback, recording and stop entries.",
    ),
    Shot::new(
        "scheduler-last-result",
        "schedule",
        "last-result",
        "Scheduler entries with how their last run went.",
    ),
    Shot::new(
        "scheduler-empty",
        "schedule",
        "empty",
        "Scheduler with no entries yet.",
    ),
    Shot::new(
        "scheduler-save-error",
        "schedule",
        "add",
        "New entry refused on save, with the reason.",
    ),
    Shot::new(
        "scheduler-add",
        "schedule",
        "add",
        "New scheduler entry form.",
    ),
    Shot::new(
        "scheduler-no-stations",
        "schedule",
        "add",
        "New entry with no station to pick: Stop only, and Search Stations.",
    ),
    Shot::new(
        "scheduler-edit",
        "schedule",
        "edit",
        "Existing weekly scheduler entry in the edit form.",
    ),
    Shot::new(
        "scheduler-remove",
        "schedule",
        "remove",
        "Press-again confirmation for removing a schedule.",
    ),
    Shot::new(
        "scheduler-stations",
        "schedule",
        "station",
        "Favorite station dropdown in the scheduler form.",
    ),
    Shot::new(
        "scheduler-dates",
        "schedule",
        "date",
        "Date dropdown for a one-time schedule.",
    ),
    Shot::new(
        "scheduler-volume",
        "schedule",
        "volume",
        "Volume dropdown in the scheduler form.",
    ),
    Shot::new(
        "scheduler-fallback-info",
        "schedule",
        "fallback-info",
        "Fallback sound information bubble.",
    ),
    Shot::new(
        "agents-off",
        "agents",
        "off",
        "Local MCP setup with network access off.",
    ),
    Shot::new(
        "agents-on",
        "agents",
        "on",
        "Network MCP setup with an example access token.",
    ),
    Shot::new(
        "agents-apps",
        "agents",
        "agent",
        "Local agent application dropdown.",
    ),
    Shot::new(
        "agents-interfaces",
        "agents",
        "interface",
        "Network listen interface dropdown.",
    ),
    Shot::new(
        "agents-auth",
        "agents",
        "auth",
        "Network authentication dropdown.",
    ),
    Shot::new(
        "agents-local-info",
        "agents",
        "local-info",
        "Local connection information bubble.",
    ),
    Shot::new(
        "agents-network-info",
        "agents",
        "network-info",
        "Network connection information bubble.",
    ),
    Shot::new(
        "agents-status-error",
        "agents",
        "status-error",
        "Network access on, but the server can't listen.",
    ),
    Shot::new(
        "agents-new-token",
        "agents",
        "new-token",
        "New Token pressed once: the second press replaces the token.",
    ),
    Shot::new(
        "agents-token-error",
        "agents",
        "token-error",
        "A new token couldn't be made.",
    ),
    Shot::new("remote-off", "remote", "off", "Remote Control disabled."),
    Shot::new(
        "remote-on",
        "remote",
        "on",
        "Remote Control enabled with paired devices.",
    ),
    Shot::new(
        "remote-no-phones",
        "remote",
        "no-phones",
        "Remote Control enabled with no phone paired yet.",
    ),
    Shot::new(
        "remote-status-info",
        "remote",
        "status-info",
        "Remote Control startup error with its full explanation open.",
    ),
    Shot::new(
        "remote-address-info",
        "remote",
        "address-info",
        "Remote Control address information bubble.",
    ),
    Shot::new(
        "remote-recordings-info",
        "remote",
        "recordings-info",
        "Recording sharing information bubble.",
    ),
    Shot::new(
        "remote-remove",
        "remote",
        "remove",
        "Press-again confirmation for removing a device.",
    ),
    Shot::new(
        "remote-pairing-code",
        "remote",
        "code",
        "Time-limited confirmation code for a phone.",
    ),
    Shot::new(
        "remote-pairing-qr",
        "remote",
        "qr",
        "QR pairing overlay and expiration countdown.",
    ),
    Shot::new(
        "statistics",
        "statistics",
        "",
        "Live stream, codec, network, buffer and decode statistics.",
    ),
    Shot::new(
        "statistics-stopped",
        "statistics",
        "stopped",
        "Statistics with nothing playing.",
    ),
    Shot::new(
        "statistics-failed",
        "statistics",
        "failed",
        "Statistics for a stream that stopped sending audio.",
    ),
    Shot::new(
        "about",
        "about",
        "",
        "About Radiotrope with build details and links.",
    ),
    Shot::new(
        "accent-color",
        "accent",
        "",
        "Accent Color presets and custom color controls.",
    ),
    Shot::new(
        "accent-color-low-contrast",
        "accent",
        "low-contrast",
        "A custom white accent: the low-contrast note under the picker.",
    ),
    Shot::new(
        "keyboard-shortcuts",
        "shortcuts",
        "",
        "Desktop keyboard shortcut reference.",
    ),
    Shot::new(
        "keyboard-shortcuts-end",
        "shortcuts",
        "scrolled-end",
        "Keyboard Shortcuts scrolled to the last card with the End key.",
    ),
];

/// The screens that open a dialog
#[cfg(test)]
const DIALOG_SCREENS: &[&str] = &[
    "search",
    "picker",
    "network-stream",
    "edit-favorite",
    "import",
    "equalizer",
    "filters",
    "recording",
    "sleep",
    "schedule",
    "agents",
    "remote",
    "statistics",
    "about",
    "accent",
    "shortcuts",
];

pub fn run(out_dir: &Path, options: &Options) -> Result<(), Box<dyn Error>> {
    let only: Vec<&str> = options
        .only
        .as_deref()
        .map(|list| {
            list.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let shots: Vec<&Shot> = SHOTS
        .iter()
        .filter(|shot| only.is_empty() || only.iter().any(|p| shot.name.starts_with(p)))
        .collect();
    if shots.is_empty() {
        return Err(format!("no shot name starts with {}", only.join(" or ")).into());
    }

    let platform = HeadlessPlatform::default();
    let window = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform))?;
    std::fs::create_dir_all(out_dir)?;

    let themes: &[&str] = match options.theme {
        Theme::Dark => &["dark"],
        Theme::Light => &["light"],
        Theme::Both => &["dark", "light"],
    };
    let mut index = String::from(concat!(
        "# Radiotrope UI snapshots\n\n",
        "The Theme selector is inline in the View menu.\n",
        "Wi-Fi Settings and the on-screen keyboard are embedded-only. ",
        "OS-native file pickers and tray and window menus are outside the Slint app.\n",
    ));
    let mut window_noted = false;
    for &theme in themes {
        writeln!(index, "\n## {theme}\n")?;
        for shot in &shots {
            let Staged {
                ui,
                window,
                mut pixels,
            } = stage(&window, shot, theme, options)?;
            if !window_noted {
                let scale = ui.window().scale_factor();
                let size = ui.window().size().to_logical(scale);
                let heading = index.rfind("\n## ").unwrap_or(index.len());
                let note = format!(
                    "\nWindow: {} x {} logical pixels at scale {scale}.\n",
                    size.width, size.height
                );
                index.insert_str(heading, &note);
                window_noted = true;
            }
            time.set(time.get() + Duration::from_millis(350));
            slint::platform::update_timers_and_animations();
            paint(&window, &mut pixels);
            ui.hide()?;

            let name = format!("{theme}-{}.png", shot.name);
            image::save_buffer(
                out_dir.join(&name),
                pixels.as_bytes(),
                pixels.width(),
                pixels.height(),
                image::ColorType::Rgb8,
            )
            .map_err(|error| format!("could not save {name}: {error}"))?;
            writeln!(index, "- [{name}]({name}) · {}", shot.description)?;
        }
    }
    std::fs::write(out_dir.join("index.md"), index)?;
    Ok(())
}

/// One shot's window with its screen open, drawn once
struct Staged {
    ui: App,
    window: Rc<MinimalSoftwareWindow>,
    pixels: SharedPixelBuffer<Rgb8Pixel>,
}

/// Make the window for `shot`, fill it with the fixtures and open its
/// screen, as a user would see it; the clock has not moved on yet
fn stage(
    made: &Cell<Option<Rc<MinimalSoftwareWindow>>>,
    shot: &Shot,
    theme: &str,
    options: &Options,
) -> Result<Staged, Box<dyn Error>> {
    let ui = App::new()?;
    // The shortcut list and the menu hints, as in the live app
    crate::shortcuts::install(&ui);
    let window = made.take().ok_or("Slint did not create a window adapter")?;
    let defaults = ui.global::<Defaults>();
    ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: options.scale.unwrap_or(1.0),
    });
    window.set_size(match options.size {
        Size::Default => {
            LogicalSize::new(defaults.get_window_width(), defaults.get_window_height())
        }
        Size::Min => LogicalSize::new(
            defaults.get_window_min_width(),
            defaults.get_window_min_height(),
        ),
        Size::Exact(width, height) => LogicalSize::new(width, height),
    });
    ui.set_snapshot_mode(true);
    ui.set_theme_mode(theme.into());
    crate::setup_text_util(&ui);
    seed(&ui);
    configure(&ui, shot);
    let needs_layout =
        matches!(shot.screen, "menu" | "view-group" | "tools-group") || shot.name == "main-empty";
    if !needs_layout {
        open_screen(&ui, shot);
    }
    ui.show()?;
    let size = window.size();
    let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(size.width, size.height);

    // Lay out the window before opening popups, whose origins depend on
    // the geometry of the items that open them.
    paint(&window, &mut pixels);
    if needs_layout {
        open_screen(&ui, shot);
    }
    after_open(&ui, shot);
    // Let Slint evaluate change handlers and finish its short UI
    // transitions on a fixed clock, without a GUI event loop.
    slint::platform::update_timers_and_animations();
    paint(&window, &mut pixels);
    Ok(Staged { ui, window, pixels })
}

/// What a user does once the screen is open: a key, a popup
fn after_open(ui: &App, shot: &Shot) {
    match (shot.screen, shot.state) {
        (_, "scrolled-end") => press(ui, Key::End),
        // Enter in the form's first field saves it, and the save fails
        ("edit-favorite", "error") => press(ui, Key::Return),
        _ if shot.name == "scheduler-save-error" => press(ui, Key::Return),
        // Down from the text field goes into the list, then moves in it
        ("search", "keyboard") | ("picker", "country-keys") => {
            for _ in 0..if shot.screen == "search" { 2 } else { 3 } {
                press(ui, Key::DownArrow);
                slint::platform::update_timers_and_animations();
            }
        }
        ("picker", "no-match") => {
            // What the filter callback leaves for a text nothing matches
            ui.set_filter_entries_text("Atlantis".into());
            ui.set_filter_entries(ModelRc::default());
        }
        _ => {}
    }
    if shot.screen == "main" && shot.state == "chip-bubble" {
        // The pointer on the notice chip under the station
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(300.0, 112.0),
        });
    }
    if shot.screen == "favorite-menu" {
        ui.set_snapshot_favorite_menu(shot.state.into());
    }
    if shot.screen == "bubble" {
        ui.set_snapshot_bubble(shot.state.into());
    }
    if shot.screen == "accent" && shot.state == "low-contrast" {
        ui.set_accent_color(Color::from_rgb_u8(0xff, 0xff, 0xff));
    }
    if shot.state == "presets" {
        // A preset in use, so the list shows its tick
        ui.set_eq_preset_name("Rock".into());
        ui.set_snapshot_preset_menu(true);
    }
    ui.set_snapshot_overlay(shot.state.into());
}

/// Press and release one key
fn press(ui: &App, key: Key) {
    let text = SharedString::from(key);
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn paint(window: &MinimalSoftwareWindow, pixels: &mut SharedPixelBuffer<Rgb8Pixel>) {
    window.request_redraw();
    let stride = pixels.width() as usize;
    window.draw_if_needed(|renderer| {
        renderer.render(pixels.make_mut_slice(), stride);
    });
}

fn model<T: Clone + 'static>(items: impl IntoIterator<Item = T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items.into_iter().collect::<Vec<_>>()))
}

fn strings<'a>(items: impl IntoIterator<Item = &'a str>) -> ModelRc<SharedString> {
    model(items.into_iter().map(SharedString::from))
}

const STATIONS: [(&str, &str, [u8; 3]); 8] = [
    ("Harbour Jazz", "United Kingdom", [33, 109, 116]),
    ("North Coast FM", "Ireland", [60, 105, 65]),
    ("Studio Canal", "France", [129, 67, 125]),
    ("Midnight Assembly", "United Kingdom", [42, 74, 139]),
    ("Küste Kultur", "Germany", [153, 73, 50]),
    ("Radio Verde", "Portugal", [38, 114, 88]),
    ("Dawn Sessions", "Canada", [155, 68, 88]),
    ("Sundial Radio", "Australia", [157, 105, 35]),
];

fn logo(index: usize) -> Image {
    let [red, green, blue] = STATIONS[index].2;
    let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(64, 64);
    for (i, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
        let x = (i % 64) as i32 - 32;
        let y = (i / 64) as i32 - 32;
        let ring = (x * x + y * y) / 90;
        let stripe = if (x + y + index as i32 * 3).rem_euclid(19) < 7 {
            28
        } else {
            0
        };
        let glow = (24 - ring).max(0) as u8 + stripe;
        *pixel = Rgb8Pixel::new(red + glow, green + glow, blue + glow);
    }
    Image::from_rgb8(pixels)
}

fn station_url(index: usize) -> SharedString {
    format!("https://radio.example/stream/{}", index + 1).into()
}

fn seed(ui: &App) {
    ui.set_favorites_list(model(STATIONS.iter().enumerate().map(
        |(index, &(name, country, _))| {
            FavoriteStation {
                id: format!("station-{index}").into(),
                name: name.into(),
                country: country.into(),
                url: station_url(index),
                logo_url: format!("https://radio.example/logos/{}.png", index + 1).into(),
                listen_time: radiotrope_app::text::minutes(
                    ((8 - index) * 60 + 34 + index * 2) as u64 * 60,
                )
                .into(),
                last_played: if index == 0 { "Today" } else { "Yesterday" }.into(),
                play_count: 27 - index as i32 * 3,
                session_time: if index == 0 { "24 min" } else { "" }.into(),
            }
        },
    )));
    ui.set_favorite_logos(model((0..STATIONS.len()).map(logo)));
    ui.set_station_name(STATIONS[0].0.into());
    ui.set_station_country(STATIONS[0].1.into());
    ui.set_station_url(station_url(0));
    ui.set_station_logo_url("https://radio.example/logos/1.png".into());
    ui.set_current_logo(logo(0));
    ui.set_is_station_favorited(true);
    ui.set_codec_info("AAC · 192 kbps · 48 kHz · Stereo".into());
    ui.set_is_playing(true);
    ui.set_status_text(status::PLAYING.into());
    ui.set_show_visualizer(true);
    ui.set_viz_mode("wave".into());
    let viz = ui.global::<VizData>();
    viz.set_active(true);
    viz.set_has_signal(true);
    viz.set_vu_left(0.72);
    viz.set_vu_right(0.61);
    viz.set_spectrum(model([
        0.66, 0.71, 0.63, 0.50, 0.39, 0.56, 0.62, 0.43, 0.30, 0.41, 0.38, 0.26, 0.20, 0.26, 0.17,
        0.09,
    ]));
    viz.set_columns(model([0.69, 0.57, 0.48, 0.53, 0.36, 0.32, 0.23, 0.13]));
    viz.set_peaks(model([0.86, 0.71, 0.72, 0.78, 0.52, 0.43, 0.39, 0.27]));
    ui.set_can_record(true);
    ui.set_now_playing_title("The Cedar Quartet — Blue Hour".into());
    ui.set_volume(0.72);
    // The fade level: the slider at rest, not mid-fade (see main-fade-in)
    ui.set_volume_level(1.0);
    ui.set_show_station_stats(true);

    ui.set_eq_enabled(true);
    ui.set_eq_preset_name("".into());
    ui.set_eq_preamp(-2.0);
    ui.set_eq_band0(3.0);
    ui.set_eq_band1(2.0);
    ui.set_eq_band2(1.0);
    ui.set_eq_band3(-1.0);
    ui.set_eq_band4(-2.0);
    ui.set_eq_band6(1.0);
    ui.set_eq_band7(2.0);
    ui.set_eq_band8(1.0);
    show_eq_presets(ui);
    ui.set_hiss_reducer("light".into());
    ui.set_recording_folder("/home/radio/Music/Radiotrope".into());
    ui.set_recording_default_folder(ui.get_recording_folder());
    ui.set_recording_format("mp3".into());
    ui.set_recording_bitrate(192);
    ui.set_record_with_eq(true);

    seed_schedule(ui);
    seed_connections(ui);
    seed_statistics(ui);
    let about = ui.global::<AboutInfo>();
    about.set_version(env!("CARGO_PKG_VERSION").into());
    about.set_commit("a1b2c3d45".into());
    about.set_commit_date("2026-11-01".into());
    about.set_platform("Linux x86_64".into());
    about.set_config_dir("/home/radio/.config/radiotrope".into());
    about.set_cache_dir("/home/radio/.cache/radiotrope".into());
    about.set_repository(env!("CARGO_PKG_REPOSITORY").into());
    about.set_license(crate::license_label(env!("CARGO_PKG_LICENSE")).into());
    about.set_authors(crate::author_names(env!("CARGO_PKG_AUTHORS")).into());
}

fn seed_schedule(ui: &App) {
    ui.set_schedule_stations(strings(STATIONS.iter().map(|station| station.0)));
    ui.set_schedule_dates(strings([
        "Today, Sun 22 Nov",
        "Tomorrow, Mon 23 Nov",
        "Tue 24 Nov",
    ]));
    ui.set_schedule_today(0);
    ui.set_schedule_next("Next: Tomorrow 07:00, Harbour Jazz".into());
    ui.set_schedule_rows(model([
        ScheduleRow {
            id: 1,
            enabled: true,
            time: "07:00".into(),
            action: "play".into(),
            title: "Harbour Jazz".into(),
            details: "Weekdays · for 1 h · fade in".into(),
            running: false,
            last: Default::default(),
            last_bad: false,
        },
        ScheduleRow {
            id: 2,
            enabled: true,
            time: "19:30".into(),
            action: "record".into(),
            title: "North Coast FM".into(),
            details: "Friday · for 45 min".into(),
            running: false,
            last: Default::default(),
            last_bad: false,
        },
        ScheduleRow {
            id: 3,
            enabled: false,
            time: "23:45".into(),
            action: "stop".into(),
            title: "Stop playback".into(),
            details: "Every day".into(),
            running: false,
            last: Default::default(),
            last_bad: false,
        },
    ]));
    ui.on_schedule_draft(|id| {
        let editing = id != 0;
        ScheduleDraft {
            id,
            action: "play".into(),
            station: 0,
            start: if editing { "07:00" } else { "18:30" }.into(),
            once: !editing,
            date: 0,
            days: model([true, true, true, true, true, false, false]),
            end_kind: if editing { "after" } else { "never" }.into(),
            end_after: "60".into(),
            end_at: "22:00".into(),
            volume: 0,
            fade: editing,
            fallback: false,
        }
    });
}

/// The example address of this computer on the local network
const LAN_IP: &str = "192.0.2.42";

fn seed_connections(ui: &App) {
    ui.set_agents_available(true);
    ui.set_agents_apps(strings(AgentApp::ALL.iter().map(|app| app.label())));
    ui.set_agents_auths(strings(McpAuth::ALL.iter().map(|auth| auth.label())));
    ui.set_agents_auth(1);
    let interface = format!("wlan0 ({LAN_IP})");
    ui.set_agents_listen_options(strings([
        "This computer only",
        interface.as_str(),
        "All networks",
    ]));
    ui.set_agents_listen(1);
    ui.set_agents_listen_local(false);
    let port = radiotrope_app::config::mcp::DEFAULT_ADDRESS
        .rsplit_once(':')
        .map_or("8765", |(_, port)| port);
    ui.set_agents_port(port.into());
    ui.set_agents_token("snapshot-example-token-83fb2fd9".into());
    ui.set_agents_url(format!("http://{LAN_IP}:{port}/mcp").into());
    show_agent_lines(ui, AgentApp::ClaudeCode);

    ui.set_remote_name("Living Room Radio".into());
    ui.set_remote_name_placeholder("Radiotrope".into());
    ui.set_remote_address(format!("{LAN_IP}:{REMOTE_PORT}").into());
    ui.set_remote_address_note(crate::remote_ui::address_note().into());
    ui.set_remote_devices(model([
        RemoteDevice {
            id: "phone-1".into(),
            name: "Maya's phone".into(),
            seen: "Used 5 min ago".into(),
        },
        RemoteDevice {
            id: "phone-2".into(),
            name: "Kitchen tablet".into(),
            seen: "Used yesterday".into(),
        },
    ]));
}

/// The setup lines for `app`, picked in the dialog
fn show_agent_lines(ui: &App, app: AgentApp) {
    let index = AgentApp::ALL.iter().position(|a| *a == app).unwrap_or(0);
    ui.set_agents_app(index as i32);
    crate::show_agent_lines(ui);
    // The program's own path, which differs on every machine
    ui.set_agents_local_command(setup::local_line(app, "radiotrope").into());
}

fn seed_statistics(ui: &App) {
    let ui = ui.global::<crate::StatsData>();
    crate::show_health(&ui, &HealthState::Healthy);
    ui.set_playtime("24:18".into());
    ui.set_codec("AAC".into());
    ui.set_bitrate("192 kbps".into());
    ui.set_sample_rate("48 kHz".into());
    ui.set_channels("Stereo".into());
    ui.set_received("33.4 MB".into());
    ui.set_segments(MISSING.into());
    ui.set_throughput("197 kbps".into());
    ui.set_buffer_level("352 KB".into());
    ui.set_buffer_capacity("512 KB".into());
    ui.set_frames_played("68,343".into());
    ui.set_decode_errors("0".into());
    ui.set_underruns("0".into());
    ui.set_hum("50 Hz".into());
    ui.set_leveler("−2.4 dB".into());
    ui.set_hiss("Light".into());
}

fn configure(ui: &App, shot: &Shot) {
    match shot.screen {
        "main" => match shot.state {
            "stopped" | "loading" | "error" | "empty" => {
                ui.set_is_playing(false);
                let viz = ui.global::<VizData>();
                viz.set_active(false);
                viz.set_has_signal(false);
                ui.set_status_text(status::STOPPED.into());
                ui.set_can_record(false);
                ui.set_now_playing_title("".into());
                ui.set_codec_info("".into());
                match shot.state {
                    "loading" => {
                        ui.set_is_loading(true);
                        ui.set_status_text(status::CONNECTING.into());
                    }
                    "error" => {
                        ui.set_is_error(true);
                        ui.set_status_text("Can't connect to radio.example".into());
                    }
                    "empty" => {
                        ui.set_station_url("".into());
                        ui.set_station_name(crate::UNNAMED_STATION.into());
                        ui.set_status_text(status::READY.into());
                        ui.set_station_country("".into());
                        ui.set_is_station_favorited(false);
                        ui.set_current_logo(Image::default());
                        ui.set_favorites_list(ModelRc::default());
                        ui.set_favorite_logos(ModelRc::default());
                    }
                    _ => {}
                }
            }
            "sleep" => {
                configure_sleep(ui);
            }
            "schedule" => {
                ui.set_schedule_label("Record in 0:12".into());
            }
            "schedule-started" => {
                ui.set_schedule_label("Play · Harbour Jazz".into());
                ui.set_schedule_label_closable(true);
            }
            "alarm" => {
                ui.set_schedule_label("Alarm mode".into());
                ui.set_schedule_label_bell(true);
                ui.set_schedule_label_closable(true);
            }
            "recording" | "recording-confirm" => {
                ui.set_is_recording(true);
                ui.set_recording_text("2:36".into());
                ui.set_recording_size("3.6 MB".into());
            }
            "recording-saved" => ui.set_recording_notice("Saved · 3.6 MB".into()),
            "recording-error" => {
                ui.set_recording_notice("Recording failed".into());
                ui.set_recording_notice_kind("error".into());
                ui.set_recording_notice_stays(true);
            }
            "notice-error" | "chip-bubble" => {
                ui.set_recording_notice(
                    "Recording stopped: no space left on device. Saved Harbour Jazz - 2026-11-20 19-30-00.mp3".into(),
                );
                ui.set_recording_notice_kind("error".into());
                ui.set_recording_notice_stays(true);
            }
            "notice-missed" => {
                ui.set_recording_notice("Missed 07:00: Play Harbour Jazz".into());
                ui.set_recording_notice_kind("scheduler".into());
                ui.set_recording_notice_stays(true);
            }
            "save-error" => {
                ui.set_save_error("Can't save favorites: no space left on device".into());
            }
            "muted" => ui.set_is_muted(true),
            "fade-in" => ui.set_volume_level(0.72),
            "long-names" => configure_long_names(ui),
            "accent-purple" => ui.set_accent_color(Color::from_rgb_u8(0x91, 0x41, 0xac)),
            "accent-white" => ui.set_accent_color(Color::from_rgb_u8(0xff, 0xff, 0xff)),
            "system-frame" => ui.set_custom_frame(false),
            "title-bubble" => ui.set_now_playing_title("The Cedar Quartet and the North Coast Orchestra — Blue Hour: Live at Harbour Hall, second set (late-night encore and extended improvisation)".into()),
            "no-stats" => ui.set_show_station_stats(false),
            _ => {}
        },
        "visualizer" => ui.set_viz_mode(shot.state.into()),
        "search" => {
            if matches!(shot.state, "filtered" | "next-loading" | "next-error") {
                ui.set_snapshot_country_flag(flag_image(Some("GB"), None));
            }
            if shot.name == "search-results"
                || matches!(shot.state, "next-loading" | "next-error" | "keyboard")
            {
                set_results(ui);
            }
            if shot.state == "loading" {
                ui.set_search_loading(true);
            }
            if shot.state == "next-loading" {
                ui.set_has_more(true);
                ui.set_search_loading_more(true);
            }
            if matches!(shot.state, "error" | "next-error") {
                ui.set_search_error("The station directory did not respond.".into());
            }
            if matches!(shot.state, "row-loading" | "row-failed") {
                // The third result picked from the list: the window plays it
                set_results(ui);
                let picked = ui.get_search_results().row_data(2).unwrap_or_default();
                ui.set_station_name(picked.name);
                ui.set_station_url(picked.url);
                ui.set_is_station_favorited(false);
                ui.set_is_playing(false);
                ui.set_now_playing_title("".into());
                ui.set_codec_info("".into());
                if shot.state == "row-loading" {
                    ui.set_is_loading(true);
                    ui.set_status_text(status::CONNECTING.into());
                } else {
                    ui.set_is_error(true);
                    ui.set_status_text("Can't connect to radio.example".into());
                }
            }
        }
        "edit-favorite" if shot.state == "error" => {
            ui.on_edit_favorite(|_| {
                "Can't save: this favorite was removed. Close and try again.".into()
            });
        }
        "recording" if shot.state == "folder-error" => {
            ui.set_recording_folder_error(
                "Can't open the folder: Permission denied (os error 13)".into(),
            );
        }
        "schedule" if shot.state == "last-result" => {
            let last = [
                ("Last: played · Fri 07:00", false),
                ("Last: failed, no space left on device · Fri 19:30", true),
                ("Last: missed · Thu 23:45", true),
            ];
            let rows: Vec<ScheduleRow> = ui
                .get_schedule_rows()
                .iter()
                .zip(last)
                .map(|(row, (last, last_bad))| ScheduleRow {
                    last: last.into(),
                    last_bad,
                    ..row
                })
                .collect();
            ui.set_schedule_rows(model(rows));
        }
        "schedule" if shot.state == "empty" => {
            ui.set_schedule_rows(ModelRc::default());
            ui.set_schedule_next("".into());
        }
        "schedule" if shot.name == "scheduler-save-error" => ui.on_schedule_save(|draft| {
            format!(
                "{} has already passed today. Pick a later time or another day.",
                draft.start
            )
            .into()
        }),
        "sleep" if shot.state == "running" => configure_sleep(ui),
        "sleep" if shot.name == "sleep-nothing-playing" => {
            ui.set_is_playing(false);
            ui.set_is_loading(false);
        }
        "schedule" if shot.name == "scheduler-no-stations" => {
            ui.set_schedule_stations(ModelRc::default());
        }
        "import" => {
            ui.set_import_subtitle("12 stations found in favorites.json".into());
            ui.set_import_add_summary("Add 5 new stations".into());
            ui.set_import_replace_summary("Replace your list with 12 stations".into());
            if shot.state == "result" {
                ui.set_import_message("Favorites imported".into());
                ui.set_import_message_detail("Added 5 stations and skipped 7 duplicates.".into());
                ui.set_import_can_undo(true);
            } else if shot.state == "error" {
                ui.set_import_message("Can't read the file: permission denied".into());
                ui.set_import_message_detail(
                    "The selected file could not be read as a favorites list.".into(),
                );
                ui.set_import_message_error(true);
            }
            ui.set_show_import_favorites(true);
        }
        "bubble" => {
            if shot.state == "agents" {
                ui.set_agent_count(1);
                ui.set_agent_list(model([
                    AgentRow {
                        name: "Claude Code".into(),
                        place: "This computer".into(),
                        network: false,
                        seen: "Connected".into(),
                        connected: true,
                    },
                    AgentRow {
                        name: "Studio assistant".into(),
                        place: "192.0.2.18".into(),
                        network: true,
                        seen: "4 min ago".into(),
                        connected: false,
                    },
                ]));
            } else {
                ui.set_listeners(strings(["Maya's phone", "Kitchen tablet"]));
            }
        }
        "agents" => {
            // The agent whose note the info bubble shows
            match shot.state {
                "local-info" => show_agent_lines(ui, AgentApp::ClaudeDesktop),
                "network-info" => show_agent_lines(ui, AgentApp::Codex),
                _ => {}
            }
            ui.set_agents_connected(
                if shot.state == "on" {
                    "Connected: Claude Code"
                } else {
                    "No agent connected yet"
                }
                .into(),
            );
            if shot.state == "token-error" {
                ui.set_agents_token_error("Can't make a new token: permission denied".into());
            }
            if !matches!(shot.state, "off" | "agent" | "local-info") {
                ui.set_agents_network(true);
                if shot.state == "status-error" {
                    ui.set_agents_status(
                        format!(
                            "Can't listen on {LAN_IP}:{}: Address already in use (os error 98)",
                            ui.get_agents_port()
                        )
                        .into(),
                    );
                    ui.set_agents_status_error(true);
                } else {
                    ui.set_agents_status(format!("Listening on {}", ui.get_agents_url()).into());
                }
            }
        }
        "statistics" => match shot.state {
            "failed" => crate::show_health(
                &ui.global::<crate::StatsData>(),
                &HealthState::Failed(FailureReason::StreamStall),
            ),
            "stopped" => crate::clear_stats_ui(ui),
            _ => {}
        },
        "remote" if shot.state != "off" => {
            ui.set_remote_on(true);
            match shot.state {
                "status-info" => {
                    ui.set_remote_status(format!("Can't start Remote Control: port {REMOTE_PORT} is in use by another program. Close that program and try again.").into());
                    ui.set_remote_address("".into());
                    ui.set_remote_status_error(true);
                    ui.set_remote_status_short("Can't start".into());
                }
                "code" => {
                    ui.set_pairing_device("Maya's phone".into());
                    ui.set_pairing_code("483 621".into());
                    ui.set_pairing_seconds(52);
                    ui.set_pairing_shown(true);
                    // Cancel and Esc hide it, as in the app
                    let ui_weak = ui.as_weak();
                    ui.on_remote_cancel_pairing(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_pairing_shown(false);
                        }
                    });
                }
                "qr" => {
                    ui.set_pairing_qr(
                        qr_image("https://radio.example/pair?snapshot=3dfa70aab4e18507")
                            .expect("the fixed pairing payload fits in a QR code"),
                    );
                    ui.set_pairing_qr_seconds(88);
                    ui.set_pairing_qr_shown(true);
                    let ui_weak = ui.as_weak();
                    ui.on_remote_cancel_qr(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_pairing_qr_shown(false);
                        }
                    });
                }
                "no-phones" => ui.set_remote_devices(ModelRc::default()),
                _ => {}
            }
        }
        _ => {}
    }
}

fn open_screen(ui: &App, shot: &Shot) {
    ui.invoke_snapshot_open(shot.screen.into(), shot.state.into());
    if shot.screen == "picker" {
        match shot.state {
            "error" => {
                ui.set_filter_entries_loading(false);
                ui.set_filter_entries_error("The station directory did not respond.".into());
            }
            "loading" => {}
            "genre" => set_picker_entries(ui, "genre"),
            _ => set_picker_entries(ui, "country"),
        }
    }
}

/// Names longer than the header and the rows have room for
fn configure_long_names(ui: &App) {
    const LONG: [&str; 3] = [
        "Harbour Jazz and Blues: The Late Night Session from the North Atlantic Coast",
        "Radio Nacional de las Islas Atlánticas · Programa Cultural",
        "Midnight Assembly Community Broadcasting Collective",
    ];
    let favorites = ui.get_favorites_list();
    ui.set_favorites_list(model(favorites.iter().enumerate().map(
        |(index, mut station)| {
            if let Some(name) = LONG.get(index) {
                station.name = (*name).into();
            }
            station
        },
    )));
    ui.set_station_name(LONG[0].into());
    ui.set_station_country("United Kingdom of Great Britain and Northern Ireland".into());
}

fn configure_sleep(ui: &App) {
    ui.set_sleep_active(true);
    ui.set_sleep_left("18:42".into());
    ui.set_sleep_until("23:15".into());
    ui.set_sleep_left_secs(18 * 60 + 42);
    ui.set_sleep_length(30);
    ui.set_sleep_fade(true);
}

fn set_results(ui: &App) {
    let results = [
        ("Harbour Jazz", 0, "AAC", 192),
        ("Midnight Assembly", 3, "MP3", 128),
        ("Low Tide Jazz", 4, "MP3", 192),
        ("Maple Street Radio", 7, "AAC", 128),
    ];
    ui.set_search_results(model(results.iter().enumerate().map(
        |(index, &(name, logo_index, codec, bitrate))| BrowseStation {
            name: name.into(),
            url: station_url(if index < 2 {
                logo_index
            } else {
                index + STATIONS.len()
            }),
            country: "United Kingdom".into(),
            logo_url: "".into(),
            provider_id: format!("directory-{index}").into(),
            codec: codec.into(),
            bitrate,
            is_favorite: index < 2,
        },
    )));
    ui.set_browse_logos(model(results.iter().map(|(_, index, _, _)| logo(*index))));
}

fn set_picker_entries(ui: &App, kind: &str) {
    let entries: &[(&str, &str, i32)] = if kind == "country" {
        &[
            ("United Kingdom", "GB", 1842),
            ("United States", "US", 6305),
            ("France", "FR", 1926),
            ("Germany", "DE", 2550),
            ("Ireland", "IE", 199),
            ("Portugal", "PT", 315),
        ]
    } else {
        &[
            ("Jazz", "jazz", 719),
            ("Blues", "blues", 328),
            ("Classical", "classical", 1084),
            ("Ambient", "ambient", 293),
            ("Soul", "soul", 496),
        ]
    };
    ui.set_filter_entries(model(entries.iter().map(|&(name, code, count)| {
        FilterEntry {
            name: name.into(),
            code: code.into(),
            station_count: count,
            flag: if kind == "country" {
                flag_image(Some(code), None)
            } else {
                Image::default()
            },
        }
    })));
    ui.set_filter_entries_loading(false);
}

#[cfg(test)]
#[path = "ui_snapshots_test.rs"]
mod tests;

#[cfg(test)]
#[path = "key_lists_test.rs"]
mod key_list_tests;
