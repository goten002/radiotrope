//! The Recording Settings dialog, the record button and its notices, and
//! the recording folder

use std::sync::{Arc, Mutex};
use std::time::Instant;

use slint::ComponentHandle;

use radiotrope_app::config::ui::RECORDING_NOTICE_TIME;
use radiotrope_app::data::recordings;
use radiotrope_app::data::types::Station;
use radiotrope_app::network::logo::LogoService;
use radiotrope_app::{text, visual};

use crate::app::state::{AppSnapshot, NoticeKind};
use crate::context::{lock, AppContext};
use crate::stats_ui::format_bytes;
use crate::{app, App};

/// Largest station logo embedded as cover art in a recording
const MAX_COVER_BYTES: usize = 512 * 1024;

/// Wire the Tools menu recording items and the Recording Settings
/// dialog. The format, folder and before/after-EQ choice are saved right away.
pub(crate) fn setup(
    ui: &App,
    ctx: &AppContext,
    settings: &radiotrope_app::data::settings::Settings,
) {
    let (cmd_tx, shared_state, logo_service) =
        (ctx.tx.clone(), ctx.state.clone(), ctx.logos.clone());
    show_recording_folder(ui, settings.recording_dir.as_deref());
    ui.set_record_with_eq(settings.record_with_eq);
    ui.set_recording_format(settings.recording_format.id().into());
    ui.set_recording_bitrate(settings.recording_bitrate.unwrap_or(0) as i32);

    ui.on_toggle_recording({
        let ui_weak = ui.as_weak();
        let shared_state = shared_state.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (recording, setup) = {
                let s = lock(&shared_state);
                (s.recording.is_some(), s.recording_setup.clone())
            };
            if recording {
                cmd_tx.send(app::state::AppCommand::StopRecording);
                return;
            }
            let station = Station::new(
                ui.get_station_name().as_str(),
                ui.get_station_url().as_str(),
            );
            cmd_tx.send(app::state::AppCommand::StartRecording {
                folder: recordings::folder(setup.dir.as_deref()),
                format: setup.format,
                bitrate: setup.bitrate,
                // The switch keeps its state but only counts with the EQ on
                with_eq: setup.with_eq && ui.get_eq_enabled(),
                cover: station_cover_png(&logo_service, &station),
            });
        }
    });

    ui.on_apply_recording_folder({
        let ui_weak = ui.as_weak();
        let state = shared_state.clone();
        move |path| {
            let Some(ui) = ui_weak.upgrade() else { return };
            apply_recording_folder(&ui, &state, &path);
        }
    });

    ui.on_restore_recording_folder({
        let ui_weak = ui.as_weak();
        let state = shared_state.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            // A folder still being checked isn't saved over it
            new_folder_check();
            save_recording_settings(&state, |s| s.recording_dir = None);
            show_recording_folder(&ui, None);
        }
    });

    ui.on_record_with_eq_toggled({
        let state = shared_state.clone();
        move |on| save_recording_settings(&state, |s| s.record_with_eq = on)
    });

    ui.on_recording_format_changed({
        let state = shared_state.clone();
        move |id| {
            let format = radiotrope_app::data::settings::RecordingFormat::from_id(id.as_str());
            save_recording_settings(&state, |s| s.recording_format = format);
        }
    });

    // 0 is Auto
    ui.on_recording_bitrate_changed({
        let state = shared_state.clone();
        move |kbps| {
            save_recording_settings(&state, |s| {
                s.recording_bitrate = (kbps > 0).then_some(kbps as u32)
            });
        }
    });

    ui.on_open_recordings_folder({
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let dir = std::path::PathBuf::from(ui.get_recording_folder().as_str());
            let ui_weak = ui_weak.clone();
            // Opening a folder that doesn't exist yet would show an error.
            // Making it can wait on a slow or sleeping drive, so it isn't
            // done on the UI thread.
            let spawned = std::thread::Builder::new()
                .name("open-recordings".into())
                .spawn(move || {
                    let opened = recordings::prepare_dir(&dir).and_then(|()| open_folder(&dir));
                    if let Err(e) = opened {
                        let _ = ui_weak.upgrade_in_event_loop(move |ui| show_folder_error(&ui, &e));
                    }
                });
            if let Err(e) = spawned {
                show_folder_error(
                    &ui,
                    &format!("Can't open the recordings folder: {}", text::io_reason(&e)),
                );
            }
        }
    });

    // A notice that stays is closed by a click on it
    ui.on_dismiss_recording_notice({
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            NOTICES.with(|n| n.borrow_mut().close());
            show_notice(&ui);
        }
    });

    ui.on_browse_recording_folder({
        let ui_weak = ui.as_weak();
        move || browse_recording_folder(ui_weak.clone(), shared_state.clone())
    });
}

