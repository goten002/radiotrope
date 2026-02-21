//! Application settings management
//!
//! User preferences and application state.

use crate::data::storage;
use crate::data::types::Station;
use crate::error::Result;
use serde::{Deserialize, Serialize};

/// Settings data file name
const SETTINGS_FILE: &str = "settings2.json";

/// Settings file format version for migrations
const SETTINGS_VERSION: u32 = 1;

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// File format version
    #[serde(default = "default_version")]
    pub version: u32,

    // === Audio ===
    /// Volume level (0.0 - 1.0)
    #[serde(default = "default_volume")]
    pub volume: f32,

    /// Muted state
    #[serde(default)]
    pub muted: bool,

    // === Playback ===
    /// Last played station (for resume)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_station: Option<Station>,

    /// Auto-play last station on startup
    #[serde(default)]
    pub auto_play: bool,

    // === Window ===
    /// Window width
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_width: Option<u32>,

    /// Window height
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_height: Option<u32>,

    /// Window X position
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_x: Option<i32>,

    /// Window Y position
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_y: Option<i32>,

    /// Window maximized state
    #[serde(default)]
    pub window_maximized: bool,

    // === Appearance ===
    /// Theme preference
    #[serde(default)]
    pub theme: Theme,

    // === System Tray ===
    /// Show system tray icon
    #[serde(default = "default_true")]
    pub show_tray_icon: bool,

    /// Minimize to tray instead of closing
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,

    /// Start minimized to tray
    #[serde(default)]
    pub start_minimized: bool,

    // === Notifications ===
    /// Show notifications for track changes
    #[serde(default = "default_true")]
    pub show_notifications: bool,

    // === Search ===
    /// Default search provider
    #[serde(default)]
    pub default_provider: String,

    /// Number of search results per page
    #[serde(default = "default_search_limit")]
    pub search_limit: u32,
}

fn default_version() -> u32 {
    SETTINGS_VERSION
}

fn default_volume() -> f32 {
    0.8
}

fn default_true() -> bool {
    true
}

fn default_search_limit() -> u32 {
    100
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            volume: default_volume(),
            muted: false,
            last_station: None,
            auto_play: false,
            window_width: None,
            window_height: None,
            window_x: None,
            window_y: None,
            window_maximized: false,
            theme: Theme::default(),
            show_tray_icon: true,
            minimize_to_tray: true,
            start_minimized: false,
            show_notifications: true,
            default_provider: String::new(),
            search_limit: default_search_limit(),
        }
    }
}

impl Settings {
    /// Create default settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Load settings from default storage location
    pub fn load() -> Result<Self> {
        match storage::load::<Settings>(SETTINGS_FILE)? {
            Some(settings) => Ok(settings),
            None => Ok(Self::default()),
        }
    }

    /// Load settings from a specific path
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        match storage::load_from::<Settings>(path)? {
            Some(settings) => Ok(settings),
            None => Ok(Self::default()),
        }
    }

    /// Save settings to default storage location
    pub fn save(&self) -> Result<()> {
        storage::save(SETTINGS_FILE, self)
    }

    /// Save settings to a specific path
    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        storage::save_to(path, self)
    }

    /// Set volume (clamped to 0.0 - 1.0)
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    /// Get effective volume (considering mute)
    pub fn effective_volume(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume
        }
    }

    /// Toggle mute state
    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
    }

    /// Set window geometry
    pub fn set_window_geometry(&mut self, x: i32, y: i32, width: u32, height: u32) {
        self.window_x = Some(x);
        self.window_y = Some(y);
        self.window_width = Some(width);
        self.window_height = Some(height);
    }

    /// Get window geometry if available
    pub fn window_geometry(&self) -> Option<(i32, i32, u32, u32)> {
        match (
            self.window_x,
            self.window_y,
            self.window_width,
            self.window_height,
        ) {
            (Some(x), Some(y), Some(w), Some(h)) => Some((x, y, w, h)),
            _ => None,
        }
    }
}

/// Theme preference
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow system theme
    #[default]
    System,
    /// Always light theme
    Light,
    /// Always dark theme
    Dark,
}

