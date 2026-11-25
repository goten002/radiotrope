//! Help's update rows and the update line in About.
//!
//! The player looks for a newer version once a day (Help > Check for
//! Updates Automatically) or when asked (Help > Check for Updates). What it
//! finds shows quietly: a dot on Help, a row in the menu and a line in
//! About, never a dialog of its own. Where the build can install the update
//! (see `radiotrope_app::update`), About's button downloads it and then
//! restarts into it, each on a click. Nothing here is reachable from MCP
//! or the phone remote.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::ComponentHandle;

use radiotrope_app::data::settings::Settings;
use radiotrope_app::update::check::{self, Found};
use radiotrope_app::update::install::{self, Applied, StageError};
use radiotrope_app::update::{self, Mode, Policy, Version};

use crate::app::state::AppSnapshot;
use crate::context::lock;
use crate::{App, Updates};

/// The first automatic check waits for the player to be up and playing
const STARTUP_DELAY: Duration = Duration::from_secs(30);
/// How often the updates thread looks whether a check is due
const DUE_CHECK: Duration = Duration::from_secs(60 * 60);

/// Help > Check for Updates Automatically, as the updates thread reads it
static AUTO_CHECK: AtomicBool = AtomicBool::new(false);

/// Where the check or the update stands
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    /// Nothing going on: only a known newer version shows
    #[default]
    Quiet,
    Checking,
    UpToDate,
    CheckFailed,
    /// Percent done, once the size is known
    Downloading(Option<u8>),
    DownloadFailed,
    /// The download was not signed by us
    NotVerified,
    /// Downloaded and checked: waiting for Restart to Update
    Ready,
    /// Restart to Update was pressed while recording
    Recording,
    Installing,
    InstallFailed,
}

impl Phase {
    /// Work that a new check or a second click must not interrupt
    fn busy(self) -> bool {
        matches!(
            self,
            Phase::Checking | Phase::Downloading(_) | Phase::Installing
        )
    }

    /// A download waits to be installed
    fn staged(self) -> bool {
        matches!(
            self,
            Phase::Ready | Phase::Recording | Phase::Installing | Phase::InstallFailed
        )
    }
}

/// What the window shows for a phase
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct View {
    /// The newer version, "" for none
    pub available: String,
    pub message: String,
    pub failed: bool,
    pub notes_url: String,
    /// "", "download" or "restart"
    pub action: &'static str,
    pub action_label: &'static str,
    pub action_enabled: bool,
}

const DOWNLOAD: &str = "Download Update";
const RESTART: &str = "Restart to Update";

/// The texts and the button for `phase`, with the newer version `known`
/// (if any) in a build of `mode`
pub(crate) fn view(phase: Phase, known: Option<&Version>, mode: Mode) -> View {
    if mode == Mode::Off {
        return View::default();
    }
    let Some(version) = known else {
        // No newer version: only the answer to a check
        let (message, failed) = match phase {
            Phase::Checking => ("Checking for updates…", false),
            Phase::UpToDate => ("Radiotrope is up to date", false),
            Phase::CheckFailed => ("Can't check for updates. Try again later.", true),
            _ => ("", false),
        };
        return View {
            message: message.into(),
            failed,
            ..View::default()
        };
    };
    let installs = mode == Mode::SelfUpdate;
    let download = |enabled| {
        if installs {
            ("download", DOWNLOAD, enabled)
        } else {
            ("", "", false)
        }
    };
    let restart = |enabled| ("restart", RESTART, enabled);
    let (message, failed, (action, action_label, action_enabled)) = match phase {
        Phase::Quiet | Phase::UpToDate => (
            format!("Version {version} is available"),
            false,
            download(true),
        ),
        Phase::Checking => ("Checking for updates…".to_string(), false, download(false)),
        Phase::CheckFailed => (
            "Can't check for updates. Try again later.".to_string(),
            true,
            download(true),
        ),
        Phase::Downloading(percent) => (
            match percent {
                Some(percent) => format!("Downloading version {version}… {percent}%"),
                None => format!("Downloading version {version}…"),
            },
            false,
            download(false),
        ),
        Phase::DownloadFailed => (
            "Can't download the update. Try again later.".to_string(),
            true,
            download(true),
        ),
        Phase::NotVerified => (
            "Can't verify the update. Nothing was installed.".to_string(),
            true,
            ("", "", false),
        ),
        Phase::Ready => (
            format!("Version {version} is ready to install"),
            false,
            restart(true),
        ),
        Phase::Recording => (
            "Stop the recording, then restart to update.".to_string(),
            false,
            restart(true),
        ),
        Phase::Installing => (
            format!("Installing version {version}…"),
            false,
            restart(false),
        ),
        Phase::InstallFailed => (
            "Can't install the update. Get it from the release page.".to_string(),
            true,
            restart(true),
        ),
    };
    View {
        available: version.to_string(),
        message,
        failed,
        notes_url: install::notes_url(version),
        action,
        action_label,
        action_enabled,
    }
}