/// Save a folder typed or picked in the Recording Settings dialog, or show
/// why it can't be used (the folder in use stays as it was). The folder is
/// made and checked on a thread of its own, as a slow or sleeping drive
/// can take a while, then saved back on the UI thread unless another
/// folder was asked for meanwhile.
fn apply_recording_folder(ui: &App, state: &Arc<Mutex<AppSnapshot>>, path: &str) {
    let path = std::path::PathBuf::from(path.trim());
    if path.as_os_str().is_empty() {
        ui.set_recording_folder_error("Enter a folder, or press Restore Default".into());
        return;
    }
    if !path.is_absolute() {
        ui.set_recording_folder_error("Enter the folder's full path".into());
        return;
    }
    let check = new_folder_check();
    let ui_weak = ui.as_weak();
    let state = state.clone();
    let spawned = std::thread::Builder::new()
        .name("recording-folder".into())
        .spawn(move || {
            let checked = recordings::prepare_dir(&path);
            // Choosing the default folder by hand keeps following the default
            let custom = (path != recordings::default_dir()).then_some(path);
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                if FOLDER_CHECK.get() != check {
                    return;
                }
                if let Err(e) = checked {
                    ui.set_recording_folder_error(e.into());
                    return;
                }
                save_recording_settings(&state, |s| s.recording_dir = custom.clone());
                show_recording_folder(&ui, custom.as_deref());
            });
        });
    if let Err(e) = spawned {
        ui.set_recording_folder_error(format!("Can't check the folder: {e}").into());
    }
}

/// A new recording folder was asked for (or the default restored): a
/// check still running for an earlier one is ignored when it ends
fn new_folder_check() -> u64 {
    FOLDER_CHECK.set(FOLDER_CHECK.get() + 1);
    FOLDER_CHECK.get()
}

/// Show `custom` (or the default folder) in the Recording Settings dialog.
fn show_recording_folder(ui: &App, custom: Option<&std::path::Path>) {
    let folder = recordings::folder(custom);
    let text: slint::SharedString = folder.display().to_string().into();
    ui.set_recording_folder(text.clone());
    ui.set_recording_folder_edit(text);
    ui.set_recording_default_folder(recordings::default_dir().display().to_string().into());
    ui.set_recording_folder_error(Default::default());
}

/// Change recording settings on disk right away, and in the shared state,
/// where an agent's recording takes them from.
pub(crate) fn save_recording_settings(
    state: &Mutex<AppSnapshot>,
    change: impl FnOnce(&mut radiotrope_app::data::settings::Settings),
) {
    let settings = crate::window_settings::update_settings(change);
    lock(state).recording_setup = app::state::RecordingSetup::from_settings(&settings);
}

/// The station's cached logo as PNG, for a recording's cover art: as the
/// player shows it, on its fog when it needs one
pub(crate) fn station_cover_png(logo_service: &LogoService, station: &Station) -> Option<Vec<u8>> {
    let (rgba, w, h) = logo_service.get_cached_rgba(station)?;
    let (rgba, w, h) = visual::with_backdrop(rgba, w, h);
    let img = image::RgbaImage::from_raw(w, h, rgba)?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    (png.len() <= MAX_COVER_BYTES).then_some(png)
}

/// The phones listening, for the menu bar chip (set only when they change)
pub(crate) fn show_listeners(ui: &App, listeners: &[String]) {
    use slint::Model as _;
    let shown = ui.get_listeners();
    if shown.row_count() == listeners.len()
        && shown.iter().zip(listeners).all(|(a, b)| a.as_str() == b)
    {
        return;
    }
    let names: Vec<slint::SharedString> = listeners.iter().map(|n| n.into()).collect();
    ui.set_listeners(std::rc::Rc::new(slint::VecModel::from(names)).into());
}

/// Mirror recording progress and notices into the UI (called on every
/// poll). `shown` is the last notice taken from the controller and when.
pub(crate) fn show_recording_state(
    ui: &App,
    recording: Option<&app::state::RecordingProgress>,
    notice: Option<&app::state::RecordingNotice>,
    is_playing: bool,
    shown: &mut (u64, Instant),
) {
    ui.set_is_recording(recording.is_some());
    ui.set_can_record(is_playing);
    if let Some(rec) = recording {
        ui.set_recording_text(text::clock(rec.duration.as_secs()).into());
        ui.set_recording_size(format_bytes(rec.bytes).into());
    }

    let now = Instant::now();
    let changed = NOTICES.with(|n| {
        let mut notices = n.borrow_mut();
        match notice {
            Some(n) if n.seq != shown.0 => {
                *shown = (n.seq, now);
                notices.post(&n.text, n.kind, now);
                true
            }
            _ => notices.pass(now),
        }
    });
    if changed {
        show_notice(ui);
    }
}

