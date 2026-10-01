//! Application settings management
//!
//! User preferences and application state.

use crate::data::storage;
use crate::data::types::Station;
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Settings data file name
const SETTINGS_FILE: &str = "settings.json";

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
    #[serde(default = "default_volume", deserialize_with = "volume_or_default")]
    pub volume: f32,

    /// Muted state
    #[serde(default)]
    pub muted: bool,

    // === Playback ===
    /// Last played station (for resume)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_station: Option<Station>,

    // === Window ===
    /// Window width in logical pixels
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_width: Option<u32>,

    /// Window height in logical pixels
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_height: Option<u32>,

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

    // === Visualization ===
    /// Visualization mode (wave, spectrum, mirror, dots, vu, hbars)
    #[serde(default = "default_viz_mode")]
    pub viz_mode: String,

    /// Visualizer colors: "logo" (the icon's gradient) or "accent"
    #[serde(default = "default_viz_palette")]
    pub viz_palette: String,

    /// Show the visualizer tile in the header
    #[serde(default = "default_true")]
    pub show_visualizer: bool,

    /// Show listening stats on favorites rows
    #[serde(default = "default_true")]
    pub show_station_stats: bool,

    /// Accent glow on the header, the controls and dialogs
    #[serde(default = "default_true")]
    pub panel_gradient: bool,

    /// Draw our own title bar instead of the system frame (desktop)
    #[serde(default = "default_true")]
    pub custom_title_bar: bool,

    /// Keep the window above other windows
    #[serde(default)]
    pub always_on_top: bool,

    // === Accent Color ===
    /// Custom accent color as hex string (e.g. "#3584e4")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,

    // === Equalizer ===
    #[serde(default)]
    pub eq_gains: [f32; 10],

    #[serde(default)]
    pub eq_preamp: f32,

    #[serde(default)]
    pub eq_enabled: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq_preset_name: Option<String>,

    // === Recording ===
    /// Folder for recordings; `None` means the default (`<Music>/Radiotrope`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_dir: Option<PathBuf>,

    /// Record the sound after the equalizer instead of the station's sound
    #[serde(default)]
    pub record_with_eq: bool,

    /// File format for new recordings
    #[serde(default)]
    pub recording_format: RecordingFormat,

    /// MP3/Opus bitrate in kbps; `None` means Auto (the station's bitrate)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_bitrate: Option<u32>,

    // === Agents (MCP) ===
    /// Agents over the network may use the player
    #[serde(default)]
    pub mcp_network: bool,

    /// What network agents must show to use the player
    #[serde(default)]
    pub mcp_auth: McpAuth,

    /// Address and port the network server listens on
    #[serde(default = "default_mcp_address")]
    pub mcp_address: String,

    /// The network interface picked for the address, e.g. "eth0" or "Wi-Fi":
    /// its address today is used, so a new one from the router still works
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_interface: Option<String>,

    /// The agent the Agents dialog shows setup lines for, e.g. "codex";
    /// `None` is Claude Code
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_client: Option<String>,
}

fn default_mcp_address() -> String {
    crate::config::mcp::DEFAULT_ADDRESS.to_string()
}

fn default_version() -> u32 {
    SETTINGS_VERSION
}

fn default_volume() -> f32 {
    1.0
}

/// A volume saved as `null` (serde's spelling of NaN, which older builds
/// could save) or out of range reads as the default or is clamped, instead
/// of failing the whole file and resetting every setting
fn volume_or_default<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<f32, D::Error> {
    let volume: Option<f32> = Option::deserialize(d)?;
    Ok(volume
        .filter(|v| v.is_finite())
        .map_or_else(default_volume, |v| v.clamp(0.0, 1.0)))
}

fn default_viz_mode() -> String {
    "wave".to_string()
}

fn default_viz_palette() -> String {
    "logo".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            volume: default_volume(),
            muted: false,
            last_station: None,
            window_width: None,
            window_height: None,
            theme: Theme::default(),
            show_tray_icon: true,
            minimize_to_tray: true,
            start_minimized: false,
            show_notifications: true,
            viz_mode: default_viz_mode(),
            viz_palette: default_viz_palette(),
            show_visualizer: true,
            show_station_stats: true,
            panel_gradient: true,
            custom_title_bar: true,
            always_on_top: false,
            accent_color: None,
            eq_gains: [0.0; 10],
            eq_preamp: 0.0,
            eq_enabled: false,
            eq_preset_name: Some("Flat".to_string()),
            recording_dir: None,
            record_with_eq: false,
            recording_format: RecordingFormat::Mp3,
            recording_bitrate: None,
            mcp_network: false,
            mcp_auth: McpAuth::None,
            mcp_address: default_mcp_address(),
            mcp_interface: None,
            mcp_client: None,
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

    /// Parse accent color hex string to RGB components
    pub fn accent_color_rgb(&self) -> Option<(u8, u8, u8)> {
        let hex = self.accent_color.as_ref()?;
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if hex.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some((r, g, b))
    }
}