/// Put a view in the window
pub(crate) fn show_view(ui: &App, view: &View) {
    let updates = ui.global::<Updates>();
    updates.set_available(view.available.as_str().into());
    updates.set_message(view.message.as_str().into());
    updates.set_failed(view.failed);
    updates.set_notes_url(view.notes_url.as_str().into());
    updates.set_action(view.action.into());
    updates.set_action_label(view.action_label.into());
    updates.set_action_enabled(view.action_enabled);
}

/// What the window's thread knows
#[derive(Default)]
struct State {
    phase: Phase,
    /// The newer version that is out
    known: Option<Version>,
    /// The checked download of `known`
    staged: Option<PathBuf>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

fn phase() -> Phase {
    STATE.with_borrow(|state| state.phase)
}

/// Move to `phase` and show it
fn set_phase(ui: &App, phase: Phase) {
    let view = STATE.with_borrow_mut(|state| {
        state.phase = phase;
        view(phase, state.known.as_ref(), update::policy().mode)
    });
    show_view(ui, &view);
}

/// The newer version worth showing: one that is newer than this build
fn newer_than_this(version: Option<&str>) -> Option<Version> {
    version
        .and_then(Version::parse)
        .filter(|version| *version > Version::current())
}

/// Show what is known about updates, and wire Help's rows and About's
/// button. With updates off the rows don't exist and nothing is started.
pub(crate) fn setup(ui: &App, settings: &Settings, shared_state: &Arc<Mutex<AppSnapshot>>) {
    let policy = update::policy();
    let updates = ui.global::<Updates>();
    updates.set_enabled(policy.mode != Mode::Off);
    if policy.mode == Mode::Off {
        return;
    }
    let auto = check::auto_check_on(policy.mode, settings.check_for_updates);
    AUTO_CHECK.store(auto, Ordering::Relaxed);
    updates.set_auto_check(auto);
    updates.on_auto_check_changed(|on| {
        AUTO_CHECK.store(on, Ordering::Relaxed);
        crate::window_settings::update_settings(|settings| settings.check_for_updates = Some(on));
    });

    // What an earlier check found shows at once, without asking again
    STATE.with_borrow_mut(|state| {
        state.known = newer_than_this(settings.update_available.as_deref());
    });
    set_phase(ui, Phase::Quiet);

    updates.on_check_now({
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            // A download in hand is not thrown away for a second look
            if phase().busy() || phase().staged() {
                return;
            }
            set_phase(&ui, Phase::Checking);
            let ui_weak = ui.as_weak();
            spawn("update-check", move || {
                let found = check_and_save(policy.mode);
                let _ = ui_weak.upgrade_in_event_loop(move |ui| asked_check_done(&ui, found));
            });
        }
    });

