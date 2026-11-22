//! The log file: where the window's messages go when nobody reads stderr
//!
//! Started from the desktop's menu, the player has no terminal: on Windows
//! its output would go nowhere, on Linux to the session journal at best. It
//! then writes to a file instead, which a user can attach to a bug report:
//! - Linux: `$XDG_STATE_HOME/radiotrope/radiotrope.log`
//!   (`~/.local/state/radiotrope/radiotrope.log`)
//! - Windows: `%LOCALAPPDATA%\radiotrope\radiotrope.log`
//!
//! and in portable mode, the `data\local` folder beside the program. The
//! file is started afresh past 1 MB, the old one kept as `radiotrope.old.log`.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The log is started afresh (the old one kept as `.old.log`) past this size
pub const MAX_BYTES: u64 = 1024 * 1024;

/// The log file's name
const FILE_NAME: &str = "radiotrope.log";

/// Where this program's log goes when stderr isn't read. `None` where
/// there is none (the Pi's kiosk build logs to the journal, macOS to its
/// console), or the folders are unknown.
pub fn path() -> Option<PathBuf> {
    if !cfg!(any(windows, all(target_os = "linux", feature = "desktop"))) {
        return None;
    }
    let dirs = radiotrope_app::data::storage::data_dirs();
    let dir = if cfg!(windows) || dirs.portable.is_some() {
        dirs.local.clone()?
    } else {
        dirs::state_dir()?.join("radiotrope")
    };
    Some(dir.join(FILE_NAME))
}

/// The file the old log is kept in: `radiotrope.old.log`
pub fn old_path(path: &Path) -> PathBuf {
    path.with_extension("old.log")
}

/// Keep the log at `path` under `max_bytes`: a larger one is moved to
/// [`old_path`] (replacing an older one), so the next write starts afresh
pub fn rotate(path: &Path, max_bytes: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
        let _ = std::fs::rename(path, old_path(path));
    }
}

/// Open the log at `path` to add to, after [`rotate`], and mark the start
/// of this run in it
pub fn open(path: &Path) -> Option<File> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    rotate(path, MAX_BYTES);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let _ = writeln!(
        file,
        "--- radiotrope {} started {} (pid {})",
        env!("CARGO_PKG_VERSION"),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        std::process::id()
    );
    Some(file)
}

/// On Linux, when stderr is not a terminal (started from the desktop's
/// menu, at login, or by a service manager), send it to [`path`]: the
/// engine's messages and a panic's. Run before anything prints.
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub fn set_up() {
    use std::os::fd::AsRawFd;

    // SAFETY: isatty only reads the state of our own descriptor
    if unsafe { libc::isatty(libc::STDERR_FILENO) } == 1 {
        return;
    }
    let Some(file) = path().as_deref().and_then(open) else {
        return;
    };
    // Stderr stays open on the file for the life of the process; `file`
    // itself, a second descriptor, is closed when it goes
    // SAFETY: both descriptors are valid; dup2 replaces stderr atomically
    unsafe {
        libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
    }
}

/// A panic is logged with the time and where it happened, then reported
/// as usual. Release builds abort on a panic, so this line is often all
/// that is left to tell why the player vanished.
pub fn log_panics() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "an unknown place".into());
        eprintln!(
            "{} Radiotrope stopped with an error at {location}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        report(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder of the test's own, removed when dropped
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rt-log-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_large_log_is_kept_as_the_old_one() {
        let dir = TempDir::new("rotate");
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, vec![b'x'; 2000]).unwrap();
        std::fs::write(old_path(&path), "older").unwrap();

        rotate(&path, 1000);

        assert!(!path.exists());
        assert_eq!(std::fs::read(old_path(&path)).unwrap().len(), 2000);
        assert_eq!(
            old_path(&path).file_name().unwrap(),
            "radiotrope.old.log",
            "the old log keeps a name a file manager opens as text"
        );
    }

    #[test]
    fn a_small_log_is_added_to() {
        let dir = TempDir::new("append");
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "earlier run\n").unwrap();

        rotate(&path, 1000);
        drop(open(&path).unwrap());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("earlier run\n--- radiotrope "), "{text}");
        assert!(!old_path(&path).exists());
    }

    #[test]
    fn open_makes_the_folder() {
        let dir = TempDir::new("folder");
        let path = dir.path().join("state").join("radiotrope").join(FILE_NAME);
        assert!(open(&path).is_some());
        assert!(path.exists());
    }

    #[cfg(all(target_os = "linux", feature = "desktop"))]
    #[test]
    fn the_linux_log_is_in_the_state_folder() {
        let path = path().unwrap();
        if radiotrope_app::data::storage::data_dirs().portable.is_none() {
            assert!(path.ends_with("radiotrope/radiotrope.log"), "{path:?}");
            assert!(path.starts_with(dirs::state_dir().unwrap()), "{path:?}");
        }
    }
}