/// A notice under the station
#[derive(Clone, Debug, PartialEq)]
struct Notice {
    text: String,
    kind: NoticeKind,
}

/// The notices under the station: one shows at a time. One that stays
/// (an error, a missed entry) is on screen until it is clicked away; a
/// newer notice goes in front of it and it comes back afterwards, so a
/// "Saved" can't wipe out last night's "Missed". The others pass after
/// [`RECORDING_NOTICE_TIME`].
#[derive(Debug, Default)]
struct Notices {
    /// The one on screen, and since when
    shown: Option<(Notice, Instant)>,
    /// Staying notices a newer one went in front of, oldest first
    behind: Vec<Notice>,
}

impl Notices {
    /// Staying notices kept behind the one on screen
    const KEPT: usize = 8;

    /// Show a new notice
    fn post(&mut self, text: &str, kind: NoticeKind, now: Instant) {
        let notice = Notice {
            text: text.to_string(),
            kind,
        };
        // Said twice (Open Folder pressed again): once is enough
        self.behind.retain(|n| *n != notice);
        if let Some((old, _)) = self.shown.take() {
            if old.kind.stays() && old != notice {
                self.behind.push(old);
                if self.behind.len() > Self::KEPT {
                    self.behind.remove(0);
                }
            }
        }
        self.shown = Some((notice, now));
    }

    /// Time goes by: a notice that doesn't stay leaves after a while.
    /// Returns whether what is on screen changed.
    fn pass(&mut self, now: Instant) -> bool {
        match &self.shown {
            Some((notice, since))
                if !notice.kind.stays()
                    && now.saturating_duration_since(*since) >= RECORDING_NOTICE_TIME =>
            {
                self.next(now);
                true
            }
            _ => false,
        }
    }

    /// The notice on screen was clicked away
    fn close(&mut self) {
        self.next(Instant::now());
    }

    /// The newest notice waiting behind comes on screen, or none
    fn next(&mut self, now: Instant) {
        self.shown = self.behind.pop().map(|notice| (notice, now));
    }

    fn on_screen(&self) -> Option<&Notice> {
        self.shown.as_ref().map(|(notice, _)| notice)
    }
}

/// Draw the notice on screen (or none)
fn show_notice(ui: &App) {
    let notice = NOTICES.with(|n| n.borrow().on_screen().cloned());
    match notice {
        Some(notice) => {
            ui.set_recording_notice_kind(notice.kind.id().into());
            ui.set_recording_notice_stays(notice.kind.stays());
            ui.set_recording_notice(notice.text.into());
        }
        None => ui.set_recording_notice(Default::default()),
    }
}

/// The recordings folder can't be opened: said in the Recording Settings
/// dialog while it is open (beside the folder), else under the station,
/// where "Open Folder" in the Tools menu and the "Saved" notice are
fn show_folder_error(ui: &App, error: &str) {
    if ui.get_recording_dialog_open() {
        ui.set_recording_folder_error(error.into());
        return;
    }
    NOTICES.with(|n| {
        n.borrow_mut()
            .post(error, NoticeKind::Error, Instant::now())
    });
    show_notice(ui);
}

/// Let the user pick the recording folder with the system's folder dialog.
/// The choice is saved straight away.
#[cfg(feature = "desktop")]
fn browse_recording_folder(ui_weak: slint::Weak<App>, state: Arc<Mutex<AppSnapshot>>) {
    let Some(ui) = ui_weak.upgrade() else { return };
    let start = std::path::PathBuf::from(ui.get_recording_folder_edit().as_str());
    // Start in the folder being edited, or the nearest parent that exists.
    // Looking can wake a sleeping drive, so it is done off the UI thread,
    // and the dialog opened back on it.
    let spawned = std::thread::Builder::new()
        .name("recording-folder".into())
        .spawn(move || {
            let dir = start
                .ancestors()
                .find(|p| p.is_dir())
                .map(std::path::Path::to_path_buf);
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let mut dialog = rfd::AsyncFileDialog::new().set_title("Choose Recording Folder");
                if let Some(dir) = dir {
                    dialog = dialog.set_directory(dir);
                }
                dialog = dialog.set_parent(&ui.window().window_handle());
                let pick = dialog.pick_folder();
                let ui_weak = ui.as_weak();
                let spawned = slint::spawn_local(async move {
                    let picked = pick.await;
                    let Some(ui) = ui_weak.upgrade() else { return };
                    // A picked folder saves straight away, like the other settings
                    if let Some(folder) = picked {
                        apply_recording_folder(&ui, &state, &folder.path().display().to_string());
                    }
                });
                if let Err(e) = spawned {
                    eprintln!("Failed to open the folder dialog: {e}");
                }
            });
        });
    if let Err(e) = spawned {
        eprintln!("Failed to open the folder dialog: {e}");
    }
}

