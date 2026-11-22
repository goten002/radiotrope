//! Storage layer for JSON persistence
//!
//! Provides consistent file I/O for all data types.

use crate::config::app::NAME;
use crate::error::{AppError, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

/// Files that, next to the program, turn on portable mode. Explorer hides
/// file extensions by default, so a "portable" made from a new text file is
/// really `portable.txt`: both count.
pub const PORTABLE_MARKERS: [&str; 2] = ["portable", "portable.txt"];

/// The folder beside the program that holds everything in portable mode
pub const PORTABLE_DATA: &str = "data";

/// Where the app keeps its files. Every place that saves something takes
/// its folder from here, so portable mode moves all of it at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    /// Settings, favorites, stats, schedules: what to back up
    pub config: Option<PathBuf>,
    /// Logos and directory answers: safe to delete
    pub cache: Option<PathBuf>,
    /// Files that stay on this computer: the log, the instance lock, the
    /// agent token, paired phones. The local (not roaming) app data folder
    /// on Windows; the config folder elsewhere.
    pub local: Option<PathBuf>,
    /// In portable mode, the `data` folder beside the program
    pub portable: Option<PathBuf>,
}

/// The system's folders, before portable mode is considered
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemDirs {
    pub config: Option<PathBuf>,
    pub cache: Option<PathBuf>,
    pub local: Option<PathBuf>,
}

impl SystemDirs {
    /// This user's folders, each with our own subfolder:
    /// - Linux: `~/.config/radiotrope`, `~/.cache/radiotrope`, local = config
    /// - macOS: `~/Library/Application Support/radiotrope`,
    ///   `~/Library/Caches/radiotrope`, local = config
    /// - Windows: `%APPDATA%\radiotrope`, `%LOCALAPPDATA%\radiotrope` for
    ///   both the caches and the local files
    pub fn current() -> Self {
        let config = dirs::config_dir().map(|p| p.join(NAME));
        #[cfg(windows)]
        let local = dirs::data_local_dir().map(|p| p.join(NAME));
        #[cfg(not(windows))]
        let local = config.clone();
        Self {
            config,
            cache: dirs::cache_dir().map(|p| p.join(NAME)),
            local,
        }
    }
}

impl DataDirs {
    /// The folders to use: all inside `portable` when given, the system's
    /// otherwise
    pub fn choose(portable: Option<&Path>, system: SystemDirs) -> Self {
        match portable {
            Some(root) => Self {
                config: Some(root.join("config")),
                cache: Some(root.join("cache")),
                local: Some(root.join("local")),
                portable: Some(root.to_path_buf()),
            },
            None => Self {
                config: system.config,
                cache: system.cache,
                local: system.local,
                portable: None,
            },
        }
    }
}

/// The portable data folder for a program in `program_dir`: its `data`
/// subfolder when a marker file sits next to the program
pub fn portable_root_in(program_dir: &Path) -> Option<PathBuf> {
    PORTABLE_MARKERS
        .iter()
        .any(|marker| program_dir.join(marker).is_file())
        .then(|| program_dir.join(PORTABLE_DATA))
}

/// The portable data folder, when this program runs in portable mode.
/// Only the Windows build has one: elsewhere the program sits in a shared
/// system folder such as `/usr/bin`.
fn portable_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let program = std::env::current_exe().ok()?;
        portable_root_in(program.parent()?)
    }
    #[cfg(not(windows))]
    None
}

/// This process's folders, worked out once
pub fn data_dirs() -> &'static DataDirs {
    static DIRS: OnceLock<DataDirs> = OnceLock::new();
    DIRS.get_or_init(|| DataDirs::choose(portable_root().as_deref(), SystemDirs::current()))
}

/// Get the application cache directory path (see [`data_dirs`])
pub fn cache_dir() -> Result<PathBuf> {
    data_dirs().cache.clone().ok_or_else(|| {
        AppError::Config("Can't find the cache folder. Check that HOME is set.".to_string())
    })
}

/// Ensure the cache directory exists
pub fn ensure_cache_dir() -> Result<PathBuf> {
    let dir = cache_dir()?;
    fs::create_dir_all(&dir)
        .map_err(|e| AppError::Config(format!("Can't create cache directory {:?}: {}", dir, e)))?;
    Ok(dir)
}

