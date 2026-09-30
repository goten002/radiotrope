//! The token network agents send to use the player
//!
//! 32 random bytes as hex, made on first use and kept in its own file in
//! the config folder, apart from settings.json. On Unix the file is readable
//! by the user only (0600); on Windows the config folder (in the user's
//! AppData) already is.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::mcp::TOKEN_FILE;
use crate::data::storage;

/// The token, made and saved first if there is none yet
pub fn load_or_create() -> io::Result<String> {
    load_or_create_at(&path()?)
}

/// The token, if one was made before (never makes one)
pub fn load_existing() -> Option<String> {
    let text = std::fs::read_to_string(path().ok()?).ok()?;
    let token = text.trim();
    is_valid(token).then(|| token.to_string())
}

/// Replace the token with a new one; clients with the old one stop working
pub fn regenerate() -> io::Result<String> {
    regenerate_at(&path()?)
}

fn path() -> io::Result<PathBuf> {
    let dir = storage::ensure_config_dir().map_err(|e| io::Error::other(e.to_string()))?;
    Ok(dir.join(TOKEN_FILE))
}

pub fn load_or_create_at(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(text) if is_valid(text.trim()) => Ok(text.trim().to_string()),
        Ok(_) => regenerate_at(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => regenerate_at(path),
        Err(e) => Err(e),
    }
}

pub fn regenerate_at(path: &Path) -> io::Result<String> {
    let token = new_token()?;
    write_private(path, &token)?;
    Ok(token)
}

fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn is_valid(token: &str) -> bool {
    token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn write_private(path: &Path, token: &str) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        // A file made before by someone else's umask keeps its mode on open
        if path.exists() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut file = options.open(path)?;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")
}

/// Compare a presented token with ours in time that doesn't depend on
/// where they differ
pub fn matches(expected: &str, presented: &str) -> bool {
    let (a, b) = (expected.as_bytes(), presented.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-token-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(TOKEN_FILE)
    }

    #[test]
    fn a_token_is_made_once_and_kept() {
        let path = temp_file("keep");
        let first = load_or_create_at(&path).unwrap();
        assert!(is_valid(&first));
        assert_eq!(load_or_create_at(&path).unwrap(), first);
        let second = regenerate_at(&path).unwrap();
        assert_ne!(second, first);
        assert_eq!(load_or_create_at(&path).unwrap(), second);
    }

    #[test]
    fn a_damaged_token_file_gets_a_new_token() {
        let path = temp_file("damaged");
        fs::write(&path, "not a token").unwrap();
        assert!(is_valid(&load_or_create_at(&path).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_file("mode");
        fs::write(&path, "x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        regenerate_at(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn tokens_are_compared_whole() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "abd"));
        assert!(!matches("abc", "ab"));
        assert!(!matches("abc", ""));
    }
}