/// The kiosk build has no folder dialog; the path is typed instead.
#[cfg(not(feature = "desktop"))]
fn browse_recording_folder(_ui_weak: slint::Weak<App>, _state: Arc<Mutex<AppSnapshot>>) {}

/// Open a folder in the system file manager, or say why it didn't start
/// (a sentence for the user)
fn open_folder(dir: &std::path::Path) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    command.arg(explorer_path(dir));
    #[cfg(not(windows))]
    command.arg(dir);
    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to open {}: {e}", dir.display());
            Err(format!(
                "Can't open the recordings folder: {}",
                file_manager_reason(program, &e)
            ))
        }
    }
}

/// Why the file manager didn't start. "Not found" is about the program
/// that opens folders, not about the folder.
fn file_manager_reason(program: &str, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("{program} isn't installed")
    } else {
        text::io_reason(error)
    }
}

/// `dir` as Explorer takes it: given forward slashes ("C:/Users/..."), as a
/// typed or saved folder can have, it opens Documents instead
#[cfg(any(windows, test))]
fn explorer_path(dir: &std::path::Path) -> std::ffi::OsString {
    match dir.to_str() {
        Some(text) => text.replace('/', "\\").into(),
        // Not Unicode, so not typed in: left as it is
        None => dir.as_os_str().to_owned(),
    }
}

thread_local! {
    /// Counts the recording folders asked for, so only the last one asked
    /// for is saved when its check comes back
    static FOLDER_CHECK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The notices under the station
    static NOTICES: std::cell::RefCell<Notices> = std::cell::RefCell::new(Notices::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_gets_backslashes() {
        let path = |p: &str| explorer_path(std::path::Path::new(p));
        assert_eq!(
            path("C:/Users/José/Music/Radiotrope"),
            r"C:\Users\José\Music\Radiotrope"
        );
        assert_eq!(path(r"\\nas\music/Radiotrope"), r"\\nas\music\Radiotrope");
        assert_eq!(path(r"D:\Recordings"), r"D:\Recordings");
    }

    fn text_of(notices: &Notices) -> Option<&str> {
        notices.on_screen().map(|n| n.text.as_str())
    }

    #[test]
    fn a_saved_notice_passes_and_an_error_stays() {
        let start = Instant::now();
        let later = start + RECORDING_NOTICE_TIME;
        let mut notices = Notices::default();
        notices.post("Saved Jazz FM.mp3", NoticeKind::Saved, start);
        assert!(!notices.pass(later - std::time::Duration::from_millis(1)));
        assert!(notices.pass(later));
        assert_eq!(text_of(&notices), None);

        for kind in [NoticeKind::Error, NoticeKind::Scheduler] {
            notices.post("Missed 07:00: Play Jazz FM", kind, start);
            assert!(
                !notices.pass(start + RECORDING_NOTICE_TIME * 1000),
                "{kind:?}"
            );
            assert_eq!(text_of(&notices), Some("Missed 07:00: Play Jazz FM"));
            notices.close();
            assert_eq!(text_of(&notices), None);
        }

        // The Sleep Timer's news passes too
        notices.post(
            "Sleep Timer ended; recording continues",
            NoticeKind::SleepTimer,
            start,
        );
        assert!(notices.pass(later));
    }

    #[test]
    fn a_notice_that_stays_comes_back_after_a_newer_one() {
        let start = Instant::now();
        let mut notices = Notices::default();
        notices.post("Missed 07:00: Play Jazz FM", NoticeKind::Scheduler, start);
        notices.post("Saved Jazz FM.mp3", NoticeKind::Saved, start);
        assert_eq!(text_of(&notices), Some("Saved Jazz FM.mp3"));
        assert!(notices.pass(start + RECORDING_NOTICE_TIME));
        assert_eq!(text_of(&notices), Some("Missed 07:00: Play Jazz FM"));

        // Two that stay: the newer first, then the older, then none
        notices.post(
            "Recording stopped: no space left on device",
            NoticeKind::Error,
            start,
        );
        notices.close();
        assert_eq!(text_of(&notices), Some("Missed 07:00: Play Jazz FM"));
        notices.close();
        assert_eq!(text_of(&notices), None);
    }

    #[test]
    fn the_same_error_twice_is_shown_once() {
        let start = Instant::now();
        let mut notices = Notices::default();
        let error = "Can't open the recordings folder: permission denied";
        notices.post(error, NoticeKind::Error, start);
        notices.post(error, NoticeKind::Error, start);
        notices.post("Saved Jazz FM.mp3", NoticeKind::Saved, start);
        notices.post(error, NoticeKind::Error, start);
        notices.close();
        assert_eq!(text_of(&notices), None);
    }

    #[test]
    fn a_missing_file_manager_is_named() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            file_manager_reason("xdg-open", &missing),
            "xdg-open isn't installed"
        );
    }
}