/// Get the application config directory path (see [`data_dirs`])
pub fn config_dir() -> Result<PathBuf> {
    data_dirs().config.clone().ok_or_else(|| {
        AppError::Config("Can't find the settings folder. Check that HOME is set.".to_string())
    })
}

/// The folder for files that stay on this computer (see [`DataDirs::local`])
pub fn local_dir() -> Result<PathBuf> {
    data_dirs().local.clone().ok_or_else(|| {
        AppError::Config(
            "Can't find the local app data folder. Check that HOME is set.".to_string(),
        )
    })
}

/// Ensure the config directory exists, creating it if necessary
pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = config_dir()?;
    create_dir_if_needed(&dir)?;
    Ok(dir)
}

/// Get path to a specific data file in the default config directory
pub fn data_path(filename: &str) -> Result<PathBuf> {
    Ok(config_dir()?.join(filename))
}

// =============================================================================
// Path-based functions (for testing and custom locations)
// =============================================================================

/// Create a directory if it doesn't exist, with proper error handling
pub(super) fn create_dir_if_needed(path: &Path) -> Result<()> {
    match fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = match e.kind() {
                ErrorKind::PermissionDenied => {
                    format!("Can't create directory {:?}: permission denied", path)
                }
                ErrorKind::NotFound => {
                    format!(
                        "Can't create directory {:?}: parent path does not exist",
                        path
                    )
                }
                _ => {
                    format!("Can't create directory {:?}: {}", path, e)
                }
            };
            Err(AppError::Config(msg))
        }
    }
}

/// How often a read or write is tried before giving up. On Windows another
/// program (antivirus, OneDrive, a backup tool) can hold the file open for a
/// moment, and the next try usually works.
const IO_TRIES: u32 = 5;
const IO_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Run `op` up to [`IO_TRIES`] times while it fails with an error a short
/// wait may clear. A missing file or a read-only disk is reported at once.
fn with_retries<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut tries = 1;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(e)
                if tries < IO_TRIES
                    && !matches!(
                        e.kind(),
                        ErrorKind::NotFound
                            | ErrorKind::ReadOnlyFilesystem
                            | ErrorKind::StorageFull
                            | ErrorKind::InvalidData
                    ) =>
            {
                tries += 1;
                std::thread::sleep(IO_RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Read file contents with proper error handling
fn read_file(path: &Path) -> Result<Option<String>> {
    match with_retries(|| fs::read_to_string(path)) {
        Ok(content) => Ok(Some(content)),
        Err(e) => match e.kind() {
            ErrorKind::NotFound => Ok(None),
            ErrorKind::PermissionDenied => Err(AppError::Config(format!(
                "Can't read {:?}: permission denied",
                path
            ))),
            _ => Err(AppError::Config(format!("Can't read {:?}: {}", path, e))),
        },
    }
}

/// `<path><suffix>`, e.g. `favorites.json.bak`
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The copy of the previous good save, used when the file itself is damaged
pub fn backup_path(path: &Path) -> PathBuf {
    sibling(path, ".bak")
}

/// A file as it was at one moment: when it last changed, its size and, on
/// Unix, its inode. Every save puts a new file in place (see
/// [`write_file`]), so a save by any process changes the stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileStamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            #[cfg(unix)]
            ino: std::os::unix::fs::MetadataExt::ino(meta),
        }
    }
}

/// The stamp of `path` now, or `None` when it is missing or can't be read
pub fn stamp(path: &Path) -> Option<FileStamp> {
    fs::metadata(path).ok().map(|meta| FileStamp::of(&meta))
}