    updates.on_run_action({
        let ui_weak = ui.as_weak();
        let shared_state = shared_state.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match phase() {
                Phase::Ready | Phase::Recording | Phase::InstallFailed => {
                    restart_to_update(&ui, policy, &shared_state)
                }
                phase if phase.busy() || phase == Phase::NotVerified => {}
                _ => download(&ui, policy),
            }
        }
    });

    watch(ui);
}

/// The updates thread: clears old downloads, then looks once a day while
/// the automatic check is on. Errors are silent: the next hour tries again.
fn watch(ui: &App) {
    let ui_weak = ui.as_weak();
    let mode = update::policy().mode;
    spawn("updates", move || {
        install::clear_staged();
        std::thread::sleep(STARTUP_DELAY);
        loop {
            if AUTO_CHECK.load(Ordering::Relaxed) {
                let last = Settings::load().ok().and_then(|s| s.update_checked);
                if check::is_due(last, check::now()) {
                    // No answer (offline, say) is not worth a word
                    if let Ok(found) = check_and_save(mode) {
                        let _ =
                            ui_weak.upgrade_in_event_loop(move |ui| quiet_check_done(&ui, found));
                    }
                }
            }
            std::thread::sleep(DUE_CHECK);
        }
    });
}

fn spawn(name: &str, work: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(work) {
        eprintln!("Updates: can't start a thread: {e}");
    }
}

/// Ask the releases for the latest version, and keep the answer (when it
/// was asked, and the newer version if there is one) in settings.json
fn check_and_save(mode: Mode) -> Result<Found, check::CheckError> {
    let found = check::check(mode)?;
    let newer = match &found {
        Found::Newer(version) => Some(version.to_string()),
        Found::UpToDate => None,
    };
    // An answer that can't be saved is asked for again next time
    let _ = Settings::update(|settings| {
        settings.update_checked = Some(check::now());
        settings.update_available = newer;
    });
    Ok(found)
}

/// Take in a check's answer; false when a download is under way or in
/// hand, which the answer leaves alone
fn note_found(found: Found) -> bool {
    STATE.with_borrow_mut(|state| {
        if matches!(state.phase, Phase::Downloading(_)) || state.phase.staged() {
            return false;
        }
        state.known = match found {
            Found::Newer(version) => Some(version),
            Found::UpToDate => None,
        };
        true
    })
}

/// The answer to Help > Check for Updates
fn asked_check_done(ui: &App, found: Result<Found, check::CheckError>) {
    match found {
        Ok(found) => {
            if note_found(found) {
                set_phase(ui, Phase::UpToDate);
            }
        }
        Err(e) => {
            eprintln!("Update check: {e}");
            if phase() == Phase::Checking {
                set_phase(ui, Phase::CheckFailed);
            }
        }
    }
}

/// The answer to the daily check: shown without a word unless there is
/// something new
fn quiet_check_done(ui: &App, found: Found) {
    let phase = phase();
    if phase.busy() {
        return;
    }
    if note_found(found) {
        // An answer the user asked for stays on screen
        let keep = matches!(phase, Phase::UpToDate | Phase::CheckFailed)
            && STATE.with_borrow(|state| state.known.is_none());
        set_phase(ui, if keep { phase } else { Phase::Quiet });
    }
}

