use std::cell::Cell;
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
use slint::{
    ComponentHandle, Image, LogicalSize, ModelRc, Rgb8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};

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

#[derive(Default)]
struct HeadlessPlatform {
    window: Rc<Cell<Option<Rc<MinimalSoftwareWindow>>>>,
    time: Rc<Cell<Duration>>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        self.window.set(Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        self.time.get()
    }
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
        "scheduler",
        "schedule",
        "",
        "Scheduled playback, recording and stop entries.",
    ),
    Shot::new(
        "scheduler-add",
        "schedule",
        "add",
        "New scheduler entry form.",
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
    Shot::new("remote-off", "remote", "off", "Remote Control disabled."),
    Shot::new(
        "remote-on",
        "remote",
        "on",
        "Remote Control enabled with paired devices.",
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
        "keyboard-shortcuts",
        "shortcuts",
        "",
        "Desktop keyboard shortcut reference.",
    ),
];

pub fn run(out_dir: &Path, theme: Theme, scale: f32) -> Result<(), Box<dyn Error>> {
    let platform = HeadlessPlatform::default();
    let window = platform.window.clone();
    let time = platform.time.clone();
    slint::platform::set_platform(Box::new(platform))?;
    std::fs::create_dir_all(out_dir)?;

    let themes: &[&str] = match theme {
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
    for &theme in themes {
        writeln!(index, "\n## {theme}\n")?;
        for shot in SHOTS {
            let ui = App::new()?;
            // The shortcut list and the menu hints, as in the live app
            crate::shortcuts::install(&ui);
            let window = window
                .take()
                .ok_or("Slint did not create a window adapter")?;
            let defaults = ui.global::<Defaults>();
            ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
            window.set_size(LogicalSize::new(
                defaults.get_window_width(),
                defaults.get_window_height(),
            ));
            ui.set_snapshot_mode(true);
            ui.set_theme_mode(theme.into());
            crate::setup_text_util(&ui);
            seed(&ui);
            configure(&ui, shot);
            let needs_layout = matches!(shot.screen, "menu" | "view-group" | "tools-group")
                || shot.name == "main-empty";
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
            if shot.screen == "favorite-menu" {
                ui.set_snapshot_favorite_menu(shot.state.into());
            }
            if shot.screen == "bubble" {
                ui.set_snapshot_bubble(shot.state.into());
            }
            if shot.state == "presets" {
                ui.set_snapshot_preset_menu(true);
            }
            ui.set_snapshot_overlay(shot.state.into());
            // Let Slint evaluate change handlers and finish its short UI
            // transitions on a fixed clock, without a GUI event loop.
            slint::platform::update_timers_and_animations();
            paint(&window, &mut pixels);
            time.set(time.get() + Duration::from_millis(350));
            slint::platform::update_timers_and_animations();
            paint(&window, &mut pixels);
            ui.hide()?;

            let name = format!("{theme}-{}.png", shot.name);
            image::save_buffer(
                out_dir.join(&name),
                pixels.as_bytes(),
                size.width,
                size.height,
                image::ColorType::Rgb8,
            )
            .map_err(|error| format!("could not save {name}: {error}"))?;
            writeln!(index, "- [{name}]({name}) · {}", shot.description)?;
        }
    }
    std::fs::write(out_dir.join("index.md"), index)?;
    Ok(())
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
    ui.set_status_text("Playing".into());
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
    ui.set_volume_level(0.72);
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
        "Today · Sun 22 Nov",
        "Tomorrow · Mon 23 Nov",
        "Tue 24 Nov",
    ]));
    ui.set_schedule_today(0);
    ui.set_schedule_next("Next: Tomorrow at 07:00".into());
    ui.set_schedule_rows(model([
        ScheduleRow {
            id: 1,
            enabled: true,
            time: "07:00".into(),
            action: "play".into(),
            title: "Harbour Jazz".into(),
            details: "Weekdays · for 1 h · fade in".into(),
            running: false,
        },
        ScheduleRow {
            id: 2,
            enabled: true,
            time: "19:30".into(),
            action: "record".into(),
            title: "North Coast FM".into(),
            details: "Friday · for 45 min".into(),
            running: false,
        },
        ScheduleRow {
            id: 3,
            enabled: false,
            time: "23:45".into(),
            action: "stop".into(),
            title: "Stop playback".into(),
            details: "Every day".into(),
            running: false,
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

fn seed_connections(ui: &App) {
    ui.set_agents_available(true);
    ui.set_agents_apps(strings(["Claude Code", "Codex", "Other"]));
    ui.set_agents_auths(strings(["None", "Access token"]));
    ui.set_agents_auth(1);
    ui.set_agents_listen_options(strings(["This computer · 127.0.0.1", "Wi-Fi · 192.0.2.42"]));
    ui.set_agents_listen(1);
    ui.set_agents_port("8787".into());
    ui.set_agents_local_command("radiotrope --mcp".into());
    ui.set_agents_local_note(
        crate::mcp::setup::AgentApp::ClaudeDesktop
            .local_note()
            .into(),
    );
    ui.set_agents_network_note(crate::mcp::setup::AgentApp::Codex.network_note(true).into());
    ui.set_agents_network_placeholder("Turn on network access to see the setup command.".into());
    ui.set_agents_command(
        "claude mcp add --transport http radiotrope https://192.0.2.42:8787/mcp".into(),
    );
    ui.set_agents_token("snapshot-example-token-83fb2fd9".into());
    ui.set_agents_url("https://192.0.2.42:8787/mcp".into());

    ui.set_remote_name("Living Room Radio".into());
    ui.set_remote_name_placeholder("Radiotrope".into());
    ui.set_remote_address("https://192.0.2.42:8750".into());
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

fn seed_statistics(ui: &App) {
    ui.set_stat_health("Playing".into());
    ui.set_stat_playtime("24:18".into());
    ui.set_stat_codec("AAC".into());
    ui.set_stat_bitrate("192 kbps".into());
    ui.set_stat_sample_rate("48 kHz".into());
    ui.set_stat_channels("Stereo".into());
    ui.set_stat_received("33.4 MB".into());
    ui.set_stat_segments("—".into());
    ui.set_stat_throughput("197 kbps".into());
    ui.set_stat_buffer_level("352 KB".into());
    ui.set_stat_buffer_capacity("512 KB".into());
    ui.set_stat_frames_played("68,343".into());
    ui.set_stat_decode_errors("0".into());
    ui.set_stat_underruns("0".into());
    ui.set_stat_hum("50 Hz".into());
    ui.set_stat_leveler("−2.4 dB".into());
    ui.set_stat_hiss("Light".into());
}

fn configure(ui: &App, shot: &Shot) {
    match shot.screen {
        "main" => match shot.state {
            "stopped" | "loading" | "error" | "empty" => {
                ui.set_is_playing(false);
                let viz = ui.global::<VizData>();
                viz.set_active(false);
                viz.set_has_signal(false);
                ui.set_status_text("Stopped".into());
                ui.set_can_record(false);
                ui.set_now_playing_title("".into());
                ui.set_codec_info("".into());
                match shot.state {
                    "loading" => {
                        ui.set_is_loading(true);
                        ui.set_status_text("Connecting…".into());
                    }
                    "error" => {
                        ui.set_is_error(true);
                        ui.set_status_text("Can't connect to radio.example".into());
                    }
                    "empty" => {
                        ui.set_station_url("".into());
                        ui.set_station_name(crate::UNNAMED_STATION.into());
                        ui.set_status_text("Ready".into());
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
            "recording" => {
                ui.set_is_recording(true);
                ui.set_recording_text("2:36".into());
                ui.set_recording_size("3.6 MB".into());
            }
            "recording-saved" => ui.set_recording_notice("Saved · 3.6 MB".into()),
            "recording-error" => {
                ui.set_recording_notice("Recording failed".into());
                ui.set_recording_notice_error(true);
            }
            "muted" => ui.set_is_muted(true),
            "title-bubble" => ui.set_now_playing_title("The Cedar Quartet and the North Coast Orchestra — Blue Hour: Live at Harbour Hall, second set (late-night encore and extended improvisation)".into()),
            "no-stats" => ui.set_show_station_stats(false),
            _ => {}
        },
        "visualizer" => ui.set_viz_mode(shot.state.into()),
        "search" => {
            if matches!(shot.state, "filtered" | "next-loading" | "next-error") {
                ui.set_snapshot_country_flag(flag_image(Some("GB"), None));
            }
            if shot.name == "search-results" || matches!(shot.state, "next-loading" | "next-error")
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
        }
        "sleep" if shot.state == "running" => configure_sleep(ui),
        "import" => {
            ui.set_import_subtitle("12 stations found in favorites.json".into());
            ui.set_import_add_summary("Add 5 new stations".into());
            ui.set_import_replace_summary("Replace your list with 12 stations".into());
            if shot.state == "result" {
                ui.set_import_message("Favorites imported".into());
                ui.set_import_message_detail("Added 5 stations and skipped 7 duplicates.".into());
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
        "agents" if !matches!(shot.state, "off" | "agent" | "local-info") => {
            ui.set_agents_network(true);
            ui.set_agents_status("Listening on 192.0.2.42:8787".into());
        }
        "statistics" if shot.state == "failed" => {
            ui.set_stat_health("Failed · stopped sending".into());
        }
        "remote" if shot.state != "off" => {
            ui.set_remote_on(true);
            match shot.state {
                "status-info" => {
                    ui.set_remote_status("Can't start Remote Control: port 8750 is in use by another program. Close that program and try again.".into());
                    ui.set_remote_status_error(true);
                    ui.set_remote_status_short("Can't start".into());
                }
                "code" => {
                    ui.set_pairing_device("Maya's phone".into());
                    ui.set_pairing_code("483 621".into());
                    ui.set_pairing_seconds(52);
                    ui.set_pairing_shown(true);
                }
                "qr" => {
                    ui.set_pairing_qr(
                        qr_image("https://radio.example/pair?snapshot=3dfa70aab4e18507")
                            .expect("the fixed pairing payload fits in a QR code"),
                    );
                    ui.set_pairing_qr_seconds(88);
                    ui.set_pairing_qr_shown(true);
                }
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
            kind => set_picker_entries(ui, kind),
        }
    }
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
