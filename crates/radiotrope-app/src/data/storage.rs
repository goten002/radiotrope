//! Storage layer for JSON persistence
//!
//! Provides consistent file I/O for all data types.

use crate::config::app::NAME;
use crate::error::{AppError, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Get the application config directory path
pub fn config_dir() -> Result<PathBuf> {
    dirs::config_dir().map(|p| p.join(NAME)).ok_or_else(|| {
        AppError::Config(
            "Could not determine config directory. HOME environment variable may not be set."
                .to_string(),
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
fn create_dir_if_needed(path: &Path) -> Result<()> {
    match fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = match e.kind() {
                ErrorKind::PermissionDenied => {
                    format!("Permission denied: cannot create directory {:?}", path)
                }
                ErrorKind::NotFound => {
                    format!(
                        "Cannot create directory {:?}: parent path does not exist",
                        path
                    )
                }
                _ => {
                    format!("Failed to create directory {:?}: {}", path, e)
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
                "Permission denied: cannot read {:?}",
                path
            ))),
            _ => Err(AppError::Config(format!(
                "Failed to read {:?}: {}",
                path, e
            ))),
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

/// Write `content` to a new file next to `path`, flush it to the disk, then
/// move it over `path` in one step. A crash or power cut leaves either the
/// old file or the new one, never a half-written one. The file being
/// replaced is kept as `<path>.bak` first.
fn write_file(path: &Path, content: &str) -> Result<()> {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = sibling(path, &format!(".{}-{n}.tmp", std::process::id()));

    let result = write_new(&tmp, content).and_then(|()| {
        // Keep the last good save. Only a file that still reads as JSON is
        // worth keeping, so a damaged file never replaces a good backup.
        if is_json_file(path) {
            let _ = with_retries(|| fs::copy(path, backup_path(path)));
        }
        with_retries(|| fs::rename(&tmp, path))
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| {
        let msg = match e.kind() {
            ErrorKind::PermissionDenied => {
                format!("Permission denied: cannot write to {:?}", path)
            }
            ErrorKind::NotFound => {
                format!(
                    "Cannot write to {:?}: parent directory does not exist",
                    path
                )
            }
            ErrorKind::ReadOnlyFilesystem => {
                format!("Cannot write to {:?}: filesystem is read-only", path)
            }
            _ => {
                format!("Failed to write to {:?}: {}", path, e)
            }
        };
        AppError::Config(msg)
    })
}

/// Create `path` with `content` and wait until it is on the disk
fn write_new(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = fs::File::create(path)?;
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
        Ok(()) => eprintln!("{path:?} could not be read and was moved to {aside:?}"),
        Err(e) => eprintln!("{path:?} could not be read or moved aside: {e}"),
    }
}

/// Copy a file that was only partly readable to `<path>.bad-<unix time>`,
/// before a save drops the parts that couldn't be read
pub fn keep_copy(path: &Path) {
    let copy = damaged_path(path);
    match fs::copy(path, &copy) {
        Ok(_) => eprintln!("{path:?} was only partly readable; the original is kept as {copy:?}"),
        Err(e) => eprintln!("{path:?} was only partly readable and could not be copied: {e}"),
    }
}

/// Parse `path`: `Ok(None)` when it is missing or empty
fn parse_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let content = match read_file(path)? {
        Some(c) => c,
        None => return Ok(None),
    };

    // Empty file is treated as non-existent
    if content.trim().is_empty() {
        return Ok(None);
    }

    let data = serde_json::from_str(&content)
        .map_err(|e| AppError::Config(format!("Failed to parse {:?}: {}", path, e)))?;

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
            Err(e) => Some(AppError::Config(format!(
                "Failed to parse {:?}: {}",
                path, e
            ))),
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
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_if_needed(parent)?;
        }
    }

    let content = serde_json::to_string_pretty(data)
        .map_err(|e| AppError::Config(format!("Failed to serialize data: {}", e)))?;

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
                    "Permission denied: cannot delete {:?}",
                    path
                ))),
                _ => Err(AppError::Config(format!(
                    "Failed to delete {:?}: {}",
                    path, e
                ))),
            }
        }
    }
}

/// Check if a file exists at a specific path
pub fn exists_at(path: &Path) -> bool {
    path.exists()
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
}