/// Download Update: fetch the release file and check its signature
fn download(ui: &App, policy: &'static Policy) {
    let Some(version) = STATE.with_borrow(|state| state.known.clone()) else {
        return;
    };
    if policy.mode != Mode::SelfUpdate {
        return;
    }
    set_phase(ui, Phase::Downloading(None));
    let ui_weak = ui.as_weak();
    spawn("update-download", move || {
        let progress_ui = ui_weak.clone();
        let mut shown = None;
        let mut progress = move |done: u64, total: Option<u64>| {
            let percent =
                total.map(|total| (done.saturating_mul(100) / total.max(1)).min(100) as u8);
            if percent != shown {
                shown = percent;
                let _ = progress_ui.upgrade_in_event_loop(move |ui| {
                    if matches!(phase(), Phase::Downloading(_)) {
                        set_phase(&ui, Phase::Downloading(percent));
                    }
                });
            }
        };
        let staged = install::stage_release(policy.mode, &policy.install, &version, &mut progress);
        let _ = ui_weak.upgrade_in_event_loop(move |ui| match staged {
            Ok(path) => {
                STATE.with_borrow_mut(|state| state.staged = Some(path));
                set_phase(&ui, Phase::Ready);
            }
            Err(e) => {
                eprintln!("Update to {version}: {e}");
                set_phase(
                    &ui,
                    match e {
                        StageError::Verify(_) => Phase::NotVerified,
                        StageError::Disabled => Phase::Quiet,
                        StageError::Network(_) | StageError::Disk(_) => Phase::DownloadFailed,
                    },
                );
            }
        });
    });
}