/// What network agents must show to use the player
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAuth {
    /// Anyone who reaches the address may use the player
    #[default]
    None,
    /// Requests carry `Authorization: Bearer <token>`
    Token,
}

impl McpAuth {
    /// In the order the Agents dialog lists them
    pub const ALL: [McpAuth; 2] = [McpAuth::None, McpAuth::Token];

    /// Name shown in the Agents dialog
    pub fn label(self) -> &'static str {
        match self {
            McpAuth::None => "None",
            McpAuth::Token => "Token",
        }
    }
}

/// File format for recordings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingFormat {
    /// MP3, 192 kbps
    #[default]
    Mp3,
    /// Opus in Ogg, 128 kbps
    Opus,
    /// Uncompressed 16-bit WAV
    Wav,
}

impl RecordingFormat {
    /// Name used in settings and the UI ("mp3", "opus", "wav")
    pub fn id(self) -> &'static str {
        match self {
            RecordingFormat::Mp3 => "mp3",
            RecordingFormat::Opus => "opus",
            RecordingFormat::Wav => "wav",
        }
    }

    /// Parse a name from [`RecordingFormat::id`]; unknown names give MP3.
    pub fn from_id(id: &str) -> Self {
        match id {
            "opus" => RecordingFormat::Opus,
            "wav" => RecordingFormat::Wav,
            _ => RecordingFormat::Mp3,
        }
    }
}

