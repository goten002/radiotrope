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
    dirs::config_dir()
        .map(|p| p.join(NAME))
        .ok_or_else(|| AppError::Config(
            "Could not determine config directory. HOME environment variable may not be set.".to_string()
        ))
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
                    format!("Cannot create directory {:?}: parent path does not exist", path)
                }
                _ => {
                    format!("Failed to create directory {:?}: {}", path, e)
                }
            };
            Err(AppError::Config(msg))
        }
    }
}

/// Read file contents with proper error handling
fn read_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) => {
            match e.kind() {
                ErrorKind::NotFound => Ok(None),
                ErrorKind::PermissionDenied => {
                    Err(AppError::Config(format!(
                        "Permission denied: cannot read {:?}", path
                    )))
                }
                _ => {
                    Err(AppError::Config(format!(
                        "Failed to read {:?}: {}", path, e
                    )))
                }
            }
        }
    }
}

/// Write file contents with proper error handling
fn write_file(path: &Path, content: &str) -> Result<()> {
    match fs::write(path, content) {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = match e.kind() {
                ErrorKind::PermissionDenied => {
                    format!("Permission denied: cannot write to {:?}", path)
                }
                ErrorKind::NotFound => {
                    format!("Cannot write to {:?}: parent directory does not exist", path)
                }
                ErrorKind::ReadOnlyFilesystem => {
                    format!("Cannot write to {:?}: filesystem is read-only", path)
                }
                _ => {
                    format!("Failed to write to {:?}: {}", path, e)
                }
            };
            Err(AppError::Config(msg))
        }
    }
}

/// Load data from a JSON file at a specific path
///
/// Returns `None` if the file doesn't exist.
/// Returns an error if the file exists but can't be read or parsed.
pub fn load_from<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let content = match read_file(path)? {
        Some(c) => c,
        None => return Ok(None),
    };

    // Empty file is treated as non-existent
    if content.trim().is_empty() {
        return Ok(None);
    }

    let data = serde_json::from_str(&content).map_err(|e| {
        AppError::Config(format!("Failed to parse {:?}: {}", path, e))
    })?;

    Ok(Some(data))
}

/// Save data to a JSON file at a specific path
///
/// Creates parent directories if they don't exist.
pub fn save_to<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_if_needed(parent)?;
        }
    }

    let content = serde_json::to_string_pretty(data).map_err(|e| {
        AppError::Config(format!("Failed to serialize data: {}", e))
    })?;

    write_file(path, &content)
}

/// Delete a file at a specific path
pub fn delete_at(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            match e.kind() {
                ErrorKind::NotFound => Ok(()), // Already gone, that's fine
                ErrorKind::PermissionDenied => {
                    Err(AppError::Config(format!(
                        "Permission denied: cannot delete {:?}", path
                    )))
                }
                _ => {
                    Err(AppError::Config(format!(
                        "Failed to delete {:?}: {}", path, e
                    )))
                }
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

        let _ = fs::remove_file(&path);
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
            .join(format!("radiotrope_test_{}", TEST_COUNTER.fetch_add(1, Ordering::SeqCst)))
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

        let _ = fs::remove_file(&path);
    }
}
