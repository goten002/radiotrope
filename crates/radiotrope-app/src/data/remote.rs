//! The phones paired with the player, for the Remote API
//!
//! Kept in `remote.json` next to the agents' token (the config folder; on
//! Windows the local app data folder), readable by the user only: it holds
//! each phone's token. Also holds the player's own id, made once, which
//! phones use to find this player again after its address changes.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::remote::{FILE, MAX_DEVICES, MAX_DEVICE_NAME_CHARS};
use crate::data::{agent_token, storage};

/// What `remote.json` holds
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemoteStore {
    /// This player's id, as announced on the network
    pub player_id: String,
    #[serde(default)]
    pub devices: Vec<Device>,
}

/// A paired phone
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// The id the phone made for itself
    pub id: String,
    /// What the phone calls itself, e.g. "George's Pixel"
    pub name: String,
    /// What it sends as `Authorization: Bearer <token>`
    pub token: String,
    /// Unix time it was paired
    pub paired_at: i64,
    /// Unix time of its last request
    #[serde(default)]
    pub last_used: i64,
}

impl RemoteStore {
    /// The saved store, or a new one (with a new player id) saved first.
    /// A damaged file is moved aside and a new one made: phones pair again.
    pub fn load_or_create() -> io::Result<Self> {
        Self::load_or_create_at(&path()?)
    }

    pub fn load_or_create_at(path: &Path) -> io::Result<Self> {
        let loaded = storage::load_from::<RemoteStore>(path).unwrap_or_else(|e| {
            eprintln!("Paired phones could not be read, so they need pairing again: {e}");
            None
        });
        match loaded {
            Some(store) if is_hex(&store.player_id, 32) => Ok(store),
            loaded => {
                let store = RemoteStore {
                    player_id: random_hex(16)?,
                    devices: loaded.map(|s| s.devices).unwrap_or_default(),
                };
                store.save_at(path)?;
                Ok(store)
            }
        }
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_at(&path()?)
    }

    pub fn save_at(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        storage::write_private(path, &json)
    }

    /// Pair a phone: a new token for it, replacing the one it had. With
    /// [`MAX_DEVICES`] paired, the one used longest ago makes way.
    pub fn pair(&mut self, id: &str, name: &str, now: i64) -> io::Result<String> {
        let token = random_hex(32)?;
        self.devices.retain(|d| d.id != id);
        while self.devices.len() >= MAX_DEVICES {
            let oldest = self
                .devices
                .iter()
                .enumerate()
                .min_by_key(|(_, d)| d.last_used.max(d.paired_at))
                .map(|(i, _)| i);
            match oldest {
                Some(i) => self.devices.remove(i),
                None => break,
            };
        }
        self.devices.push(Device {
            id: id.to_string(),
            name: device_name(name),
            token: token.clone(),
            paired_at: now,
            last_used: now,
        });
        Ok(token)
    }

    /// The phone that holds this token
    pub fn find_by_token(&self, presented: &str) -> Option<&Device> {
        self.devices
            .iter()
            .find(|d| agent_token::matches(&d.token, presented))
    }

    /// Forget a phone; its token stops working. Tells whether it was there.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| d.id != id);
        self.devices.len() != before
    }

    /// Note a phone's request. Tells whether its time moved by a minute
    /// or more, so callers save only then.
    pub fn touch(&mut self, id: &str, now: i64) -> bool {
        match self.devices.iter_mut().find(|d| d.id == id) {
            Some(d) if now - d.last_used >= 60 => {
                d.last_used = now;
                true
            }
            _ => false,
        }
    }
}

/// Saves wait here for their turn
static SAVING: Mutex<()> = Mutex::new(());

/// Save what `store` holds when this save's turn comes, to `path` or the
/// usual file. Saves run one at a time and each writes the phones as they
/// are then, so an older list never lands after a newer one.
pub fn save_shared(store: &Mutex<RemoteStore>, path: Option<&Path>) -> io::Result<()> {
    let _turn = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let latest = store.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match path {
        Some(path) => latest.save_at(path),
        None => latest.save(),
    }
}

/// A phone's name as kept: trimmed, without control characters, cut at
/// [`MAX_DEVICE_NAME_CHARS`]; "Phone" when nothing is left
pub fn device_name(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    match name.trim() {
        "" => "Phone".to_string(),
        name => name.to_string(),
    }
}

fn path() -> io::Result<PathBuf> {
    let dir = agent_token::private_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(FILE))
}

/// `bytes` random bytes as hex
pub fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("radiotrope-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn the_player_id_is_made_once_and_kept() {
        let path = temp_file("id.json");
        let first = RemoteStore::load_or_create_at(&path).unwrap();
        assert_eq!(first.player_id.len(), 32);
        let again = RemoteStore::load_or_create_at(&path).unwrap();
        assert_eq!(first.player_id, again.player_id);
    }

    #[test]
    fn a_paired_phone_is_found_by_its_token_until_removed() {
        let path = temp_file("pair.json");
        let mut store = RemoteStore::load_or_create_at(&path).unwrap();
        let token = store.pair("phone-1", "  George's Pixel\n", 100).unwrap();
        assert_eq!(token.len(), 64);
        store.save_at(&path).unwrap();

        let store = RemoteStore::load_or_create_at(&path).unwrap();
        let device = store.find_by_token(&token).unwrap();
        assert_eq!(device.name, "George's Pixel");
        assert!(store.find_by_token("wrong").is_none());

        let mut store = store;
        assert!(store.remove("phone-1"));
        assert!(store.find_by_token(&token).is_none());
    }

    #[test]
    fn pairing_again_replaces_the_old_token() {
        let mut store = RemoteStore::default();
        let old = store.pair("phone-1", "Pixel", 1).unwrap();
        let new = store.pair("phone-1", "Pixel", 2).unwrap();
        assert_eq!(store.devices.len(), 1);
        assert!(store.find_by_token(&old).is_none());
        assert!(store.find_by_token(&new).is_some());
    }

    #[test]
    fn the_phone_used_longest_ago_makes_way() {
        let mut store = RemoteStore::default();
        for i in 0..MAX_DEVICES {
            store
                .pair(&format!("p{i}"), "Phone", 100 + i as i64)
                .unwrap();
        }
        store.touch("p0", 10_000);
        store.pair("new", "Phone", 20_000).unwrap();
        assert_eq!(store.devices.len(), MAX_DEVICES);
        assert!(store.devices.iter().any(|d| d.id == "p0"));
        assert!(!store.devices.iter().any(|d| d.id == "p1"));
    }

    #[test]
    fn names_are_tidied() {
        assert_eq!(device_name("\u{7}  "), "Phone");
        assert_eq!(
            device_name(&"x".repeat(100)).chars().count(),
            MAX_DEVICE_NAME_CHARS
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_file("mode.json");
        RemoteStore::load_or_create_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