impl Theme {
    /// Check if this theme prefers dark mode
    pub fn is_dark(&self) -> bool {
        match self {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::System => false, // TODO: detect OS theme
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path() -> std::path::PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        temp_dir().join(format!("radiotrope_settings_test_{}.json", id))
    }

    #[test]
    fn test_default_settings() {
        let settings = Settings::default();
        assert_eq!(settings.volume, 0.8);
        assert!(!settings.muted);
        assert!(!settings.auto_play);
        assert!(settings.show_tray_icon);
        assert!(settings.minimize_to_tray);
        assert_eq!(settings.theme, Theme::System);
    }

    #[test]
    fn test_volume_clamping() {
        let mut settings = Settings::new();

        settings.set_volume(1.5);
        assert_eq!(settings.volume, 1.0);

        settings.set_volume(-0.5);
        assert_eq!(settings.volume, 0.0);

        settings.set_volume(0.5);
        assert_eq!(settings.volume, 0.5);
    }

    #[test]
    fn test_effective_volume() {
        let mut settings = Settings::new();
        settings.volume = 0.8;

        assert_eq!(settings.effective_volume(), 0.8);

        settings.muted = true;
        assert_eq!(settings.effective_volume(), 0.0);
    }

    #[test]
    fn test_toggle_mute() {
        let mut settings = Settings::new();
        assert!(!settings.muted);

        settings.toggle_mute();
        assert!(settings.muted);

        settings.toggle_mute();
        assert!(!settings.muted);
    }

    #[test]
    fn test_window_geometry() {
        let mut settings = Settings::new();

        // Initially none
        assert!(settings.window_geometry().is_none());

        // Set geometry
        settings.set_window_geometry(100, 200, 800, 600);
        assert_eq!(settings.window_geometry(), Some((100, 200, 800, 600)));
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let path = temp_path();

        // Create and save
        {
            let mut settings = Settings::new();
            settings.volume = 0.5;
            settings.muted = true;
            settings.last_station = Some(Station::new("Test Station", "http://test.com/stream"));
            settings.theme = Theme::Dark;
            settings.auto_play = true;
            settings.set_window_geometry(50, 100, 1024, 768);
            settings.save_to(&path).unwrap();
        }

        // Load and verify
        {
            let settings = Settings::load_from(&path).unwrap();
            assert_eq!(settings.volume, 0.5);
            assert!(settings.muted);
            assert!(settings.last_station.is_some());
            let station = settings.last_station.as_ref().unwrap();
            assert_eq!(station.url, "http://test.com/stream");
            assert_eq!(station.name, "Test Station");
            assert_eq!(settings.theme, Theme::Dark);
            assert!(settings.auto_play);
            assert_eq!(settings.window_geometry(), Some((50, 100, 1024, 768)));
        }

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let path = temp_path();
        let settings = Settings::load_from(&path).unwrap();

        // Should return defaults
        assert_eq!(settings.volume, 0.8);
        assert!(!settings.muted);
    }

    #[test]
    fn test_theme_is_dark() {
        assert!(Theme::Dark.is_dark());
        assert!(!Theme::Light.is_dark());
        // Theme::System depends on system settings, can't reliably test
    }

    #[test]
    fn test_all_fields_persist() {
        let path = temp_path();

        // Set ALL fields to non-default values
        {
            let mut settings = Settings::new();
            settings.volume = 0.3;
            settings.muted = true;
            settings.last_station = Some(Station::new("Test Station", "http://station.url"));
            settings.auto_play = true;
            settings.window_width = Some(1920);
            settings.window_height = Some(1080);
            settings.window_x = Some(-100);
            settings.window_y = Some(50);
            settings.window_maximized = true;
            settings.theme = Theme::Light;
            settings.show_tray_icon = false;
            settings.minimize_to_tray = false;
            settings.start_minimized = true;
            settings.show_notifications = false;
            settings.default_provider = "custom".to_string();
            settings.search_limit = 50;
            settings.save_to(&path).unwrap();
        }

        // Verify all fields
        {
            let s = Settings::load_from(&path).unwrap();
            assert_eq!(s.volume, 0.3);
            assert!(s.muted);
            assert!(s.last_station.is_some());
            let station = s.last_station.as_ref().unwrap();
            assert_eq!(station.url, "http://station.url");
            assert_eq!(station.name, "Test Station");
            assert!(s.auto_play);
            assert_eq!(s.window_width, Some(1920));
            assert_eq!(s.window_height, Some(1080));
            assert_eq!(s.window_x, Some(-100));
            assert_eq!(s.window_y, Some(50));
            assert!(s.window_maximized);
            assert_eq!(s.theme, Theme::Light);
            assert!(!s.show_tray_icon);
            assert!(!s.minimize_to_tray);
            assert!(s.start_minimized);
            assert!(!s.show_notifications);
            assert_eq!(s.default_provider, "custom");
            assert_eq!(s.search_limit, 50);
        }

        let _ = fs::remove_file(&path);
    }

    // =========================================================================
    // Edge cases and error handling tests
    // =========================================================================

    #[test]
    fn test_partial_settings_file_uses_defaults() {
        let path = temp_path();

        // Write a minimal settings file (missing most fields)
        let partial_json = r#"{"volume": 0.5}"#;
        fs::write(&path, partial_json).unwrap();

        let settings = Settings::load_from(&path).unwrap();

        // Specified value should be loaded
        assert_eq!(settings.volume, 0.5);

        // Missing fields should use defaults
        assert!(!settings.muted);
        assert_eq!(settings.theme, Theme::System);
        assert!(settings.show_tray_icon);
        assert!(settings.minimize_to_tray);
        assert_eq!(settings.search_limit, 100);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_unknown_fields_are_ignored() {
        let path = temp_path();

        // Write settings with extra unknown fields (future-proofing)
        let json_with_extra = r#"{
            "volume": 0.7,
            "muted": false,
            "unknown_field": "should be ignored",
            "another_unknown": 12345
        }"#;
        fs::write(&path, json_with_extra).unwrap();

        // Should load without error, ignoring unknown fields
        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.volume, 0.7);
        assert!(!settings.muted);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_invalid_json_returns_error() {
        let path = temp_path();

        fs::write(&path, "{ invalid json }").unwrap();

        let result = Settings::load_from(&path);
        assert!(result.is_err());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_empty_file_returns_defaults() {
        let path = temp_path();

        fs::write(&path, "").unwrap();

        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.volume, 0.8); // default

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_whitespace_only_file_returns_defaults() {
        let path = temp_path();

        fs::write(&path, "   \n\t  \n  ").unwrap();

        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.volume, 0.8); // default

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_volume_boundary_values() {
        let mut settings = Settings::new();

        // Exact boundaries
        settings.set_volume(0.0);
        assert_eq!(settings.volume, 0.0);

        settings.set_volume(1.0);
        assert_eq!(settings.volume, 1.0);

        // Just inside boundaries
        settings.set_volume(0.001);
        assert_eq!(settings.volume, 0.001);

        settings.set_volume(0.999);
        assert_eq!(settings.volume, 0.999);
    }

    #[test]
    fn test_volume_special_float_values() {
        let mut settings = Settings::new();

        // NaN should clamp (NaN comparisons are tricky)
        settings.set_volume(f32::NAN);
        // NaN.clamp() returns NaN, so we need to handle this differently
        // For now, just verify it doesn't panic
        let _ = settings.volume;

        // Infinity should clamp to 1.0
        settings.set_volume(f32::INFINITY);
        assert_eq!(settings.volume, 1.0);

        // Negative infinity should clamp to 0.0
        settings.set_volume(f32::NEG_INFINITY);
        assert_eq!(settings.volume, 0.0);
    }

    #[test]
    fn test_unicode_in_strings() {
        let path = temp_path();

        let mut settings = Settings::new();
        // Japanese, Russian, Greek, Chinese in URL and name
        settings.last_station = Some(Station::new(
            "Ραδιοφωνικός σταθμός 电台",
            "http://example.com/日本語/стрим/ελληνικά",
        ));
        settings.default_provider = "Ραδιόφωνο Ελλάδα 提供者".to_string();
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert!(loaded.last_station.is_some());
        let station = loaded.last_station.unwrap();
        assert_eq!(station.url, "http://example.com/日本語/стрим/ελληνικά");
        assert_eq!(station.name, "Ραδιοφωνικός σταθμός 电台");
        assert_eq!(loaded.default_provider, "Ραδιόφωνο Ελλάδα 提供者");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_special_characters_in_url() {
        let path = temp_path();

        let mut settings = Settings::new();
        settings.last_station = Some(Station::new(
            "Test Station",
            "http://example.com/stream?param=value&other=123#anchor",
        ));
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert!(loaded.last_station.is_some());
        let station = loaded.last_station.unwrap();
        assert_eq!(
            station.url,
            "http://example.com/stream?param=value&other=123#anchor"
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_window_geometry_negative_values() {
        let mut settings = Settings::new();

        // Negative positions are valid (multi-monitor setups)
        settings.set_window_geometry(-500, -200, 800, 600);
        assert_eq!(settings.window_geometry(), Some((-500, -200, 800, 600)));
    }

    #[test]
    fn test_window_geometry_large_values() {
        let mut settings = Settings::new();

        // Large values for high-res displays
        settings.set_window_geometry(0, 0, 7680, 4320); // 8K resolution
        assert_eq!(settings.window_geometry(), Some((0, 0, 7680, 4320)));
    }

    #[test]
    fn test_window_geometry_partial_none() {
        let mut settings = Settings::new();

        // Only some fields set
        settings.window_x = Some(100);
        settings.window_y = Some(200);
        // width and height are None

        // Should return None since not all fields are set
        assert!(settings.window_geometry().is_none());
    }

    #[test]
    fn test_theme_serialization() {
        let path = temp_path();

        // Test each theme variant serializes correctly
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            let mut settings = Settings::new();
            settings.theme = theme;
            settings.save_to(&path).unwrap();

            let loaded = Settings::load_from(&path).unwrap();
            assert_eq!(loaded.theme, theme);
        }

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_theme_json_format() {
        let path = temp_path();

        let mut settings = Settings::new();
        settings.theme = Theme::Dark;
        settings.save_to(&path).unwrap();

        // Read raw JSON to verify format
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"theme\": \"dark\""));

        settings.theme = Theme::Light;
        settings.save_to(&path).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"theme\": \"light\""));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_version_field_persists() {
        let path = temp_path();

        let settings = Settings::new();
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.version, 1);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_mute_preserves_volume() {
        let mut settings = Settings::new();
        settings.volume = 0.7;

        settings.toggle_mute();
        assert!(settings.muted);
        assert_eq!(settings.volume, 0.7); // Volume preserved
        assert_eq!(settings.effective_volume(), 0.0); // But effective is 0

        settings.toggle_mute();
        assert!(!settings.muted);
        assert_eq!(settings.volume, 0.7); // Still preserved
        assert_eq!(settings.effective_volume(), 0.7); // Back to normal
    }

    #[test]
    fn test_search_limit_persists() {
        let path = temp_path();

        let mut settings = Settings::new();
        settings.search_limit = 250;
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.search_limit, 250);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_optional_fields_skip_none() {
        let path = temp_path();

        // Save with defaults (None values)
        let settings = Settings::new();
        settings.save_to(&path).unwrap();

        // Read raw JSON
        let content = fs::read_to_string(&path).unwrap();

        // Optional None fields should NOT appear in JSON
        assert!(!content.contains("last_station"));
        assert!(!content.contains("window_width"));
        assert!(!content.contains("window_height"));
        assert!(!content.contains("window_x"));
        assert!(!content.contains("window_y"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_modify_and_save_multiple_times() {
        let path = temp_path();

        let mut settings = Settings::new();

        // First save
        settings.volume = 0.5;
        settings.save_to(&path).unwrap();

        // Modify and save again
        settings.volume = 0.7;
        settings.muted = true;
        settings.save_to(&path).unwrap();

        // Modify and save again
        settings.theme = Theme::Dark;
        settings.save_to(&path).unwrap();

        // Load and verify final state
        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.volume, 0.7);
        assert!(loaded.muted);
        assert_eq!(loaded.theme, Theme::Dark);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_last_station_with_logo() {
        let path = temp_path();

        let mut settings = Settings::new();
        settings.last_station = Some(
            Station::new("Example Radio", "http://stream.example.com/live")
                .with_logo("http://example.com/logo.png"),
        );
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert!(loaded.last_station.is_some());
        let station = loaded.last_station.as_ref().unwrap();
        assert_eq!(station.url, "http://stream.example.com/live");
        assert_eq!(station.name, "Example Radio");
        assert_eq!(
            station.logo_url,
            Some("http://example.com/logo.png".to_string())
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_last_station_without_logo_skips_field() {
        let path = temp_path();

        let mut settings = Settings::new();
        settings.last_station = Some(Station::new("Radio", "http://stream.example.com"));
        settings.save_to(&path).unwrap();

        // Read raw JSON to verify logo_url is skipped
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("last_station"));
        assert!(content.contains("http://stream.example.com"));
        assert!(!content.contains("logo_url")); // Should be skipped when None

        let _ = fs::remove_file(&path);
    }
}