/// Restart to Update: install the checked download. Not while recording:
/// the restart would end the recording.
fn restart_to_update(ui: &App, policy: &'static Policy, shared_state: &Arc<Mutex<AppSnapshot>>) {
    let Some(staged) = STATE.with_borrow(|state| state.staged.clone()) else {
        set_phase(ui, Phase::Quiet);
        return;
    };
    if lock(shared_state).recording.is_some() {
        set_phase(ui, Phase::Recording);
        return;
    }
    match install::apply(&policy.install, &staged) {
        // The new AppImage starts when this player has gone
        Ok(Applied::Replaced) => crate::quit_app(),
        // The setup program closes the player (`radiotrope --quit`). If it
        // ends and we are still here, it installed nothing.
        Ok(Applied::SetupStarted(mut setup)) => {
            set_phase(ui, Phase::Installing);
            let ui_weak = ui.as_weak();
            spawn("update-setup", move || {
                let ended = setup.wait();
                eprintln!("Update: the setup program ended without closing the player ({ended:?})");
                let _ = ui_weak.upgrade_in_event_loop(|ui| {
                    if phase() == Phase::Installing {
                        set_phase(&ui, Phase::InstallFailed);
                    }
                });
            });
        }
        Err(e) => {
            eprintln!("Update: can't install {}: {e}", staged.display());
            set_phase(ui, Phase::InstallFailed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    const PHASES: [Phase; 12] = [
        Phase::Quiet,
        Phase::Checking,
        Phase::UpToDate,
        Phase::CheckFailed,
        Phase::Downloading(None),
        Phase::Downloading(Some(42)),
        Phase::DownloadFailed,
        Phase::NotVerified,
        Phase::Ready,
        Phase::Recording,
        Phase::Installing,
        Phase::InstallFailed,
    ];

    #[test]
    fn a_build_with_updates_off_shows_nothing() {
        for phase in PHASES {
            assert_eq!(view(phase, Some(&v("9.0.0")), Mode::Off), View::default());
            assert_eq!(view(phase, None, Mode::Off), View::default());
        }
    }

    #[test]
    fn nothing_is_said_until_there_is_something_to_say() {
        for mode in [Mode::Notify, Mode::SelfUpdate] {
            assert_eq!(view(Phase::Quiet, None, mode), View::default());
        }
    }

    #[test]
    fn a_check_answers_in_one_line() {
        let line = |phase| view(phase, None, Mode::Notify);
        assert_eq!(line(Phase::Checking).message, "Checking for updates…");
        assert_eq!(line(Phase::UpToDate).message, "Radiotrope is up to date");
        let failed = line(Phase::CheckFailed);
        assert_eq!(failed.message, "Can't check for updates. Try again later.");
        assert!(failed.failed);
        for phase in [Phase::Checking, Phase::UpToDate, Phase::CheckFailed] {
            let view = line(phase);
            assert_eq!(view.available, "");
            assert_eq!(view.action, "", "no button without an update");
            assert_eq!(view.notes_url, "");
        }
    }

    #[test]
    fn a_newer_version_shows_with_its_release_notes() {
        let version = v("0.2.0");
        for mode in [Mode::Notify, Mode::SelfUpdate] {
            let view = view(Phase::Quiet, Some(&version), mode);
            assert_eq!(view.available, "0.2.0");
            assert_eq!(view.message, "Version 0.2.0 is available");
            assert!(!view.failed);
            assert_eq!(
                view.notes_url,
                "https://github.com/goten002/radiotrope/releases/tag/v0.2.0"
            );
        }
    }

    #[test]
    fn only_a_self_updating_build_offers_the_download() {
        let version = v("0.2.0");
        let button = |phase, mode| {
            let view = view(phase, Some(&version), mode);
            (view.action, view.action_label, view.action_enabled)
        };
        assert_eq!(
            button(Phase::Quiet, Mode::SelfUpdate),
            ("download", "Download Update", true)
        );
        for phase in PHASES {
            if !phase.staged() {
                assert_eq!(button(phase, Mode::Notify).0, "", "{phase:?}");
            }
        }
    }

    #[test]
    fn the_button_follows_the_download() {
        let version = v("0.2.0");
        let at = |phase| view(phase, Some(&version), Mode::SelfUpdate);
        let downloading = at(Phase::Downloading(Some(42)));
        assert_eq!(downloading.message, "Downloading version 0.2.0… 42%");
        assert_eq!(
            (downloading.action, downloading.action_enabled),
            ("download", false)
        );
        assert_eq!(
            at(Phase::Downloading(None)).message,
            "Downloading version 0.2.0…"
        );

        let ready = at(Phase::Ready);
        assert_eq!(ready.message, "Version 0.2.0 is ready to install");
        assert_eq!(
            (ready.action, ready.action_label, ready.action_enabled),
            ("restart", "Restart to Update", true)
        );

        let failed = at(Phase::DownloadFailed);
        assert!(failed.failed);
        assert_eq!(
            (failed.action, failed.action_enabled),
            ("download", true),
            "to try again"
        );

        // A download that isn't ours can't be installed, or tried again
        let unverified = at(Phase::NotVerified);
        assert!(unverified.failed);
        assert_eq!(unverified.action, "");
        assert_eq!(
            unverified.message,
            "Can't verify the update. Nothing was installed."
        );

        let installing = at(Phase::Installing);
        assert_eq!(installing.message, "Installing version 0.2.0…");
        assert!(!installing.action_enabled);

        assert_eq!(
            at(Phase::Recording).message,
            "Stop the recording, then restart to update."
        );
    }

    #[test]
    fn texts_follow_the_style_guide() {
        let version = v("0.2.0");
        for phase in PHASES {
            for known in [None, Some(&version)] {
                let view = view(phase, known, Mode::SelfUpdate);
                let text = &view.message;
                assert!(!text.contains("..."), "{text}: the one-character ellipsis");
                assert!(
                    !text.contains("Cannot") && !text.contains("Failed"),
                    "{text}"
                );
                // A period on sentences, none on one-line status lines
                let sentences = text.matches(". ").count() + usize::from(text.ends_with('.'));
                assert!(sentences == 0 || text.ends_with('.'), "{text}");
                assert!(!view.action_label.contains('…') && !view.action_label.ends_with('.'));
            }
        }
    }

    #[test]
    fn only_a_newer_version_is_remembered() {
        assert_eq!(newer_than_this(None), None);
        assert_eq!(newer_than_this(Some("not a version")), None);
        assert_eq!(newer_than_this(Some(env!("CARGO_PKG_VERSION"))), None);
        assert_eq!(newer_than_this(Some("0.0.1")), None);
        assert_eq!(newer_than_this(Some("999.0.0")), Some(v("999.0.0")));
    }
}