/// Write `content` to a new file next to `path`, flush it to the disk, then
/// move it over `path` in one step. A crash or power cut leaves either the
/// old file or the new one, never a half-written one. The file being
/// replaced is kept as `<path>.bak` first. Returns the new file's stamp.
fn write_file(path: &Path, content: &str) -> Result<FileStamp> {
    let tmp = temp_sibling(path);

    let result = write_new(&tmp, content, false).and_then(|()| {
        // Taken before the move, which keeps it: taken after, it could
        // already be another process's save
        let stamp = FileStamp::of(&with_retries(|| fs::metadata(&tmp))?);
        // Keep the last good save. Only a file that still reads as JSON is
        // worth keeping, so a damaged file never replaces a good backup.
        if is_json_file(path) {
            let _ = with_retries(|| fs::copy(path, backup_path(path)));
        }
        with_retries(|| fs::rename(&tmp, path))?;
        Ok(stamp)
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| {
        let msg = match e.kind() {
            ErrorKind::PermissionDenied => {
                format!("Can't write to {:?}: permission denied", path)
            }
            ErrorKind::NotFound => {
                format!("Can't write to {:?}: parent directory does not exist", path)
            }
            ErrorKind::ReadOnlyFilesystem => {
                format!("Can't write to {:?}: filesystem is read-only", path)
            }
            _ => {
                format!("Can't write to {:?}: {}", path, e)
            }
        };
        AppError::Config(msg)
    })
}

/// Replace `path` with `content` in one step, the way [`write_file`] saves
/// the JSON files, but with no backup copy and, on Unix, readable by the
/// user only (0600). For a small private file: the agents' token. A crash
/// leaves the old file or the new one, never a half-written one.
pub fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = temp_sibling(path);
    let result =
        write_new(&tmp, content, true).and_then(|()| with_retries(|| fs::rename(&tmp, path)));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A name next to `path` for the new file of a save, unique to this
/// process and this save
fn temp_sibling(path: &Path) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    sibling(path, &format!(".{}-{n}.tmp", std::process::id()))
}

/// Create `path` with `content` and wait until it is on the disk. A
/// `private` file is readable by the user only (on Unix; on Windows the
/// user's own folders already are).
fn write_new(path: &Path, content: &str, private: bool) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = fs::File::create(path)?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        // Before anything is written; a file left by a crash keeps its
        // mode on create
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = private;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

/// Whether `path` holds JSON that parses (whatever its shape)
fn is_json_file(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .is_some_and(|c| serde_json::from_str::<serde_json::Value>(&c).is_ok())
}

/// `<path>.bad-<unix time>`: where a damaged file is kept
fn damaged_path(path: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    sibling(path, &format!(".bad-{secs}"))
}

/// Move a damaged file aside as `<path>.bad-<unix time>`, so the next save
/// can't overwrite what may still be recovered by hand
fn set_aside(path: &Path) {
    let aside = damaged_path(path);
    match fs::rename(path, &aside) {
        Ok(()) => eprintln!("{path:?} can't be read and was moved to {aside:?}"),
        Err(e) => eprintln!("{path:?} can't be read or moved aside: {e}"),
    }
}

/// Copy a file that was only partly readable to `<path>.bad-<unix time>`,
/// before a save drops the parts that couldn't be read
pub fn keep_copy(path: &Path) {
    let copy = damaged_path(path);
    match fs::copy(path, &copy) {
        Ok(_) => eprintln!("{path:?} was only partly readable; the original is kept as {copy:?}"),
        Err(e) => eprintln!("{path:?} was only partly readable and can't be copied: {e}"),
    }
}

/// Parse `path`: `Ok(None)` when it is missing or empty. Unlike
/// [`load_from`], a damaged file is only an error: no backup, nothing moved.
pub fn parse_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let content = match read_file(path)? {
        Some(c) => c,
        None => return Ok(None),
    };

    // Empty file is treated as non-existent
    if content.trim().is_empty() {
        return Ok(None);
    }

    let data = serde_json::from_str(&content)
        .map_err(|e| AppError::Config(format!("Can't parse {:?}: {}", path, e)))?;

    Ok(Some(data))
}

/// Load data from a JSON file at a specific path
///
/// Returns `None` if the file doesn't exist.
///
/// A file that is empty or doesn't parse (a crash during a save by an older
/// version, a disk error, a hand edit) falls back to the backup of the last
/// good save. A file that doesn't parse is moved aside first, so it is
/// never saved over. Without a usable backup a damaged file is an error.
pub fn load_from<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    // A file that can't be read at all (permissions) is an error, and left alone
    let parse_error = match read_file(path)? {
        None => return Ok(None),
        Some(content) if content.trim().is_empty() => None,
        Some(content) => match serde_json::from_str(&content) {
            Ok(data) => return Ok(Some(data)),
            Err(e) => Some(AppError::Config(format!("Can't parse {:?}: {}", path, e))),
        },
    };

    let backup = backup_path(path);
    let restored = parse_file::<T>(&backup).ok().flatten();

    if parse_error.is_some() {
        set_aside(path);
    }
    match (restored, parse_error) {
        (Some(data), _) => {
            eprintln!("{path:?} was damaged or empty; using the backup {backup:?}");
            Ok(Some(data))
        }
        (None, Some(e)) => Err(e),
        (None, None) => Ok(None),
    }
}

