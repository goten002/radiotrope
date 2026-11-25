//! Update check and self-update
//!
//! What a build may do is its [`Mode`]: nothing, tell the user that a newer
//! version is out, or also download and install it. The mode is the lowest
//! of what the build allows (`RADIOTROPE_UPDATES` when it was compiled, set
//! by the packages a package manager updates), what the installed package
//! says (an `update-mode` file), what the user's environment says
//! (`RADIOTROPE_UPDATES` again, when the player starts) and what the way it
//! was installed can do: only an install by the Windows setup program and
//! an AppImage replace themselves.
//!
//! Everything that reaches the network takes the mode and does nothing
//! with [`Mode::Off`]: see [`check::check_with`] and [`install::stage`].
//! Nothing is installed without the user's click, and only a download
//! signed with the key in [`verify::PUBLIC_KEY`].

pub mod check;
pub mod install;
pub mod verify;
pub mod version;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use version::Version;

/// The releases, on GitHub
pub const RELEASES_URL: &str = "https://github.com/goten002/radiotrope/releases";

/// `RADIOTROPE_UPDATES=off|notify|self`: read when the player is compiled
/// (what the build allows at most) and again when it starts (which can only
/// lower it)
pub const UPDATES_VAR: &str = "RADIOTROPE_UPDATES";

/// The file a package or an administrator leaves to say `off` or `notify`
pub const MODE_FILE: &str = "update-mode";

/// What this build does about new versions, least first
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    /// Never looks: a package manager brings the updates
    Off,
    /// Looks, and says when a newer version is out
    Notify,
    /// Also downloads and installs it, when the user asks
    SelfUpdate,
}

impl Mode {
    /// `off`, `notify` or `self`, in any case
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Mode::Off),
            "notify" => Some(Mode::Notify),
            "self" => Some(Mode::SelfUpdate),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Notify => "notify",
            Mode::SelfUpdate => "self",
        }
    }

    /// A limit someone wrote (the environment, a mode file). A value that
    /// can't be read means off: whoever wrote it meant to hold updates back.
    fn limit(text: &str) -> Self {
        Self::parse(text).unwrap_or(Mode::Off)
    }
}

/// What the build allows at most: `RADIOTROPE_UPDATES` when it was compiled
/// (build.rs checks the value; without one, `self` on the desktop and
/// `off` in the embedded build)
pub fn build_mode() -> Mode {
    Mode::limit(env!("RADIOTROPE_UPDATES_BUILD"))
}

/// How this copy of the player got onto the computer, as far as it matters
/// for replacing it
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// By the Windows setup program (Inno Setup): the next setup program
    /// upgrades it
    WindowsSetup,
    /// Running from this AppImage file, in a folder we can write to
    AppImage(PathBuf),
    /// Anything else (a package, a tarball, the portable zip, a build from
    /// source): its files are not ours to replace
    Other,
}

impl InstallKind {
    /// What this kind of install can do at most
    pub fn most(&self) -> Mode {
        match self {
            InstallKind::WindowsSetup | InstallKind::AppImage(_) => Mode::SelfUpdate,
            InstallKind::Other => Mode::Notify,
        }
    }
}

/// Everything that decides the mode
pub struct Limits<'a> {
    pub build: Mode,
    /// `RADIOTROPE_UPDATES` in the environment
    pub env: Option<&'a str>,
    /// What the mode files say
    pub files: &'a [String],
    pub install: &'a InstallKind,
    /// A public key is compiled in: without one no download can be checked
    pub key_set: bool,
}

/// The mode: the lowest of all the limits
pub fn resolve(limits: &Limits) -> Mode {
    let mut mode = limits.build.min(limits.install.most());
    if !limits.key_set {
        mode = mode.min(Mode::Notify);
    }
    if let Some(env) = limits.env {
        mode = mode.min(Mode::limit(env));
    }
    for file in limits.files {
        mode = mode.min(Mode::limit(file));
    }
    mode
}

/// What this run does about updates
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub mode: Mode,
    pub install: InstallKind,
}

/// This run's policy, worked out once
pub fn policy() -> &'static Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    POLICY.get_or_init(|| {
        let build = build_mode();
        // An off build looks at nothing else
        if build == Mode::Off {
            return Policy {
                mode: Mode::Off,
                install: InstallKind::Other,
            };
        }
        let exe = std::env::current_exe().ok();
        let install = detect_install(exe.as_deref());
        let env = std::env::var(UPDATES_VAR).ok();
        let files: Vec<String> = mode_files(exe.as_deref())
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .collect();
        let mode = resolve(&Limits {
            build,
            env: env.as_deref(),
            files: &files,
            install: &install,
            key_set: verify::key_is_set(),
        });
        Policy { mode, install }
    })
}