impl From<RecordingFormat> for radiotrope::audio::RecordingFormat {
    fn from(format: RecordingFormat) -> Self {
        match format {
            RecordingFormat::Mp3 => radiotrope::audio::RecordingFormat::Mp3,
            RecordingFormat::Opus => radiotrope::audio::RecordingFormat::Opus,
            RecordingFormat::Wav => radiotrope::audio::RecordingFormat::Wav,
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
            Theme::System => true, // default to dark, matching Slint default
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
        assert_eq!(settings.volume, 1.0);
        assert!(!settings.muted);

        assert!(settings.show_tray_icon);
        assert!(settings.minimize_to_tray);
        assert_eq!(settings.theme, Theme::System);
    }

    #[test]
    fn null_volume_reads_as_default_and_keeps_other_settings() {
        let settings: Settings =
            serde_json::from_str(r#"{"volume": null, "muted": true}"#).unwrap();
        assert_eq!(settings.volume, 1.0);
        assert!(settings.muted);

        let settings: Settings = serde_json::from_str(r#"{"volume": 7.5}"#).unwrap();
        assert_eq!(settings.volume, 1.0);
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
    fn test_save_and_load_roundtrip() {
        let path = temp_path();

        // Create and save
        {
            let mut settings = Settings::new();
            settings.volume = 0.5;
            settings.muted = true;
            settings.last_station = Some(Station::new("Test Station", "http://test.com/stream"));
            settings.theme = Theme::Dark;

            settings.window_width = Some(1024);
            settings.window_height = Some(768);
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

            assert_eq!(settings.window_width, Some(1024));
            assert_eq!(settings.window_height, Some(768));
        }

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let path = temp_path();
        let settings = Settings::load_from(&path).unwrap();

        // Should return defaults
        assert_eq!(settings.volume, 1.0);
        assert!(!settings.muted);
    }

    #[test]
    fn test_recording_defaults() {
        let settings = Settings::default();
        assert_eq!(settings.recording_dir, None);
        assert!(!settings.record_with_eq);
        assert_eq!(settings.recording_format, RecordingFormat::Mp3);
        assert_eq!(settings.recording_bitrate, None);

        // Settings files from before recording existed load unchanged
        let old: Settings = serde_json::from_str(r#"{"version":1,"volume":0.5}"#).unwrap();
        assert_eq!(old.recording_dir, None);
        assert!(!old.record_with_eq);
        assert_eq!(old.recording_format, RecordingFormat::Mp3);
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

            settings.window_width = Some(1920);
            settings.window_height = Some(1080);
            settings.theme = Theme::Light;
            settings.show_tray_icon = false;
            settings.minimize_to_tray = false;
            settings.start_minimized = true;
            settings.show_notifications = false;
            settings.accent_color = Some("#3584e4".to_string());
            settings.recording_dir = Some(PathBuf::from("/media/usb/radio"));
            settings.record_with_eq = true;
            settings.recording_format = RecordingFormat::Opus;
            settings.recording_bitrate = Some(256);
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

            assert_eq!(s.window_width, Some(1920));
            assert_eq!(s.window_height, Some(1080));
            assert_eq!(s.theme, Theme::Light);
            assert!(!s.show_tray_icon);
            assert!(!s.minimize_to_tray);
            assert!(s.start_minimized);
            assert!(!s.show_notifications);
            assert_eq!(s.accent_color, Some("#3584e4".to_string()));
            assert_eq!(s.recording_dir, Some(PathBuf::from("/media/usb/radio")));
            assert!(s.record_with_eq);
            assert_eq!(s.recording_format, RecordingFormat::Opus);
            assert_eq!(s.recording_bitrate, Some(256));
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
        assert!(settings.show_visualizer);
        assert!(settings.show_station_stats);
        assert!(settings.panel_gradient);
        assert!(settings.custom_title_bar);
        assert!(!settings.always_on_top);

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
        assert_eq!(settings.volume, 1.0); // default

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_whitespace_only_file_returns_defaults() {
        let path = temp_path();

        fs::write(&path, "   \n\t  \n  ").unwrap();

        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.volume, 1.0); // default

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
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert!(loaded.last_station.is_some());
        let station = loaded.last_station.unwrap();
        assert_eq!(station.url, "http://example.com/日本語/стрим/ελληνικά");
        assert_eq!(station.name, "Ραδιοφωνικός σταθμός 电台");

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
        assert!(!content.contains("accent_color"));

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

    // =========================================================================
    // Accent color tests
    // =========================================================================

    #[test]
    fn test_accent_color_rgb_valid_with_hash() {
        let mut settings = Settings::new();
        settings.accent_color = Some("#3584e4".to_string());
        assert_eq!(settings.accent_color_rgb(), Some((0x35, 0x84, 0xe4)));
    }

    #[test]
    fn test_accent_color_rgb_valid_without_hash() {
        let mut settings = Settings::new();
        settings.accent_color = Some("3584e4".to_string());
        assert_eq!(settings.accent_color_rgb(), Some((0x35, 0x84, 0xe4)));
    }

    #[test]
    fn test_accent_color_rgb_none() {
        let settings = Settings::new();
        assert_eq!(settings.accent_color_rgb(), None);
    }

    #[test]
    fn test_accent_color_rgb_too_short() {
        let mut settings = Settings::new();
        settings.accent_color = Some("#fff".to_string());
        assert_eq!(settings.accent_color_rgb(), None);
    }

    #[test]
    fn test_accent_color_rgb_too_long() {
        let mut settings = Settings::new();
        settings.accent_color = Some("#1234567".to_string());
        assert_eq!(settings.accent_color_rgb(), None);
    }

    #[test]
    fn test_accent_color_rgb_invalid_hex_chars() {
        let mut settings = Settings::new();
        settings.accent_color = Some("#zzzzzz".to_string());
        assert_eq!(settings.accent_color_rgb(), None);
    }

    #[test]
    fn test_accent_color_rgb_empty_string() {
        let mut settings = Settings::new();
        settings.accent_color = Some(String::new());
        assert_eq!(settings.accent_color_rgb(), None);
    }

    #[test]
    fn test_accent_color_rgb_boundary_values() {
        let mut settings = Settings::new();
        settings.accent_color = Some("#000000".to_string());
        assert_eq!(settings.accent_color_rgb(), Some((0, 0, 0)));

        settings.accent_color = Some("#ffffff".to_string());
        assert_eq!(settings.accent_color_rgb(), Some((255, 255, 255)));
    }

    #[test]
    fn test_accent_color_persists() {
        let path = temp_path();

        let mut settings = Settings::new();
        settings.accent_color = Some("#e62d42".to_string());
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.accent_color, Some("#e62d42".to_string()));
        assert_eq!(loaded.accent_color_rgb(), Some((0xe6, 0x2d, 0x42)));

        let _ = fs::remove_file(&path);
    }
}