/// Save data to a JSON file at a specific path
///
/// Creates parent directories if they don't exist. The write is atomic and
/// the previous file is kept as a backup (see [`write_file`]).
pub fn save_to<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    save_to_stamped(path, data).map(|_| ())
}

/// [`save_to`], returning the saved file's stamp
pub fn save_to_stamped<T: Serialize>(path: &Path, data: &T) -> Result<FileStamp> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_if_needed(parent)?;
        }
    }

    let content = serde_json::to_string_pretty(data)
        .map_err(|e| AppError::Config(format!("Can't serialize data: {}", e)))?;

    write_file(path, &content)
}

/// Delete a file at a specific path
pub fn delete_at(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            match e.kind() {
                ErrorKind::NotFound => Ok(()), // Already gone, that's fine
                ErrorKind::PermissionDenied => Err(AppError::Config(format!(
                    "Can't delete {:?}: permission denied",
                    path
                ))),
                _ => Err(AppError::Config(format!("Can't delete {:?}: {}", path, e))),
            }
        }
    }
}

/// Check if a file exists at a specific path
pub fn exists_at(path: &Path) -> bool {
    path.exists()
}

// =============================================================================
// Clean-up of the settings folder
// =============================================================================

/// A save's new file older than this was left by a crash between the write
/// and the move (see [`write_file`]); no save takes this long
const STALE_TEMP: Duration = Duration::from_secs(10 * 60);

/// How many damaged copies (`<file>.bad-<time>`) are kept per file
const DAMAGED_KEPT: usize = 3;

/// Damaged copies older than this are dropped even when fewer are kept
const DAMAGED_MAX_AGE: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// Whether `name` is a save's new file: `<file>.<pid>-<n>.tmp`
fn is_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(".tmp") else {
        return false;
    };
    let Some((_, tag)) = rest.rsplit_once('.') else {
        return false;
    };
    let Some((pid, n)) = tag.split_once('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(pid) && digits(n)
}

/// `(file, unix time)` for a damaged copy named `<file>.bad-<unix time>`
fn damaged_name(name: &str) -> Option<(&str, u64)> {
    let (file, secs) = name.rsplit_once(".bad-")?;
    Some((file, secs.parse().ok()?))
}

/// Remove what saves leave behind in `dir` and nobody reads again:
/// - new files of a save that never finished (a crash between the write and
///   the move), once older than 10 minutes
/// - damaged copies (`<file>.bad-<time>`) beyond the three newest of each
///   file, and any older than 90 days
///
/// `now` is the current time. Returns how many files were removed.
pub fn tidy_folder(dir: &Path, now: SystemTime) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    let mut remove = |path: &Path| match fs::remove_file(path) {
        Ok(()) => removed += 1,
        Err(e) => eprintln!("Can't remove {path:?}: {e}"),
    };
    let mut damaged: std::collections::HashMap<String, Vec<(u64, PathBuf)>> =
        std::collections::HashMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_file = entry.file_type().is_ok_and(|t| t.is_file());
        if !is_file {
            continue;
        }
        if is_temp_name(&name) {
            let modified = entry.metadata().and_then(|m| m.modified());
            let age = modified.map(|m| now.duration_since(m).unwrap_or_default());
            if age.is_ok_and(|age| age > STALE_TEMP) {
                remove(&entry.path());
            }
        } else if let Some((file, secs)) = damaged_name(&name) {
            damaged
                .entry(file.to_string())
                .or_default()
                .push((secs, entry.path()));
        }
    }
    let now_secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for mut copies in damaged.into_values() {
        copies.sort_by_key(|c| std::cmp::Reverse(c.0));
        for (i, (secs, path)) in copies.iter().enumerate() {
            let too_old = now_secs.saturating_sub(*secs) > DAMAGED_MAX_AGE.as_secs();
            if i >= DAMAGED_KEPT || too_old {
                remove(path);
            }
        }
    }
    removed
}

// =============================================================================
// Convenience functions (use default config directory)
// =============================================================================

/// Load data from a JSON file in the config directory
pub fn load<T: DeserializeOwned>(filename: &str) -> Result<Option<T>> {
    let path = data_path(filename)?;
    load_from(&path)
}

/// Save data to a JSON file in the config directory
///
/// Creates the config directory if it doesn't exist.
pub fn save<T: Serialize>(filename: &str, data: &T) -> Result<()> {
    let path = data_path(filename)?;
    save_to(&path, data)
}

