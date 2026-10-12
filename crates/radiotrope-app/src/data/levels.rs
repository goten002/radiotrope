//! How loud each station is, remembered for station volume matching
//!
//! Kept in levels.json by stream URL: a station heard before starts at the
//! right volume instead of being learned again. Stations not heard for
//! [`KEEP_DAYS`] are forgotten when the file is read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::storage;

pub const LEVELS_FILE: &str = "levels.json";

/// Stations not played for this long are dropped
pub const KEEP_DAYS: i64 = 90;

/// One station's loudness
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Level {
    /// Its loudness in LUFS
    pub lufs: f32,
    /// When it was last measured (Unix seconds)
    pub seen: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct LevelsFile {
    #[serde(default)]
    stations: HashMap<String, Level>,
}

/// The remembered loudness of the stations played
#[derive(Default)]
pub struct Levels {
    /// Where they are saved; `None` keeps them in memory (tests)
    path: Option<PathBuf>,
    stations: HashMap<String, Level>,
}

impl Levels {
    /// From levels.json in the config folder. A missing or broken file
    /// starts empty.
    pub fn load(now: i64) -> Self {
        match storage::data_path(LEVELS_FILE) {
            Ok(path) => Self::load_from(&path, now),
            Err(_) => Self::default(),
        }
    }

    pub fn load_from(path: &Path, now: i64) -> Self {
        let stations = match storage::load_from::<LevelsFile>(path) {
            Ok(Some(file)) => file.stations,
            Ok(None) => HashMap::new(),
            Err(e) => {
                eprintln!("Station levels not read, starting over: {e}");
                HashMap::new()
            }
        };
        let mut levels = Self {
            path: Some(path.to_path_buf()),
            stations,
        };
        levels.forget_old(now);
        levels
    }

    fn forget_old(&mut self, now: i64) {
        let oldest = now - KEEP_DAYS * 24 * 60 * 60;
        self.stations
            .retain(|_, level| level.seen >= oldest && level.lufs.is_finite());
    }

    /// The loudness remembered for `url`
    pub fn get(&self, url: &str) -> Option<f32> {
        self.stations.get(url).map(|level| level.lufs)
    }

    /// Remember `url`'s loudness, and save
    pub fn remember(&mut self, url: &str, lufs: f32, now: i64) {
        if url.is_empty() || !lufs.is_finite() {
            return;
        }
        self.stations
            .insert(url.to_string(), Level { lufs, seen: now });
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let file = LevelsFile {
            stations: self.stations.clone(),
        };
        if let Err(e) = storage::save_to(path, &file) {
            eprintln!("Station levels not saved: {e}");
        }
    }

    pub fn len(&self) -> usize {
        self.stations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60;

    /// A fresh folder for one test
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radiotrope_levels_test_{}_{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn remembers_and_reads_back() {
        let path = test_dir("remembers").join(LEVELS_FILE);
        let mut levels = Levels::load_from(&path, 1_000 * DAY);
        assert!(levels.is_empty());
        levels.remember("http://a/stream", -9.5, 1_000 * DAY);
        levels.remember("http://b/stream", -18.0, 1_000 * DAY);

        let again = Levels::load_from(&path, 1_001 * DAY);
        assert_eq!(again.get("http://a/stream"), Some(-9.5));
        assert_eq!(again.get("http://b/stream"), Some(-18.0));
        assert_eq!(again.get("http://c/stream"), None);
    }

    #[test]
    fn forgets_stations_not_heard_for_90_days() {
        let path = test_dir("forgets").join(LEVELS_FILE);
        let mut levels = Levels::load_from(&path, 0);
        levels.remember("old", -10.0, 0);
        levels.remember("recent", -12.0, 50 * DAY);

        let later = Levels::load_from(&path, (KEEP_DAYS + 10) * DAY);
        assert_eq!(later.get("old"), None);
        assert_eq!(later.get("recent"), Some(-12.0));
    }

    #[test]
    fn a_broken_file_starts_empty() {
        let path = test_dir("broken").join(LEVELS_FILE);
        std::fs::write(&path, "{ not json").unwrap();
        let mut levels = Levels::load_from(&path, 0);
        assert!(levels.is_empty());
        // And is written over
        levels.remember("a", -14.0, 0);
        assert_eq!(Levels::load_from(&path, 0).get("a"), Some(-14.0));
    }

    #[test]
    fn nonsense_is_not_kept() {
        let mut levels = Levels::default();
        levels.remember("a", f32::NAN, 0);
        levels.remember("", -10.0, 0);
        assert!(levels.is_empty());
    }
}
