//! Recording folder and file names
//!
//! Where recordings go (`<Music>/Radiotrope` unless the user picked another
//! folder) and how files are named, with names that are valid on Linux,
//! Windows and macOS.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::config::app::NAME;

/// Folder name under the user's Music folder
const FOLDER_NAME: &str = "Radiotrope";

/// Longest station name kept in a file name, in UTF-8 bytes (cut at a
/// character). With the date and a number added the name stays well under
/// the 255-byte limit of Linux file systems, which 80 characters of CJK
/// text passed, and full paths under the 260-character limit of older
/// Windows APIs.
const MAX_NAME_BYTES: usize = 120;

/// Names Windows reserves for devices, with or without an extension.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The default recording folder: `<Music>/Radiotrope`.
///
/// Uses the OS Music folder (XDG `MUSIC` dir on Linux, the Music known
/// folder on Windows, `~/Music` on macOS), then `~/Music` when the OS has
/// none configured (minimal Linux images such as the Pi), then the app's
/// data folder.
pub fn default_dir() -> PathBuf {
    if let Some(music) = dirs::audio_dir() {
        return music.join(FOLDER_NAME);
    }
    if let Some(home) = dirs::home_dir() {
        return home.join("Music").join(FOLDER_NAME);
    }
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(NAME)
        .join("recordings")
}

/// The folder recordings go to: the user's choice, or the default.
pub fn folder(custom: Option<&Path>) -> PathBuf {
    match custom {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => default_dir(),
    }
}

/// Create `dir` if needed and check that files can be written in it.
pub fn prepare_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let probe = dir.join(format!(".{NAME}-write-test-{}", std::process::id()));
    fs::write(&probe, b"").map_err(|e| format!("Cannot write to {}: {e}", dir.display()))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// A path in `dir` for a new recording of `station` started at `time`,
/// e.g. `Station - 2026-09-26 20-15-03.mp3` for extension `mp3`, with
/// ` (2)`, ` (3)`... added if that name is taken.
pub fn new_file_path(dir: &Path, station: &str, time: DateTime<Local>, ext: &str) -> PathBuf {
    let stem = format!(
        "{} - {}",
        sanitize_name(station),
        time.format("%Y-%m-%d %H-%M-%S")
    );
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}).{ext}")))
        .find(|p| !p.exists())
        .expect("some numbered name is free")
}

/// Make a station name safe to use in a file name on every OS.
///
/// Replaces characters Windows forbids and control characters with `_`,
/// trims spaces and trailing dots, caps the length, and avoids Windows
/// device names. Falls back to "Recording" for an empty result.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().map(|c| match c {
        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
        c if c.is_control() => '_',
        c => c,
    }) {
        if out.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
    }

    let trimmed = out.trim_end_matches(['.', ' ']).trim_start();
    out = trimmed.to_string();

    if out.is_empty() {
        return "Recording".to_string();
    }
    let base = out.split('.').next().unwrap_or("").trim_end();
    if WINDOWS_RESERVED
        .iter()
        .any(|r| r.eq_ignore_ascii_case(base))
    {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 9, 26, 20, 15, 3).unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radiotrope-recnames-{}-{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_dir_ends_in_radiotrope() {
        let dir = default_dir();
        assert!(dir.ends_with(FOLDER_NAME) || dir.ends_with("recordings"));
    }

    #[test]
    fn custom_folder_wins_unless_empty() {
        let custom = PathBuf::from("/media/usb/radio");
        assert_eq!(folder(Some(&custom)), custom);
        assert_eq!(folder(Some(Path::new(""))), default_dir());
        assert_eq!(folder(None), default_dir());
    }

    #[test]
    fn file_name_has_station_and_time() {
        let dir = temp_dir("name");
        let path = new_file_path(&dir, "Test FM", at(), "mp3");
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "Test FM - 2026-09-26 20-15-03.mp3"
        );
    }

    #[test]
    fn taken_names_get_a_number() {
        let dir = temp_dir("taken");
        let first = new_file_path(&dir, "Test FM", at(), "mp3");
        fs::write(&first, b"").unwrap();
        let second = new_file_path(&dir, "Test FM", at(), "mp3");
        assert!(second.ends_with("Test FM - 2026-09-26 20-15-03 (2).mp3"));
        fs::write(&second, b"").unwrap();
        let third = new_file_path(&dir, "Test FM", at(), "mp3");
        assert!(third.ends_with("Test FM - 2026-09-26 20-15-03 (3).mp3"));
        // Another format has its own names
        let opus = new_file_path(&dir, "Test FM", at(), "opus");
        assert!(opus.ends_with("Test FM - 2026-09-26 20-15-03.opus"));
    }

    #[test]
    fn forbidden_characters_are_replaced() {
        assert_eq!(
            sanitize_name(r#"A<B>C:D"E/F\G|H?I*J"#),
            "A_B_C_D_E_F_G_H_I_J"
        );
        assert_eq!(sanitize_name("Tab\there\nnew"), "Tab_here_new");
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        assert_eq!(sanitize_name("  Radio 1 ... "), "Radio 1");
        assert_eq!(sanitize_name("..."), "Recording");
        assert_eq!(sanitize_name(""), "Recording");
    }

    #[test]
    fn windows_device_names_are_avoided() {
        assert_eq!(sanitize_name("CON"), "_CON");
        assert_eq!(sanitize_name("nul"), "_nul");
        assert_eq!(sanitize_name("Com1.fm"), "_Com1.fm");
        assert_eq!(sanitize_name("CONTACT FM"), "CONTACT FM");
    }

    #[test]
    fn long_and_non_ascii_names() {
        let greek = "Ράδιο Αθήνα 98.4";
        assert_eq!(sanitize_name(greek), greek);
        let ascii = "a".repeat(200);
        assert_eq!(sanitize_name(&ascii), "a".repeat(MAX_NAME_BYTES));
        // Two, three and four bytes a character: cut at a character
        // boundary, never past the limit
        for c in ["Ω", "語", "🎵"] {
            let long = format!("x{}", c.repeat(200));
            let name = sanitize_name(&long);
            assert!(name.len() <= MAX_NAME_BYTES);
            assert!(name.len() > MAX_NAME_BYTES - c.len());
            assert!(name.starts_with('x') && name[1..].chars().all(|n| n.to_string() == c));
        }
        // The whole file name fits Linux's 255 bytes
        let dir = temp_dir("long");
        let path = new_file_path(&dir, &"語".repeat(100), at(), "opus");
        assert!(path.file_name().unwrap().len() < 255);
        fs::write(&path, b"").unwrap();
    }

    #[test]
    fn prepare_dir_creates_nested_folders() {
        let dir = temp_dir("prepare").join("a").join("b");
        prepare_dir(&dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "probe file removed");
    }
}
