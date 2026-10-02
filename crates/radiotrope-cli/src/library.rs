//! The app's favorites and settings, read only
//!
//! The terminal player never writes the app's files: it reads the fields
//! it needs from `favorites.json` and `settings.json` and leaves the rest.
//! Each field is read on its own, so a field the app adds, drops or
//! changes costs only that field.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// The app's folder name, under the system's config folder
const APP_DIR: &str = "radiotrope";
const FAVORITES_FILE: &str = "favorites.json";
const SETTINGS_FILE: &str = "settings.json";

/// The app's config folder: `~/.config/radiotrope` on Linux,
/// `%APPDATA%\radiotrope` on Windows
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join(APP_DIR))
}

pub fn favorites_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(FAVORITES_FILE))
}

pub fn settings_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(SETTINGS_FILE))
}

/// A station to play: a favorite, or a URL given on the command line
#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    pub name: String,
    pub url: String,
    pub country: Option<String>,
}

/// The favorites in the app's order. A missing or unreadable file is no
/// favorites; an entry without a name or URL is skipped.
pub fn read_favorites(path: &Path) -> Vec<Station> {
    let Some(file) = read_json(path) else {
        return Vec::new();
    };
    let Some(entries) = file.get("favorites").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut favorites: Vec<(i64, Station)> = entries
        .iter()
        .filter_map(|entry| {
            let url = text(entry, "url")?;
            let name = text(entry, "name").unwrap_or_else(|| url.clone());
            let order = entry.get("sort_order").and_then(Value::as_i64).unwrap_or(0);
            Some((
                order,
                Station {
                    name,
                    url,
                    country: text(entry, "country"),
                },
            ))
        })
        .collect();
    // The app's own order (sort_order), with the name settling ties
    favorites.sort_by(|(a, x), (b, y)| {
        a.cmp(b)
            .then_with(|| x.name.to_lowercase().cmp(&y.name.to_lowercase()))
    });
    favorites.into_iter().map(|(_, station)| station).collect()
}

/// Recording file format, as the app names it in its settings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordFormat {
    #[default]
    Mp3,
    Opus,
    Wav,
}

/// The app's settings the terminal player uses
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// 0.0 to 1.0
    pub volume: f32,
    pub muted: bool,
    pub accent: Option<(u8, u8, u8)>,
    pub eq_enabled: bool,
    pub eq_gains: [f32; 10],
    pub eq_preamp: f32,
    pub eq_preamp_moved: bool,
    pub eq_preset_name: Option<String>,
    /// `None` is the default folder
    pub recording_dir: Option<PathBuf>,
    pub recording_format: RecordFormat,
    /// `None` is Auto: the station's bitrate
    pub recording_bitrate: Option<u32>,
    pub record_with_eq: bool,
}

impl Default for Settings {
    /// The app's defaults, for a first run with no settings file
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
            accent: None,
            eq_enabled: false,
            eq_gains: [0.0; 10],
            eq_preamp: 0.0,
            eq_preamp_moved: false,
            eq_preset_name: Some("Flat".to_string()),
            recording_dir: None,
            recording_format: RecordFormat::Mp3,
            recording_bitrate: None,
            record_with_eq: false,
        }
    }
}

/// The settings saved by the app, with its defaults for anything missing
pub fn read_settings(path: &Path) -> Settings {
    let mut settings = Settings::default();
    let Some(file) = read_json(path) else {
        return settings;
    };
    if let Some(volume) = file.get("volume").and_then(Value::as_f64) {
        let volume = volume as f32;
        if volume.is_finite() {
            settings.volume = volume.clamp(0.0, 1.0);
        }
    }
    if let Some(muted) = flag(&file, "muted") {
        settings.muted = muted;
    }
    settings.accent = text(&file, "accent_color").and_then(|hex| parse_hex_rgb(&hex));
    if let Some(on) = flag(&file, "eq_enabled") {
        settings.eq_enabled = on;
    }
    if let Some(gains) = file.get("eq_gains").and_then(Value::as_array) {
        for (gain, value) in settings.eq_gains.iter_mut().zip(gains) {
            if let Some(db) = value.as_f64().map(|v| v as f32).filter(|v| v.is_finite()) {
                *gain = db;
            }
        }
    }
    if let Some(db) = file
        .get("eq_preamp")
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .filter(|v| v.is_finite())
    {
        settings.eq_preamp = db;
    }
    if let Some(moved) = flag(&file, "eq_preamp_moved") {
        settings.eq_preamp_moved = moved;
    }
    // Left out of the file when the gains are a custom curve
    settings.eq_preset_name = text(&file, "eq_preset_name");
    settings.recording_dir = text(&file, "recording_dir").map(PathBuf::from);
    settings.recording_format = match text(&file, "recording_format").as_deref() {
        Some("opus") => RecordFormat::Opus,
        Some("wav") => RecordFormat::Wav,
        _ => RecordFormat::Mp3,
    };
    settings.recording_bitrate = file
        .get("recording_bitrate")
        .and_then(Value::as_u64)
        .and_then(|kbps| u32::try_from(kbps).ok());
    if let Some(with_eq) = flag(&file, "record_with_eq") {
        settings.record_with_eq = with_eq;
    }
    settings
}