/// Where a mode file may be: beside the installed files (written by the
/// package) and, on Linux and macOS, in /etc (written by an administrator)
pub fn mode_files(exe: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(unix) {
        paths.push(PathBuf::from("/etc/radiotrope").join(MODE_FILE));
    }
    if let Some(dir) = exe.and_then(Path::parent) {
        if cfg!(windows) {
            paths.push(dir.join(MODE_FILE));
        } else if let Some(file) = bundle_mode_file(exe).filter(|_| cfg!(target_os = "macos")) {
            paths.push(file);
        } else if let Some(prefix) = dir.parent() {
            // <prefix>/bin/radiotrope -> <prefix>/share/radiotrope/update-mode
            paths.push(prefix.join("share").join("radiotrope").join(MODE_FILE));
        }
    }
    paths
}

/// The mode file of a macOS app bundle:
/// `Radiotrope.app/Contents/MacOS/radiotrope` ->
/// `Radiotrope.app/Contents/Resources/update-mode`. `None` for a program
/// that isn't inside a bundle (built from source, or a package's `bin`).
fn bundle_mode_file(exe: Option<&Path>) -> Option<PathBuf> {
    let programs = exe?.parent()?;
    let contents = programs.parent()?;
    (programs.file_name()? == "MacOS" && contents.file_name()? == "Contents")
        .then(|| contents.join("Resources").join(MODE_FILE))
}

/// How the program at `exe` was installed
fn detect_install(exe: Option<&Path>) -> InstallKind {
    let Some(exe) = exe else {
        return InstallKind::Other;
    };
    if cfg!(windows) {
        return if installed_by_setup(exe) {
            InstallKind::WindowsSetup
        } else {
            InstallKind::Other
        };
    }
    if cfg!(target_os = "linux") {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        if let Some(file) = appimage_file(exe, var("APPIMAGE"), var("APPDIR")) {
            if folder_is_writable(&file) {
                return InstallKind::AppImage(file);
            }
        }
    }
    InstallKind::Other
}

/// The Windows setup program leaves its uninstaller beside the programs;
/// the portable zip has none
pub fn installed_by_setup(exe: &Path) -> bool {
    exe.parent()
        .is_some_and(|dir| dir.join("unins000.exe").is_file())
}

/// The AppImage file this program runs from. The AppImage runtime sets
/// `APPIMAGE` (the file) and `APPDIR` (where it is mounted), and programs
/// started from an AppImage inherit both: they count only when our own
/// program is inside that mount.
pub fn appimage_file(
    exe: &Path,
    appimage: Option<PathBuf>,
    appdir: Option<PathBuf>,
) -> Option<PathBuf> {
    let file = appimage.filter(|path| path.is_absolute())?;
    let mount = appdir.filter(|path| path.is_absolute())?;
    exe.starts_with(&mount).then_some(file)
}

