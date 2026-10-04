//! The token network agents send to use the player
//!
//! 32 random bytes as hex, made on first use and kept in its own file,
//! apart from settings.json: in the config folder, and on Windows in the
//! local app data folder (not the roaming one, which a domain profile
//! copies to other computers). On Unix the file is readable by the user
//! only (0600); on Windows the user's app data folder already is. A new
//! token is written next to the file and moved over it, so a crash never
//! leaves half a token (which would make a new one, locking out every
//! client).

use std::fs;
use std::io;
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
    let dir = private_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(TOKEN_FILE);
    // Earlier versions kept it in the roaming folder
    #[cfg(windows)]
    {
        if let Ok(roaming) = storage::config_dir() {
            take_over(&roaming.join(TOKEN_FILE), &path);
        }
    }
    Ok(path)
}

/// The folder the token is kept in, with the other files only the user
/// should read (the paired phones)
pub fn private_dir() -> io::Result<PathBuf> {
    #[cfg(windows)]
    let dir = dirs::data_local_dir().map(|dir| dir.join(crate::config::app::NAME));
    #[cfg(not(windows))]
    let dir = storage::config_dir().ok();
    dir.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no folder to keep the token in"))
}

/// Move the token kept at `old` to `new`, unless `new` has one already, so
/// the clients set up with it keep working
#[cfg_attr(not(windows), allow(dead_code))]
fn take_over(old: &Path, new: &Path) {
    if old == new || new.exists() {
        return;
    }
    let Ok(text) = fs::read_to_string(old) else {
        return;
    };
    let token = text.trim();
    if is_valid(token) && write_private(new, token).is_ok() {
        let _ = fs::remove_file(old);
    }
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
    storage::write_private(path, &format!("{token}\n"))
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

    #[test]
    fn a_token_write_that_never_finished_keeps_the_old_token() {
        let path = temp_file("crash");
        let token = load_or_create_at(&path).unwrap();
        // What a crash during the next write leaves: its new file, cut short
        let unfinished = path.with_file_name(format!("{TOKEN_FILE}.999-0.tmp"));
        fs::write(&unfinished, "3f").unwrap();
        assert_eq!(load_or_create_at(&path).unwrap(), token);
        // A finished write leaves no file of its own behind
        let _ = fs::remove_file(&unfinished);
        regenerate_at(&path).unwrap();
        let names: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [TOKEN_FILE]);
    }

    #[test]
    fn a_token_from_the_old_folder_is_moved_and_kept() {
        let old = temp_file("old-place");
        let new = temp_file("new-place");
        let token = load_or_create_at(&old).unwrap();
        take_over(&old, &new);
        assert_eq!(load_or_create_at(&new).unwrap(), token);
        assert!(!old.exists());
        // One already in the new folder wins, and the old one stays put
        let older = regenerate_at(&old).unwrap();
        take_over(&old, &new);
        assert_eq!(load_or_create_at(&new).unwrap(), token);
        assert_eq!(load_or_create_at(&old).unwrap(), older);
        // A damaged one isn't moved
        let fresh = temp_file("fresh-place");
        fs::write(&old, "not a token").unwrap();
        take_over(&old, &fresh);
        assert!(!fresh.exists());
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