/// Delete a data file from the config directory
pub fn delete(filename: &str) -> Result<()> {
    let path = data_path(filename)?;
    delete_at(&path)
}

/// Check if a data file exists in the config directory
pub fn exists(filename: &str) -> Result<bool> {
    let path = data_path(filename)?;
    Ok(exists_at(&path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::env::temp_dir;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        temp_dir().join(format!("radiotrope_test_{}_{}.json", id, name))
    }

    fn system() -> SystemDirs {
        SystemDirs {
            config: Some(PathBuf::from("/home/u/AppData/Roaming/radiotrope")),
            cache: Some(PathBuf::from("/home/u/AppData/Local/radiotrope")),
            local: Some(PathBuf::from("/home/u/AppData/Local/radiotrope")),
        }
    }

    #[test]
    fn without_a_marker_the_system_folders_are_used() {
        let dirs = DataDirs::choose(None, system());
        assert_eq!(dirs.config, system().config);
        assert_eq!(dirs.cache, system().cache);
        assert_eq!(dirs.local, system().local);
        assert_eq!(dirs.portable, None);
    }

    #[test]
    fn portable_mode_keeps_everything_beside_the_program() {
        let root = PathBuf::from("/media/stick/Radiotrope/data");
        let dirs = DataDirs::choose(Some(&root), system());
        assert_eq!(dirs.portable.as_deref(), Some(root.as_path()));
        // No folder of the user's profile is left
        for dir in [&dirs.config, &dirs.cache, &dirs.local] {
            let dir = dir.as_ref().unwrap();
            assert!(dir.starts_with(&root), "{dir:?}");
            assert!(!dir.to_string_lossy().contains("AppData"), "{dir:?}");
        }
        // Settings, cache and local files stay apart
        assert_ne!(dirs.config, dirs.cache);
        assert_ne!(dirs.cache, dirs.local);
        assert_ne!(dirs.config, dirs.local);
    }

    #[test]
    fn portable_mode_works_without_a_home_folder() {
        let root = PathBuf::from("/opt/radiotrope/data");
        let dirs = DataDirs::choose(Some(&root), SystemDirs::default());
        assert_eq!(dirs.config, Some(root.join("config")));
        assert_eq!(dirs.cache, Some(root.join("cache")));
        assert_eq!(dirs.local, Some(root.join("local")));
    }

    #[test]
    fn a_marker_file_next_to_the_program_turns_on_portable_mode() {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let program_dir =
            temp_dir().join(format!("radiotrope_portable_{}_{id}", std::process::id()));
        let _ = fs::remove_dir_all(&program_dir);
        fs::create_dir_all(&program_dir).unwrap();

        assert_eq!(portable_root_in(&program_dir), None);
        // A folder of that name is not a marker
        fs::create_dir(program_dir.join("portable")).unwrap();
        assert_eq!(portable_root_in(&program_dir), None);
        fs::remove_dir(program_dir.join("portable")).unwrap();

        for marker in PORTABLE_MARKERS {
            fs::write(program_dir.join(marker), b"").unwrap();
            assert_eq!(
                portable_root_in(&program_dir),
                Some(program_dir.join(PORTABLE_DATA)),
                "{marker}"
            );
            fs::remove_file(program_dir.join(marker)).unwrap();
        }
        let _ = fs::remove_dir_all(&program_dir);
    }

    #[test]
    fn the_paths_used_everywhere_come_from_data_dirs() {
        let dirs = data_dirs();
        assert_eq!(config_dir().ok(), dirs.config);
        assert_eq!(cache_dir().ok(), dirs.cache);
        assert_eq!(local_dir().ok(), dirs.local);
        // The test program has no marker beside it
        assert_eq!(dirs.portable, None);
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestData {
        name: String,
        value: i32,
    }

    #[test]
    fn test_save_and_load() {
        let path = temp_path("save_load");
        let data = TestData {
            name: "test".to_string(),
            value: 42,
        };

        // Save
        save_to(&path, &data).unwrap();
        assert!(path.exists());

        // Load
        let loaded: Option<TestData> = load_from(&path).unwrap();
        assert_eq!(loaded, Some(data));

        // Cleanup
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_load_nonexistent() {
        let path = temp_path("nonexistent");
        let loaded: Option<TestData> = load_from(&path).unwrap();
        assert_eq!(loaded, None);
    }

    #[test]
    fn test_load_empty_file() {
        let path = temp_path("empty");
        fs::write(&path, "").unwrap();

        let loaded: Option<TestData> = load_from(&path).unwrap();
        assert_eq!(loaded, None);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_load_invalid_json() {
        let path = temp_path("invalid");
        fs::write(&path, "not valid json").unwrap();

        let result: Result<Option<TestData>> = load_from(&path);
        assert!(result.is_err());

        cleanup(&path);
    }

    #[test]
    fn test_delete() {
        let path = temp_path("delete");
        fs::write(&path, "test").unwrap();
        assert!(path.exists());

        delete_at(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn test_delete_nonexistent() {
        let path = temp_path("delete_nonexistent");
        // Should not error
        delete_at(&path).unwrap();
    }

    #[test]
    fn test_exists() {
        let path = temp_path("exists");

        assert!(!exists_at(&path));

        fs::write(&path, "test").unwrap();
        assert!(exists_at(&path));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_creates_parent_dirs() {
        let path = temp_dir()
            .join(format!(
                "radiotrope_test_{}",
                TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
            ))
            .join("subdir")
            .join("data.json");

        let data = TestData {
            name: "nested".to_string(),
            value: 100,
        };

        save_to(&path, &data).unwrap();
        assert!(path.exists());

        // Cleanup
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent.parent().unwrap());
        }
    }

    #[test]
    fn test_error_messages_contain_path() {
        let path = temp_path("error_test");
        fs::write(&path, "invalid json").unwrap();

        let result: Result<Option<TestData>> = load_from(&path);
        let err_msg = result.unwrap_err().to_string();

        // Error should mention the file path
        assert!(err_msg.contains("error_test") || err_msg.contains("radiotrope_test"));

        cleanup(&path);
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(backup_path(path));
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
            let prefix = format!("{}.bad-", name.to_string_lossy());
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    fn damaged_copies(path: &Path) -> usize {
        let prefix = format!("{}.bad-", path.file_name().unwrap().to_string_lossy());
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .count()
    }

    fn data(value: i32) -> TestData {
        TestData {
            name: "test".to_string(),
            value,
        }
    }

    #[test]
    fn a_save_keeps_the_previous_file_as_backup() {
        let path = temp_path("backup");
        save_to(&path, &data(1)).unwrap();
        assert!(!backup_path(&path).exists());

        save_to(&path, &data(2)).unwrap();
        let backup: Option<TestData> = parse_file(&backup_path(&path)).unwrap();
        assert_eq!(backup, Some(data(1)));
        assert_eq!(load_from::<TestData>(&path).unwrap(), Some(data(2)));
        cleanup(&path);
    }

    #[test]
    fn a_save_returns_the_stamp_of_the_file_it_left() {
        let path = temp_path("stamp");
        assert_eq!(stamp(&path), None);
        let first = save_to_stamped(&path, &data(1)).unwrap();
        assert_eq!(stamp(&path), Some(first));
        // Another save, which also copies the first to the backup
        let second = save_to_stamped(&path, &data(1000)).unwrap();
        assert_eq!(stamp(&path), Some(second));
        assert_ne!(first, second);
        cleanup(&path);
    }

    #[test]
    fn a_save_leaves_no_temp_files() {
        let dir = temp_dir().join(format!(
            "radiotrope_test_{}_tmp",
            TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let path = dir.join("data.json");
        save_to(&path, &data(1)).unwrap();
        save_to(&path, &data(2)).unwrap();
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["data.json", "data.json.bak"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_file_falls_back_to_the_backup() {
        let path = temp_path("truncated");
        save_to(&path, &data(1)).unwrap();
        save_to(&path, &data(2)).unwrap();
        // What an older version left after a crash in the middle of a save
        fs::write(&path, "{ \"name\": \"te").unwrap();

        assert_eq!(load_from::<TestData>(&path).unwrap(), Some(data(1)));
        // The damaged file is kept aside, not left to be saved over
        assert!(!path.exists());
        assert_eq!(damaged_copies(&path), 1);
        cleanup(&path);
    }

    #[test]
    fn an_empty_file_falls_back_to_the_backup() {
        let path = temp_path("emptied");
        save_to(&path, &data(1)).unwrap();
        save_to(&path, &data(2)).unwrap();
        fs::write(&path, "").unwrap();

        assert_eq!(load_from::<TestData>(&path).unwrap(), Some(data(1)));
        cleanup(&path);
    }

    #[test]
    fn a_damaged_file_never_replaces_a_good_backup() {
        let path = temp_path("bad_backup");
        save_to(&path, &data(1)).unwrap();
        save_to(&path, &data(2)).unwrap();
        fs::write(&path, "not json").unwrap();
        // A save over the damaged file keeps the good backup
        save_to(&path, &data(3)).unwrap();

        let backup: Option<TestData> = parse_file(&backup_path(&path)).unwrap();
        assert_eq!(backup, Some(data(1)));
        cleanup(&path);
    }

    #[test]
    fn a_damaged_file_without_backup_is_an_error_and_kept_aside() {
        let path = temp_path("no_backup");
        fs::write(&path, "{ broken").unwrap();

        assert!(load_from::<TestData>(&path).is_err());
        assert_eq!(damaged_copies(&path), 1);
        // The next save starts a new file; the damaged one stays where it is
        save_to(&path, &data(5)).unwrap();
        assert_eq!(damaged_copies(&path), 1);
        assert_eq!(load_from::<TestData>(&path).unwrap(), Some(data(5)));
        cleanup(&path);
    }

    fn tidy_dir(name: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = temp_dir().join(format!(
            "radiotrope_tidy_{}_{}_{name}",
            std::process::id(),
            id
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn temp_files_of_a_save_are_recognized() {
        assert!(is_temp_name("favorites.json.1234-0.tmp"));
        assert!(is_temp_name("settings.json.7-12.tmp"));
        assert!(!is_temp_name("favorites.json"));
        assert!(!is_temp_name("notes.tmp"));
        assert!(!is_temp_name("favorites.json.x-1.tmp"));
        assert!(!is_temp_name("favorites.json.12.tmp"));
    }

    #[test]
    fn tidying_removes_temp_files_only_once_they_are_stale() {
        let dir = tidy_dir("temp");
        fs::write(dir.join("favorites.json.99-0.tmp"), "{").unwrap();
        fs::write(dir.join("favorites.json"), "{}").unwrap();
        fs::write(dir.join("mine.tmp"), "x").unwrap();

        // A save may be writing it right now
        assert_eq!(tidy_folder(&dir, SystemTime::now()), 0);
        assert_eq!(names(&dir).len(), 3);

        let later = SystemTime::now() + Duration::from_secs(11 * 60);
        assert_eq!(tidy_folder(&dir, later), 1);
        assert_eq!(names(&dir), ["favorites.json", "mine.tmp"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tidying_keeps_the_three_newest_damaged_copies_of_each_file() {
        let dir = tidy_dir("damaged");
        let now = SystemTime::now();
        let now_secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for age_days in [1, 2, 3, 4, 5] {
            let secs = now_secs - age_days * 24 * 60 * 60;
            fs::write(dir.join(format!("favorites.json.bad-{secs}")), "x").unwrap();
        }
        let settings_secs = now_secs - 10;
        fs::write(dir.join(format!("settings.json.bad-{settings_secs}")), "x").unwrap();

        assert_eq!(tidy_folder(&dir, now), 2);
        let day = 24 * 60 * 60;
        let mut expected = vec![
            format!("favorites.json.bad-{}", now_secs - day),
            format!("favorites.json.bad-{}", now_secs - 2 * day),
            format!("favorites.json.bad-{}", now_secs - 3 * day),
            format!("settings.json.bad-{settings_secs}"),
        ];
        expected.sort();
        assert_eq!(names(&dir), expected);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tidying_drops_damaged_copies_older_than_90_days() {
        let dir = tidy_dir("old");
        let now = SystemTime::now();
        let now_secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let day = 24 * 60 * 60;
        let recent = format!("stats.json.bad-{}", now_secs - 89 * day);
        fs::write(dir.join(&recent), "x").unwrap();
        fs::write(
            dir.join(format!("stats.json.bad-{}", now_secs - 91 * day)),
            "x",
        )
        .unwrap();

        assert_eq!(tidy_folder(&dir, now), 1);
        assert_eq!(names(&dir), [recent]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tidying_a_missing_folder_does_nothing() {
        let dir = temp_dir().join("radiotrope_tidy_no_such_folder");
        assert_eq!(tidy_folder(&dir, SystemTime::now()), 0);
    }
}