/// The folder of `file` takes new files from us (the new AppImage is
/// written beside the old one, then renamed over it)
fn folder_is_writable(file: &Path) -> bool {
    let Some(dir) = file.parent() else {
        return false;
    };
    if !file.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: a NUL-terminated path, read only for the call
        unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        !std::fs::metadata(dir).is_ok_and(|meta| meta.permissions().readonly())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits<'a>(build: Mode, install: &'a InstallKind) -> Limits<'a> {
        Limits {
            build,
            env: None,
            files: &[],
            install,
            key_set: true,
        }
    }

    #[test]
    fn modes_are_read_in_any_case() {
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse(" Notify\n"), Some(Mode::Notify));
        assert_eq!(Mode::parse("SELF"), Some(Mode::SelfUpdate));
        assert_eq!(Mode::parse("on"), None);
        assert_eq!(Mode::parse(""), None);
        for mode in [Mode::Off, Mode::Notify, Mode::SelfUpdate] {
            assert_eq!(Mode::parse(mode.name()), Some(mode));
        }
    }

    #[test]
    fn only_the_setup_install_and_the_appimage_update_themselves() {
        let appimage = InstallKind::AppImage("/home/radio/Radiotrope.AppImage".into());
        assert_eq!(
            resolve(&limits(Mode::SelfUpdate, &InstallKind::WindowsSetup)),
            Mode::SelfUpdate
        );
        assert_eq!(
            resolve(&limits(Mode::SelfUpdate, &appimage)),
            Mode::SelfUpdate
        );
        // A deb, an rpm, a tarball, the portable zip, a build from source
        assert_eq!(
            resolve(&limits(Mode::SelfUpdate, &InstallKind::Other)),
            Mode::Notify
        );
    }

    #[test]
    fn the_build_is_the_most_a_copy_can_do() {
        let setup = InstallKind::WindowsSetup;
        assert_eq!(resolve(&limits(Mode::Notify, &setup)), Mode::Notify);
        assert_eq!(resolve(&limits(Mode::Off, &setup)), Mode::Off);
        // Nothing at run time raises it
        let raised = Limits {
            env: Some("self"),
            files: &["self".to_string()],
            ..limits(Mode::Off, &setup)
        };
        assert_eq!(resolve(&raised), Mode::Off);
        let raised = Limits {
            env: Some("self"),
            ..limits(Mode::Notify, &setup)
        };
        assert_eq!(resolve(&raised), Mode::Notify);
    }

    #[test]
    fn the_environment_and_a_mode_file_lower_it() {
        let setup = InstallKind::WindowsSetup;
        let env = |value| Limits {
            env: Some(value),
            ..limits(Mode::SelfUpdate, &setup)
        };
        assert_eq!(resolve(&env("off")), Mode::Off);
        assert_eq!(resolve(&env("notify")), Mode::Notify);
        assert_eq!(resolve(&env("self")), Mode::SelfUpdate);
        // What can't be read holds updates back
        assert_eq!(resolve(&env("no")), Mode::Off);
        assert_eq!(resolve(&env("")), Mode::Off);

        let files = ["notify\n".to_string(), "off\n".to_string()];
        let with_files = Limits {
            files: &files,
            ..limits(Mode::SelfUpdate, &setup)
        };
        assert_eq!(resolve(&with_files), Mode::Off);
        let with_file = Limits {
            files: &files[..1],
            ..limits(Mode::SelfUpdate, &setup)
        };
        assert_eq!(resolve(&with_file), Mode::Notify);
    }

    #[test]
    fn without_a_public_key_nothing_is_installed() {
        let no_key = Limits {
            key_set: false,
            ..limits(Mode::SelfUpdate, &InstallKind::WindowsSetup)
        };
        assert_eq!(resolve(&no_key), Mode::Notify);
    }

    // AppImages are a Linux thing, and so are these paths
    #[cfg(unix)]
    #[test]
    fn an_appimage_counts_only_for_the_program_inside_it() {
        let file = PathBuf::from("/home/radio/Apps/Radiotrope.AppImage");
        let mount = PathBuf::from("/tmp/.mount_RadioAbCdEf");
        let inside = mount.join("usr/bin/radiotrope");
        assert_eq!(
            appimage_file(&inside, Some(file.clone()), Some(mount.clone())),
            Some(file.clone())
        );
        // Started from another AppImage's terminal: its variables are not ours
        assert_eq!(
            appimage_file(
                Path::new("/usr/bin/radiotrope"),
                Some(file.clone()),
                Some(mount.clone())
            ),
            None
        );
        assert_eq!(appimage_file(&inside, None, Some(mount.clone())), None);
        assert_eq!(appimage_file(&inside, Some(file), None), None);
        assert_eq!(
            appimage_file(&inside, Some("Radiotrope.AppImage".into()), Some(mount)),
            None
        );
    }

    #[test]
    fn the_mode_file_is_looked_for_beside_the_installed_files() {
        let paths = mode_files(Some(Path::new(if cfg!(windows) {
            r"C:\Programs\Radiotrope\radiotrope.exe"
        } else {
            "/usr/bin/radiotrope"
        })));
        if cfg!(windows) {
            assert_eq!(
                paths,
                [PathBuf::from(r"C:\Programs\Radiotrope\update-mode")]
            );
        } else {
            assert_eq!(
                paths,
                [
                    PathBuf::from("/etc/radiotrope/update-mode"),
                    PathBuf::from("/usr/share/radiotrope/update-mode"),
                ]
            );
        }
    }

    #[test]
    fn a_mac_bundle_keeps_its_mode_file_in_resources() {
        let exe = Path::new("/Applications/Radiotrope.app/Contents/MacOS/radiotrope");
        assert_eq!(
            bundle_mode_file(Some(exe)),
            Some(PathBuf::from(
                "/Applications/Radiotrope.app/Contents/Resources/update-mode"
            ))
        );
        // Not a bundle: Homebrew's or a source build's bin folder
        for exe in ["/opt/homebrew/bin/radiotrope", "/usr/bin/radiotrope"] {
            assert_eq!(bundle_mode_file(Some(Path::new(exe))), None, "{exe}");
        }
        assert_eq!(bundle_mode_file(None), None);

        if cfg!(target_os = "macos") {
            assert_eq!(
                mode_files(Some(exe)),
                [
                    PathBuf::from("/etc/radiotrope/update-mode"),
                    PathBuf::from("/Applications/Radiotrope.app/Contents/Resources/update-mode"),
                ]
            );
        }
    }

    #[test]
    fn the_build_mode_is_one_of_the_three() {
        // build.rs refuses any other value
        let mode = env!("RADIOTROPE_UPDATES_BUILD");
        assert!(Mode::parse(mode).is_some(), "{mode:?}");
        if cfg!(all(feature = "embedded", not(feature = "desktop")))
            && option_env!("RADIOTROPE_UPDATES").is_none()
        {
            assert_eq!(build_mode(), Mode::Off);
        }
    }
}
