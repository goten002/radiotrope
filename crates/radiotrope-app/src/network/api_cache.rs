//! On-disk cache for station directory API responses
//!
//! Stores raw JSON response bodies under `<cache dir>/api/`, one file per
//! request, named by a hash of the request. An entry is fresh for the TTL the
//! caller passes; a stale entry is still used when the network request fails,
//! so the browser keeps working offline. Entries older than
//! [`API_CACHE_MAX_AGE`] are deleted when the cache is opened.

use crate::config::providers::API_CACHE_MAX_AGE;
use crate::data::cache::ensure_cache_dir;
use crate::data::types::fnv1a;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

/// Cache of API response bodies in one directory
#[derive(Debug, Clone)]
pub struct ApiCache {
    dir: PathBuf,
}

impl ApiCache {
    /// Open the cache in the application cache directory
    ///
    /// Old entries are pruned the first time this is called in a process.
    /// Returns `None` when the cache directory cannot be created.
    pub fn open_default() -> Option<Self> {
        static PRUNED: OnceLock<()> = OnceLock::new();
        let dir = ensure_cache_dir().ok()?.join("api");
        let cache = Self::with_dir(dir)?;
        PRUNED.get_or_init(|| cache.prune(API_CACHE_MAX_AGE));
        Some(cache)
    }

    /// Open a cache in `dir`, creating it if needed
    pub fn with_dir(dir: PathBuf) -> Option<Self> {
        fs::create_dir_all(&dir).ok()?;
        Some(Self { dir })
    }

    /// The directory holding the entries
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Body stored for `key` if it was written less than `ttl` ago
    pub fn get_fresh(&self, key: &str, ttl: Duration) -> Option<Vec<u8>> {
        let path = self.path(key);
        let age = age_of(&path)?;
        if age < ttl {
            fs::read(path).ok()
        } else {
            None
        }
    }

    /// Body stored for `key`, however old
    pub fn get_any(&self, key: &str) -> Option<Vec<u8>> {
        fs::read(self.path(key)).ok()
    }

    /// Store `body` for `key`. Failures are ignored: the cache is optional.
    pub fn put(&self, key: &str, body: &[u8]) {
        let path = self.path(key);
        // Write then rename, so a reader never sees a half-written file
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, body).is_ok() && fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Delete entries older than `max_age`
    pub fn prune(&self, max_age: Duration) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if age_of(&path).is_some_and(|age| age >= max_age) {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir
            .join(format!("{:016x}.json", fnv1a(key.as_bytes())))
    }
}

/// Time since the file was last written
fn age_of(path: &Path) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    // A timestamp in the future (clock change) counts as brand new
    Some(
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache(name: &str) -> ApiCache {
        let dir = std::env::temp_dir().join(format!(
            "radiotrope-api-cache-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        ApiCache::with_dir(dir).unwrap()
    }

    #[test]
    fn test_put_then_get_fresh() {
        let cache = temp_cache("fresh");
        cache.put("GET a", b"[1]");
        assert_eq!(
            cache.get_fresh("GET a", Duration::from_secs(60)).unwrap(),
            b"[1]"
        );
        assert!(cache.get_fresh("GET b", Duration::from_secs(60)).is_none());
        let _ = fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn test_expired_entry_is_stale_but_available() {
        let cache = temp_cache("stale");
        cache.put("GET a", b"[1]");
        assert!(cache.get_fresh("GET a", Duration::ZERO).is_none());
        assert_eq!(cache.get_any("GET a").unwrap(), b"[1]");
        let _ = fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn test_prune_removes_old_entries() {
        let cache = temp_cache("prune");
        cache.put("GET a", b"[1]");
        cache.prune(Duration::from_secs(3600));
        assert!(cache.get_any("GET a").is_some());
        cache.prune(Duration::ZERO);
        assert!(cache.get_any("GET a").is_none());
        let _ = fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn test_keys_map_to_distinct_files() {
        let cache = temp_cache("keys");
        assert_ne!(cache.path("GET a"), cache.path("GET b"));
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    }
}
