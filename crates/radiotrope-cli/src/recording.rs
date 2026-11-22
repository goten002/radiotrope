//! Recording folder and file names
//!
//! The same folder and names as the app's recordings (radiotrope-app's
//! `data/recordings.rs`), so recordings made here sit beside the app's and
//! look the same. A test checks the two still agree.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use radiotrope_library::text::io_reason;

/// The app's name, for its data folder and the write check's file
const APP_NAME: &str = "radiotrope";

/// Bitrate used when Auto can't find the station's bitrate, in kbps
const AUTO_FALLBACK_KBPS: u32 = 256;

/// The bitrate to record at: the chosen one, or for Auto the station's own
/// (256 kbps when unknown), kept within 32-320 kbps, as the app does
pub fn recording_kbps(chosen: Option<u32>, station: Option<u32>) -> u32 {
    chosen
        .or(station.filter(|k| *k > 0))
        .unwrap_or(AUTO_FALLBACK_KBPS)
        .clamp(32, 320)
}

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
        .join(APP_NAME)
        .join("recordings")
}

/// The folder recordings go to: the user's choice, or the default.
pub fn folder(custom: Option<&Path>) -> PathBuf {
    match custom {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => default_dir(),
    }
}

/// Create `dir` if needed and check that files can be written in it. The
/// error is the app's sentence, with the folder added (the app shows the
/// folder beside it; here there is only the line).
pub fn prepare_dir(dir: &Path) -> Result<(), String> {
    let shown = display_path(dir);
    fs::create_dir_all(dir).map_err(|e| {
        format!(
            "Can't create the recording folder: {} ({shown})",
            io_reason(&e)
        )
    })?;
    let probe = dir.join(format!(".{APP_NAME}-write-test-{}", std::process::id()));
    fs::write(&probe, b"").map_err(|e| {
        format!(
            "Can't save files in the recording folder: {} ({shown})",
            io_reason(&e)
        )
    })?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// `path` for the screen, with the home folder shortened to `~`
pub fn display_path(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
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

    #[test]
    fn auto_bitrate_follows_the_station() {
        assert_eq!(recording_kbps(None, Some(128)), 128);
        assert_eq!(recording_kbps(None, None), 256);
        assert_eq!(recording_kbps(None, Some(0)), 256);
        assert_eq!(recording_kbps(Some(192), Some(64)), 192);
        assert_eq!(recording_kbps(Some(999), None), 320);
    }

    /// Recordings made here get the app's folder and names
    #[test]
    fn names_and_folder_match_the_apps() {
        use chrono::TimeZone;
        use radiotrope_app::data::recordings as app;

        assert_eq!(default_dir(), app::default_dir());
        for custom in [None, Some(Path::new("")), Some(Path::new("/tmp/rec"))] {
            assert_eq!(folder(custom), app::folder(custom));
        }
        let names = [
            "Capital FM London",
            "AC/DC: Live? <Best> \"Hits\" | 24*7",
            "CON",
            "aux.fm",
            "  dots and spaces . . ",
            "",
            "ραδιόφωνο 日本語ラジオ ",
            &"Ω".repeat(100),
            "tab\there\u{7}",
        ];
        for name in names {
            assert_eq!(sanitize_name(name), app::sanitize_name(name), "{name:?}");
        }
        let dir = std::env::temp_dir().join(format!("radiotrope-cli-names-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let at = Local.with_ymd_and_hms(2026, 10, 2, 22, 15, 3).unwrap();
        let first = new_file_path(&dir, "Capital FM", at, "mp3");
        assert_eq!(first, app::new_file_path(&dir, "Capital FM", at, "mp3"));
        fs::write(&first, b"").unwrap();
        assert_eq!(
            new_file_path(&dir, "Capital FM", at, "mp3"),
            app::new_file_path(&dir, "Capital FM", at, "mp3")
        );
        assert!(prepare_dir(&dir).is_ok());

        // A folder that can't be made: the app's words, and the folder
        let blocked = first.join("sub");
        let ours = prepare_dir(&blocked).unwrap_err();
        let apps = app::prepare_dir(&blocked).unwrap_err();
        assert_eq!(ours, format!("{apps} ({})", display_path(&blocked)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_under_home_start_with_a_tilde() {
        let home = dirs::home_dir().unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            display_path(&home.join("Music").join("Radiotrope")),
            format!("~{sep}Music{sep}Radiotrope")
        );
        assert_eq!(display_path(&home), "~");
        let elsewhere = Path::new("/srv/rec");
        if !elsewhere.starts_with(&home) {
            assert_eq!(display_path(elsewhere), "/srv/rec");
        }
    }
}