/// When `path` last changed, to notice the app saving it
pub fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// A non-empty text field, trimmed
fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn flag(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

/// `#rrggbb` or `rrggbb` to RGB, as the app reads its accent colour
fn parse_hex_rgb(text: &str) -> Option<(u8, u8, u8)> {
    let text = text.trim();
    let hex = text.strip_prefix('#').unwrap_or(text);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    Some(((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
}

/// Find the favorite `query` names: its number in the list (from 1), or
/// the first whose name contains it, ignoring case
pub fn find_favorite<'a>(favorites: &'a [Station], query: &str) -> Option<&'a Station> {
    let query = query.trim();
    if let Ok(number) = query.parse::<usize>() {
        return number.checked_sub(1).and_then(|i| favorites.get(i));
    }
    let query = query.to_lowercase();
    favorites
        .iter()
        .find(|f| f.name.to_lowercase().contains(&query))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(name: &str) -> Station {
        Station {
            name: name.to_string(),
            url: format!("http://{name}.test/"),
            country: None,
        }
    }

    #[test]
    fn a_favorite_is_found_by_number_or_name() {
        let favorites = vec![station("Best 92.6"), station("Capital FM London")];
        assert_eq!(find_favorite(&favorites, "2"), Some(&favorites[1]));
        assert_eq!(find_favorite(&favorites, "capital"), Some(&favorites[1]));
        assert_eq!(find_favorite(&favorites, "BEST"), Some(&favorites[0]));
        assert_eq!(find_favorite(&favorites, "0"), None);
        assert_eq!(find_favorite(&favorites, "3"), None);
        assert_eq!(find_favorite(&favorites, "jazz"), None);
    }

    #[test]
    fn missing_files_give_the_defaults() {
        let nowhere = Path::new("/nonexistent/radiotrope-cli-test.json");
        assert!(read_favorites(nowhere).is_empty());
        assert_eq!(read_settings(nowhere), Settings::default());
    }

    #[test]
    fn odd_fields_cost_only_themselves() {
        let dir = std::env::temp_dir().join(format!("radiotrope-cli-odd-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        fs::write(
            &path,
            r##"{"volume": "loud", "muted": true, "accent_color": "#3584e4",
                "eq_gains": [1, "x", 3], "recording_format": "flac", "new_field": {}}"##,
        )
        .unwrap();
        let settings = read_settings(&path);
        assert_eq!(settings.volume, 1.0);
        assert!(settings.muted);
        assert_eq!(settings.accent, Some((0x35, 0x84, 0xe4)));
        assert_eq!(&settings.eq_gains[..3], &[1.0, 0.0, 3.0]);
        assert_eq!(settings.recording_format, RecordFormat::Mp3);

        let path = dir.join("favorites.json");
        fs::write(
            &path,
            r#"{"version": 1, "favorites": [
                {"name": "B", "url": "http://b.test/", "sort_order": 2},
                {"name": "No URL"},
                {"url": "http://a.test/", "sort_order": 1, "country": "Greece"}]}"#,
        )
        .unwrap();
        let favorites = read_favorites(&path);
        assert_eq!(favorites.len(), 2);
        assert_eq!(favorites[0].name, "http://a.test/");
        assert_eq!(favorites[0].country.as_deref(), Some("Greece"));
        assert_eq!(favorites[1].name, "B");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The app's own code writes the files, so a change to their format
    /// fails here rather than in the terminal
    mod app_files {
        use super::super::*;
        use radiotrope_app::data::favorites::FavoritesManager;
        use radiotrope_app::data::settings::{RecordingFormat, Settings as AppSettings};
        use radiotrope_app::data::types::{Favorite, FavoriteSort};

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir()
                .join(format!("radiotrope-cli-app-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn favorites_saved_by_the_app_read_back_in_its_order() {
            let dir = scratch("favorites");
            let path = dir.join(FAVORITES_FILE);
            let mut app = FavoritesManager::new();
            for (name, country) in [
                ("Best 92.6", Some("Greece")),
                ("BBC Radio 1", Some("United Kingdom")),
                ("Ραδιόφωνο", None),
            ] {
                let mut favorite = Favorite::new(name, format!("http://{}.test/", name.len()));
                favorite.station.country = country.map(str::to_string);
                app.add(favorite).unwrap();
            }
            // Listening stats in the file are no business of the reader
            app.record_play_by_url("http://9.test/", 600).unwrap();
            let last = app.sorted(FavoriteSort::Manual)[2].id();
            app.move_to_edge(&last, true).unwrap();
            app.save_to(&path).unwrap();

            let ours = read_favorites(&path);
            let theirs = app.sorted(FavoriteSort::Manual);
            assert_eq!(ours.len(), theirs.len());
            for (ours, theirs) in ours.iter().zip(theirs) {
                assert_eq!(ours.name, theirs.name());
                assert_eq!(ours.url, theirs.url());
                assert_eq!(ours.country, theirs.station.country);
            }
            assert_eq!(ours[0].name, "Ραδιόφωνο");
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn settings_saved_by_the_app_read_back() {
            let dir = scratch("settings");
            let path = dir.join(SETTINGS_FILE);
            let mut app = AppSettings::new();
            app.volume = 0.35;
            app.muted = true;
            app.accent_color = Some("#3584e4".to_string());
            app.eq_enabled = true;
            app.eq_gains = [1.0, 2.0, 3.0, 0.0, -1.0, -2.0, 0.5, 0.0, 4.0, 6.0];
            app.eq_preamp = -3.5;
            app.eq_preamp_moved = true;
            app.eq_preset_name = None;
            app.recording_dir = Some(dir.join("Recordings"));
            app.recording_format = RecordingFormat::Opus;
            app.recording_bitrate = Some(192);
            app.record_with_eq = true;
            app.save_to(&path).unwrap();

            let ours = read_settings(&path);
            assert_eq!(ours.volume, 0.35);
            assert!(ours.muted);
            assert_eq!(ours.accent, Some((0x35, 0x84, 0xe4)));
            assert!(ours.eq_enabled);
            assert_eq!(ours.eq_gains, app.eq_gains);
            assert_eq!(ours.eq_preamp, -3.5);
            assert!(ours.eq_preamp_moved);
            assert_eq!(ours.eq_preset_name, None);
            assert_eq!(ours.recording_dir, app.recording_dir);
            assert_eq!(ours.recording_format, RecordFormat::Opus);
            assert_eq!(ours.recording_bitrate, Some(192));
            assert!(ours.record_with_eq);

            // Each format and a preset, as the app saves them
            for (format, expected) in [
                (RecordingFormat::Mp3, RecordFormat::Mp3),
                (RecordingFormat::Wav, RecordFormat::Wav),
            ] {
                app.recording_format = format;
                app.eq_preset_name = Some("Rock".to_string());
                app.recording_bitrate = None;
                app.save_to(&path).unwrap();
                let ours = read_settings(&path);
                assert_eq!(ours.recording_format, expected);
                assert_eq!(ours.eq_preset_name.as_deref(), Some("Rock"));
                assert_eq!(ours.recording_bitrate, None);
            }

            // A first run: the app's defaults
            let defaults = AppSettings::default();
            let ours = Settings::default();
            assert_eq!(ours.volume, defaults.volume);
            assert_eq!(ours.muted, defaults.muted);
            assert_eq!(ours.eq_enabled, defaults.eq_enabled);
            assert_eq!(ours.eq_preset_name, defaults.eq_preset_name);
            assert_eq!(ours.recording_dir, defaults.recording_dir);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn the_config_folder_is_the_apps() {
            assert_eq!(
                config_dir(),
                radiotrope_app::data::storage::config_dir().ok()
            );
        }
    }
}
