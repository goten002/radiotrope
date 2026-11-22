//! Logo fetching and caching service
//!
//! Provides a unified interface for retrieving station logos,
//! handling both cache lookups and network fetching.

use crate::config::logos::{MAX_BYTES, REFRESH_AFTER};
use crate::data::cache::{decode_logo, CacheState, ImageCache, LogoInfo};
use crate::data::types::HasLogo;
use crate::error::{AppError, Result};
use crate::network::browse_logos::{row_rgba, Rgba};
use crate::network::failed_logos::FailedLogos;
use radiotrope::config::network::{user_agent, CONNECT_TIMEOUT_SECS};
use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

/// The longest a logo request may take from start to end. A logo is
/// small: a host that hasn't sent it by then is hung, and would hold up
/// the logos queued behind it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Logos downloaded at the same time
pub const FETCH_WORKERS: usize = 5;

type Job = Box<dyn FnOnce() + Send>;

/// Jobs run on up to `max` threads, which start when there is work and
/// end when it runs out
pub struct Workers {
    name: &'static str,
    max: usize,
    /// The jobs waiting, and how many threads are running
    state: Mutex<(VecDeque<Job>, usize)>,
}

impl Workers {
    pub fn new(name: &'static str, max: usize) -> Arc<Self> {
        Arc::new(Self {
            name,
            max,
            state: Mutex::default(),
        })
    }

    /// Run `job` on one of the threads, after the jobs given before it
    pub fn run(self: &Arc<Self>, job: impl FnOnce() + Send + 'static) {
        let mut state = self.lock();
        state.0.push_back(Box::new(job));
        if state.1 >= self.max {
            return;
        }
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name(self.name.into())
            .spawn(move || this.work());
        match spawned {
            Ok(_) => state.1 += 1,
            // The threads already running take the job; with none, it
            // waits for the next job's thread
            Err(e) => eprintln!("Failed to start a {} thread: {e}", self.name),
        }
    }

    fn work(&self) {
        loop {
            let job = {
                let mut state = self.lock();
                match state.0.pop_front() {
                    Some(job) => job,
                    None => {
                        state.1 -= 1;
                        return;
                    }
                }
            };
            // A job that panics doesn't take its thread's count with it
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        }
    }

    fn lock(&self) -> MutexGuard<'_, (VecDeque<Job>, usize)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What is known of an item's cached logo without touching a file (see
/// [`LogoService::glance`])
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glance {
    /// When the cached file was written; `None` with nothing cached
    pub modified: Option<SystemTime>,
    /// The logo is worth fetching: it is missing or out of date (see
    /// [`LogoService::is_missing`])
    pub wanted: bool,
}

/// Service for fetching and caching station logos
///
/// Combines the image cache with HTTP fetching to provide a simple
/// interface for getting logos - checking cache first, then fetching
/// from network if needed.
pub struct LogoService {
    cache: ImageCache,
    client: reqwest::blocking::Client,
    /// Logo URLs that failed lately, so they aren't fetched on every look
    failed: FailedLogos,
    /// The threads of [`fetch_in_background`](Self::fetch_in_background)
    fetchers: Arc<Workers>,
    /// Cache keys waiting for one of them, or being fetched by one
    queued: Mutex<HashSet<String>>,
    /// Cache keys whose logo a thread is fetching now. Another fetch of
    /// one waits for it (see [`claim`](Self::claim)).
    fetching: Mutex<HashSet<String>>,
    fetched: Condvar,
}

/// A logo this thread is fetching; dropping it lets the next one in
struct Claim<'a> {
    service: &'a LogoService,
    key: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.service.lock_fetching().remove(&self.key);
        self.service.fetched.notify_all();
    }
}

impl LogoService {
    /// Create a new logo service with default settings. A cache folder
    /// that can't be created doesn't stop it (see [`ImageCache::open`]).
    pub fn new() -> Result<Self> {
        Self::with_cache(ImageCache::open("logos"))
    }

    /// Create a logo service with a custom cache directory (for testing)
    pub fn with_cache(cache: ImageCache) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(user_agent())
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(AppError::from)?;

        Ok(Self {
            cache,
            client,
            failed: FailedLogos::new(),
            fetchers: Workers::new("logo-fetch", FETCH_WORKERS),
            queued: Mutex::default(),
            fetching: Mutex::default(),
            fetched: Condvar::new(),
        })
    }

    /// Get access to the underlying cache
    pub fn cache(&self) -> &ImageCache {
        &self.cache
    }

    /// Get mutable access to the underlying cache
    pub fn cache_mut(&mut self) -> &mut ImageCache {
        &mut self.cache
    }

    // =========================================================================
    // Main API
    // =========================================================================

    /// Get logo bytes for an item, fetching from network if not cached
    ///
    /// Returns `None` if:
    /// - The item has no logo URL
    /// - The fetch fails and nothing is cached
    pub fn get<T: HasLogo>(&self, item: &T) -> Option<Vec<u8>> {
        let key = item.logo_cache_key();
        // Check cache first
        if let Some(data) = self.cache.get(&key) {
            return Some(data);
        }
        let _claim = self.claim(&key);
        // Another fetch may have brought it meanwhile
        if let Some(data) = self.cache.get(&key) {
            return Some(data);
        }
        // Cached by an older build: shrunk here, off the UI thread
        if let Some(thumb) = self.cache.convert(&key) {
            return Some(thumb.png);
        }

        // Not cached - try to fetch
        let url = item.logo_url()?;
        let data = self.download(url).ok()?;

        // Cache it, as the thumbnail every later look gets (a cache that
        // can't be written still leaves us the thumbnail)
        match self.cache.store_thumbnail(&key, &data) {
            Some(thumb) => {
                if thumb.saved.is_ok() {
                    self.cache.set_source(&key, url);
                }
                Some(thumb.png)
            }
            None => {
                self.unusable(url);
                None
            }
        }
    }

    /// Get logo bytes only if already cached (no network request)
    pub fn get_cached<T: HasLogo>(&self, item: &T) -> Option<Vec<u8>> {
        self.cache.get_logo(item)
    }

    /// Get the path to the cached logo file (if cached)
    pub fn get_cached_path<T: HasLogo>(&self, item: &T) -> Option<PathBuf> {
        self.cache.get_logo_path(item)
    }

    /// Check if a logo is cached for the given item
    pub fn is_cached<T: HasLogo>(&self, item: &T) -> bool {
        self.cache.has_logo(item)
    }

    /// Ensure a logo is cached and up to date (see
    /// [`is_stale`](Self::is_stale)), downloading if necessary
    ///
    /// Returns:
    /// - `Ok(true)` if the logo was downloaded (or converted) and cached,
    ///   here or by another fetch this one waited for
    /// - `Ok(false)` if the logo was already cached
    /// - `Err` if there's no logo URL or the download failed. An out of
    ///   date logo stays cached then.
    pub fn ensure_cached<T: HasLogo>(&self, item: &T) -> Result<bool> {
        let key = item.logo_cache_key();
        if self.cache.state(&key) == CacheState::Ready && !self.is_stale(item) {
            return Ok(false);
        }
        let _claim = self.claim(&key);
        match self.cache.state(&key) {
            // Another fetch brought it, or refreshed it, meanwhile
            CacheState::Ready if !self.is_stale(item) => return Ok(true),
            CacheState::Ready => {}
            // Cached by an older build: shrunk here, off the UI thread
            CacheState::NeedsConversion => {
                if let Some(thumb) = self.cache.convert(&key) {
                    thumb.saved?;
                    return Ok(true);
                }
            }
            CacheState::Absent => {}
        }

        // Get URL
        let url = item
            .logo_url()
            .ok_or_else(|| AppError::NotFound("Item has no logo URL".to_string()))?;

        // Fetch and cache
        let data = self.download(url)?;
        let Some(thumb) = self.cache.store_thumbnail(&key, &data) else {
            self.unusable(url);
            return Err(AppError::Image(format!("{url} is not an image")));
        };
        // A cache that can't be written would have it downloaded on every
        // look
        thumb.saved.inspect_err(|e| self.failed.record(url, e))?;
        self.cache.set_source(&key, url);

        Ok(true)
    }

    /// `url` gave data that isn't an image we can show (a web page, an
    /// SVG that draws nothing): nothing is cached, and it isn't fetched again this session
    fn unusable(&self, url: &str) {
        self.failed.record_unusable(url);
    }

    /// Fetch `key`'s logo on this thread alone. The play path, the poll and
    /// the prefetch can all want the same logo at once: the later ones wait
    /// here, then find it cached (or failed) instead of downloading it too.
    fn claim(&self, key: &str) -> Claim<'_> {
        let mut fetching = self.lock_fetching();
        while fetching.contains(key) {
            fetching = self
                .fetched
                .wait(fetching)
                .unwrap_or_else(|e| e.into_inner());
        }
        fetching.insert(key.to_string());
        Claim {
            service: self,
            key: key.to_string(),
        }
    }

    fn lock_fetching(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.fetching.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `item` has a logo to fetch: it has a URL, isn't cached, and
    /// hasn't failed lately. A logo cached by an older build, still to be
    /// converted, counts too: [`prefetch`](Self::prefetch) converts it.
    /// A cached logo that is out of date (see [`is_stale`](Self::is_stale))
    /// counts too: [`prefetch`](Self::prefetch) downloads it again.
    ///
    /// What the cache holds is read from the disk the first time an item
    /// is asked about (see [`ImageCache::info`]), and from memory after.
    pub fn is_missing<T: HasLogo>(&self, item: &T) -> bool {
        let info = self.cache.info(&item.logo_cache_key());
        self.is_wanted(item, &info, SystemTime::now())
    }

    /// What is known of `item`'s cached logo, without touching a file:
    /// for the UI thread. `None` when nothing is known yet; asking
    /// [`is_missing`](Self::is_missing) or
    /// [`cached_modified`](Self::cached_modified) on another thread
    /// reads it.
    pub fn glance<T: HasLogo>(&self, item: &T) -> Option<Glance> {
        let info = self.cache.known(&item.logo_cache_key())?;
        Some(Glance {
            modified: info.modified,
            wanted: self.is_wanted(item, &info, SystemTime::now()),
        })
    }

    fn is_wanted<T: HasLogo>(&self, item: &T, info: &LogoInfo, now: SystemTime) -> bool {
        match info.state {
            CacheState::Ready => self.is_stale_given(item, info, now),
            CacheState::NeedsConversion => true,
            CacheState::Absent => item
                .logo_url()
                .is_some_and(|url| !url.is_empty() && !self.failed.has_failed(url)),
        }
    }

    /// Whether `item`'s cached logo should be downloaded again: it came
    /// from another URL than the item's logo URL now, it is older than
    /// [`REFRESH_AFTER`], or it was cached before its URL was noted. Not
    /// while its URL failed lately (the cached one is kept meanwhile).
    pub fn is_stale<T: HasLogo>(&self, item: &T) -> bool {
        self.is_stale_at(item, SystemTime::now())
    }

    fn is_stale_at<T: HasLogo>(&self, item: &T, now: SystemTime) -> bool {
        let info = self.cache.info(&item.logo_cache_key());
        self.is_stale_given(item, &info, now)
    }

    fn is_stale_given<T: HasLogo>(&self, item: &T, info: &LogoInfo, now: SystemTime) -> bool {
        let Some(url) = item.logo_url().filter(|url| !url.is_empty()) else {
            return false;
        };
        if self.failed.has_failed(url) {
            return false;
        }
        match &info.source {
            Some((from, at)) => {
                from != url
                    || now
                        .duration_since(*at)
                        .is_ok_and(|age| age >= REFRESH_AFTER)
            }
            None => true,
        }
    }

    /// When `item`'s cached logo file was last written (see
    /// [`ImageCache::modified`]). Read from the disk the first time, from
    /// memory after (see [`ImageCache::info`]).
    pub fn cached_modified<T: HasLogo>(&self, item: &T) -> Option<SystemTime> {
        self.cache.info(&item.logo_cache_key()).modified
    }

    /// Fetch `item`'s logo on one of [`FETCH_WORKERS`] threads, if it is
    /// missing or out of date (see [`is_missing`](Self::is_missing)), and
    /// call `arrived` there once it is cached. An item whose logo is
    /// already waiting or being fetched isn't queued again: the first
    /// call's `arrived` tells of it.
    pub fn fetch_in_background<T, F>(self: &Arc<Self>, item: T, arrived: F)
    where
        T: HasLogo + Send + 'static,
        F: FnOnce(&T) + Send + 'static,
    {
        let key = item.logo_cache_key();
        if !self.lock_queued().insert(key.clone()) {
            return;
        }
        let this = Arc::clone(self);
        self.fetchers.run(move || {
            let fetched = this.is_missing(&item) && this.ensure_cached(&item).is_ok();
            this.lock_queued().remove(&key);
            if fetched {
                arrived(&item);
            }
        });
    }

    fn lock_queued(&self) -> MutexGuard<'_, HashSet<String>> {
        self.queued.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Prefetch logos for multiple items
    ///
    /// Downloads and caches logos that aren't already cached, and converts
    /// the ones an older build cached, [`FETCH_WORKERS`] at a time.
    /// Returns the number of logos successfully fetched or converted.
    ///
    /// This is a blocking operation - for background prefetching,
    /// call this from a separate thread.
    pub fn prefetch<T: HasLogo + Sync>(&self, items: &[T]) -> usize {
        let next = AtomicUsize::new(0);
        let fetched = AtomicUsize::new(0);
        let work = || {
            while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
                // Skip if no URL, already cached, or failed lately
                if self.is_missing(item) && self.ensure_cached(item).is_ok() {
                    fetched.fetch_add(1, Ordering::Relaxed);
                }
            }
        };
        std::thread::scope(|scope| {
            // This thread is one of the workers
            for _ in 1..FETCH_WORKERS.min(items.len()) {
                scope.spawn(work);
            }
            work();
        });
        fetched.into_inner()
    }

    /// Prefetch logos, reporting progress via callback
    ///
    /// The callback receives (completed, total) counts.
    pub fn prefetch_with_progress<T: HasLogo, F>(&self, items: &[T], mut on_progress: F) -> usize
    where
        F: FnMut(usize, usize),
    {
        let total = items.len();
        let mut fetched = 0;
        let mut completed = 0;

        for item in items {
            // Skip if no URL
            if item.logo_url().is_none() {
                completed += 1;
                on_progress(completed, total);
                continue;
            }

            // Skip if already cached
            if self.cache.has_logo(item) {
                completed += 1;
                on_progress(completed, total);
                continue;
            }

            // Try to fetch and cache
            if self.ensure_cached(item).is_ok() {
                fetched += 1;
            }

            completed += 1;
            on_progress(completed, total);
        }

        fetched
    }

    /// Delete a cached logo
    pub fn delete<T: HasLogo>(&self, item: &T) {
        self.cache.delete_logo(item);
    }

    // =========================================================================
    // Low-level operations
    // =========================================================================

    /// Fetch image bytes from a URL without caching
    ///
    /// Useful for previews or one-time downloads. Anything larger than
    /// [`MAX_BYTES`] is refused: logo URLs can point at streams or huge
    /// files.
    pub fn fetch_raw(&self, url: &str) -> Result<Vec<u8>> {
        if url.is_empty() {
            return Err(AppError::NotFound("Empty URL".to_string()));
        }

        let response = self.client.get(url).send()?;

        if !response.status().is_success() {
            return Err(response.error_for_status().unwrap_err().into());
        }

        let too_large = || AppError::InvalidResponse(format!("Logo larger than {MAX_BYTES} bytes"));
        if response.content_length().is_some_and(|n| n > MAX_BYTES) {
            return Err(too_large());
        }
        // Content-Length may be missing or wrong: stop reading past the cap
        let mut data = Vec::new();
        response.take(MAX_BYTES + 1).read_to_end(&mut data)?;
        if data.len() as u64 > MAX_BYTES {
            return Err(too_large());
        }
        Ok(data)
    }

    /// [`fetch_raw`](Self::fetch_raw), unless `url` failed lately; a
    /// failure is remembered
    fn download(&self, url: &str) -> Result<Vec<u8>> {
        if self.failed.has_failed(url) {
            return Err(AppError::NotFound(format!("{url} failed lately")));
        }
        self.fetch_raw(url)
            .inspect(|_| self.failed.clear(url))
            .inspect_err(|e| self.failed.record(url, e))
    }

    /// Fetch image and decode to RGBA pixels
    ///
    /// Returns (rgba_bytes, width, height) for use with UI frameworks.
    pub fn fetch_rgba(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        let data = self.fetch_raw(url)?;
        self.decode_to_rgba(&data)
    }

    /// Decode image bytes to RGBA pixels (see [`decode_logo`] for the limits)
    pub fn decode_to_rgba(&self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
        let img =
            decode_logo(data).map_err(|e| AppError::Image(format!("Can't decode image: {}", e)))?;

        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        let bytes = rgba.into_raw();

        Ok((bytes, width, height))
    }

    /// Get logo as RGBA pixels, fetching if necessary
    ///
    /// Convenience method that combines get() with decode_to_rgba().
    pub fn get_rgba<T: HasLogo>(&self, item: &T) -> Option<(Vec<u8>, u32, u32)> {
        let data = self.get(item)?;
        self.decode_cached(item, &data)
    }

    /// Get cached logo as RGBA pixels (no network request)
    pub fn get_cached_rgba<T: HasLogo>(&self, item: &T) -> Option<(Vec<u8>, u32, u32)> {
        let data = self.get_cached(item)?;
        self.decode_cached(item, &data)
    }

    /// `item`'s cached logo shrunk for a list row (see
    /// [`browse_logos::ROW_LOGO_SIZE`](crate::network::browse_logos::ROW_LOGO_SIZE)),
    /// without any network request. One that fails to decode is deleted,
    /// as by [`get_cached_rgba`](Self::get_cached_rgba).
    pub fn get_cached_row_rgba<T: HasLogo>(&self, item: &T) -> Option<Rgba> {
        let data = self.get_cached(item)?;
        let logo = row_rgba(&data);
        if logo.is_none() {
            self.cache.delete_logo(item);
        }
        logo
    }

    /// Decode `item`'s cached logo. One that fails to decode (what older
    /// builds kept of a web page, say) is deleted, so it is fetched again,
    /// and then not kept if it still isn't an image.
    fn decode_cached<T: HasLogo>(&self, item: &T, data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        let decoded = self.decode_to_rgba(data).ok();
        if decoded.is_none() {
            self.cache.delete_logo(item);
        }
        decoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::types::Station;
    use std::env::temp_dir;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_cache() -> ImageCache {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let thread_id = std::thread::current().id();
        let dir = temp_dir().join(format!("radiotrope_logo_test_{}_{:?}", id, thread_id));
        // Clean up any existing directory from previous runs
        let _ = std::fs::remove_dir_all(&dir);
        ImageCache::with_dir(dir).unwrap()
    }

    #[test]
    fn test_service_creation() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();
        assert!(!service.cache().dir().to_string_lossy().is_empty());
    }

    #[test]
    fn test_is_cached_false_initially() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();
        let station = Station::new("Test", "http://test.com/stream");

        assert!(!service.is_cached(&station));
    }

    #[test]
    fn test_get_cached_returns_none_when_not_cached() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();
        let station = Station::new("Test", "http://test.com/stream");

        assert!(service.get_cached(&station).is_none());
    }

    #[test]
    fn test_manual_cache_then_get() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let station =
            Station::new("Test", "http://test.com/stream").with_logo("http://test.com/logo.png");

        // Manually put data in cache
        let data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        service.cache().put_logo(&station, &data).unwrap();

        // Should be cached now
        assert!(service.is_cached(&station));

        // Should return the cached data
        let retrieved = service.get_cached(&station).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_delete_removes_from_cache() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let station =
            Station::new("Test", "http://test.com/stream").with_logo("http://test.com/logo.png");

        // Add to cache
        let data = vec![1, 2, 3, 4];
        service.cache().put_logo(&station, &data).unwrap();
        assert!(service.is_cached(&station));

        // Delete
        service.delete(&station);
        assert!(!service.is_cached(&station));
    }

    #[test]
    fn test_station_without_logo_url() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        // Station without logo URL
        let station = Station::new("Test", "http://test.com/stream");

        // ensure_cached should fail
        let result = service.ensure_cached(&station);
        assert!(result.is_err());

        // get should return None (no URL to fetch)
        assert!(service.get(&station).is_none());
    }

    #[test]
    fn test_prefetch_skips_already_cached() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let station =
            Station::new("Test", "http://test.com/stream").with_logo("http://test.com/logo.png");

        // Pre-cache
        let data = vec![1, 2, 3, 4];
        service.cache().put_logo(&station, &data).unwrap();

        // Prefetch should skip (already cached) and return 0
        let fetched = service.prefetch(&[station]);
        assert_eq!(fetched, 0);
    }

    #[test]
    fn test_decode_to_rgba_valid_png() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        // Create a minimal 1x1 PNG using the image crate
        use image::{ImageBuffer, Rgba};
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(1, 1, Rgba([255, 0, 0, 255]));

        let mut png_data = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut png_data);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();

        let result = service.decode_to_rgba(&png_data);
        assert!(result.is_ok());

        let (rgba, width, height) = result.unwrap();
        assert_eq!(width, 1);
        assert_eq!(height, 1);
        assert_eq!(rgba.len(), 4); // 1 pixel * 4 bytes (RGBA)
        assert_eq!(rgba, vec![255, 0, 0, 255]); // Red pixel
    }

    #[test]
    fn test_decode_to_rgba_invalid_data() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let invalid_data = vec![0, 1, 2, 3, 4, 5];
        let result = service.decode_to_rgba(&invalid_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_get_cached_path() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let station =
            Station::new("Test", "http://test.com/stream").with_logo("http://test.com/logo.png");

        // Not cached
        assert!(service.get_cached_path(&station).is_none());

        // Cache it
        let data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        service.cache().put_logo(&station, &data).unwrap();

        // Now has path
        let path = service.get_cached_path(&station);
        assert!(path.is_some());
        assert!(path.unwrap().exists());
    }

    #[test]
    fn test_get_cached_rgba() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let station =
            Station::new("Test", "http://test.com/stream").with_logo("http://test.com/logo.png");

        // Not cached
        assert!(service.get_cached_rgba(&station).is_none());

        // Cache a valid PNG
        use image::{ImageBuffer, Rgba};
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(2, 2, Rgba([0, 255, 0, 255]));
        let mut png_data = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut png_data);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();

        service.cache().put_logo(&station, &png_data).unwrap();

        // Now should decode
        let rgba = service.get_cached_rgba(&station);
        assert!(rgba.is_some());
        let (bytes, width, height) = rgba.unwrap();
        assert_eq!(width, 2);
        assert_eq!(height, 2);
        assert_eq!(bytes.len(), 16); // 2x2 pixels * 4 bytes
    }

    #[test]
    fn test_fetch_raw_empty_url() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let result = service.fetch_raw("");
        assert!(result.is_err());
    }

    #[test]
    fn test_prefetch_with_progress_callback() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        // Stations without valid URLs won't actually fetch, but progress should work
        let stations = vec![
            Station::new("Station 1", "http://s1.com"),
            Station::new("Station 2", "http://s2.com"),
            Station::new("Station 3", "http://s3.com"),
        ];

        let mut progress_calls = Vec::new();
        service.prefetch_with_progress(&stations, |completed, total| {
            progress_calls.push((completed, total));
        });

        // Should have called progress for each station
        assert_eq!(progress_calls.len(), 3);
        assert_eq!(progress_calls[0], (1, 3));
        assert_eq!(progress_calls[1], (2, 3));
        assert_eq!(progress_calls[2], (3, 3));
    }

    #[test]
    fn test_ensure_cached_already_cached() {
        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let station =
            Station::new("Test", "http://test.com/stream").with_logo("http://test.com/logo.png");

        // Pre-cache
        let data = vec![1, 2, 3, 4];
        service.cache().put_logo(&station, &data).unwrap();
        service
            .cache()
            .set_source(&station.logo_cache_key(), "http://test.com/logo.png");

        // ensure_cached should return Ok(false) - already cached
        let result = service.ensure_cached(&station);
        assert!(result.is_ok());
        assert!(!result.unwrap()); // false = was already cached
    }

    #[test]
    fn test_cache_access() {
        let cache = temp_cache();
        let mut service = LogoService::with_cache(cache).unwrap();

        // Test cache() accessor
        assert!(service.cache().dir().exists());

        // Test cache_mut() accessor
        let _ = service.cache_mut().clear();
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([9, 99, 199, 255]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn a_download_past_the_cap_is_refused_and_not_retried() {
        use crate::network::test_http::{ok, serve};
        let service = LogoService::with_cache(temp_cache()).unwrap();
        let big = vec![0u8; MAX_BYTES as usize + 1];

        // Said up front, and found out while reading
        for with_length in [true, false] {
            let url = format!("{}/logo.png", serve(ok(&big, with_length), Duration::ZERO));
            assert!(matches!(
                service.fetch_raw(&url),
                Err(AppError::InvalidResponse(_))
            ));
            let station = Station::new("Big", "http://big.test/stream").with_logo(&url);
            assert!(service.ensure_cached(&station).is_err());
            // Too large is for good: no second download
            assert!(service.failed.has_failed(&url));
            assert!(!service.is_missing(&station));
        }

        let url = format!("{}/logo.png", serve(ok(&png(4, 4), true), Duration::ZERO));
        assert_eq!(service.fetch_raw(&url).unwrap(), png(4, 4));
    }

    #[test]
    fn a_downloaded_logo_comes_back_as_its_thumbnail() {
        use crate::data::cache::LOGO_MAX_SIZE;
        use crate::network::test_http::{ok, serve};
        let service = LogoService::with_cache(temp_cache()).unwrap();
        let url = format!(
            "{}/logo.png",
            serve(ok(&png(640, 320), true), Duration::ZERO)
        );
        let station = Station::new("Wide", "http://wide.test/stream").with_logo(&url);
        let (_, w, h) = service.get_rgba(&station).unwrap();
        assert_eq!((w, h), (LOGO_MAX_SIZE, LOGO_MAX_SIZE / 2));
    }

    #[test]
    fn a_logo_cached_by_an_older_build_is_converted_by_the_prefetch() {
        use crate::data::cache::LOGO_MAX_SIZE;
        let cache = temp_cache();
        let dir = cache.dir().to_path_buf();
        let service = LogoService::with_cache(cache).unwrap();
        // No logo URL: nothing to download, only the old entry to convert
        let station = Station::new("Old", "http://old.test/stream");
        std::fs::write(
            dir.join(format!("{}.png", station.logo_cache_key())),
            png(400, 400),
        )
        .unwrap();

        // What the UI thread sees: no logo yet, and one to fetch
        assert!(service.get_cached_rgba(&station).is_none());
        assert!(service.is_missing(&station));

        assert_eq!(service.prefetch(std::slice::from_ref(&station)), 1);
        let (_, w, h) = service.get_cached_rgba(&station).unwrap();
        assert_eq!((w, h), (LOGO_MAX_SIZE, LOGO_MAX_SIZE));
        assert!(!service.is_missing(&station));
    }

    #[test]
    fn a_page_instead_of_a_logo_is_not_cached_or_fetched_again() {
        use crate::network::test_http::{ok, serve};
        let service = LogoService::with_cache(temp_cache()).unwrap();
        for path in ["prefetched.png", "played.png"] {
            let server = serve(ok(b"<html>Moved</html>", true), Duration::ZERO);
            let url = format!("{server}/{path}");
            let station = Station::new(path, format!("http://{path}.test/stream")).with_logo(&url);
            if path == "prefetched.png" {
                assert!(service.ensure_cached(&station).is_err());
            } else {
                assert!(service.get_rgba(&station).is_none());
            }
            assert!(!service.is_cached(&station));
            assert!(service.failed.has_failed(&url));
            assert!(!service.is_missing(&station));
        }
    }

    #[test]
    fn a_cached_logo_that_fails_to_decode_is_dropped() {
        let service = LogoService::with_cache(temp_cache()).unwrap();
        let station = Station::new("Junk", "http://junk.test/stream")
            .with_logo("http://logo.invalid/junk.png");
        // What older builds kept of a web page
        let file = service
            .cache()
            .dir()
            .join(format!("{}.png", station.logo_cache_key()));
        std::fs::write(&file, b"<html>Moved</html>").unwrap();

        assert!(service.get_cached_rgba(&station).is_none());
        assert!(!file.exists());
        // So it is fetched again
        assert!(service.is_missing(&station));
    }

    #[test]
    fn a_logo_wanted_three_times_at_once_is_downloaded_once() {
        use crate::network::test_http::{ok, serve_counted};
        let service = Arc::new(LogoService::with_cache(temp_cache()).unwrap());
        let (server, requests) = serve_counted(ok(&png(8, 8), true), Duration::from_millis(300));
        let station = Station::new("Popular", "http://popular.test/stream")
            .with_logo(format!("{server}/logo.png").as_str());

        // The prefetch, the play path and the poll
        let fetches: Vec<_> = (0..3)
            .map(|i| {
                let service = service.clone();
                let station = station.clone();
                std::thread::spawn(move || match i {
                    0 => service.ensure_cached(&station).is_ok(),
                    _ => service.get_rgba(&station).is_some(),
                })
            })
            .collect();
        for fetch in fetches {
            assert!(fetch.join().unwrap());
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(service.lock_fetching().is_empty());
    }

    #[test]
    fn huge_images_are_not_decoded() {
        let service = LogoService::with_cache(temp_cache()).unwrap();
        // A few kB of PNG that would unpack to 5000 pixels a row
        assert!(service.decode_to_rgba(&png(5000, 1)).is_err());
        assert!(service.decode_to_rgba(&png(100, 100)).is_ok());
    }

    /// Five stations whose logos a server sends after `delay`
    fn slow_logos(delay: Duration) -> (Vec<Station>, Arc<std::sync::atomic::AtomicUsize>) {
        use crate::network::test_http::{ok, serve_counted};
        let (server, requests) = serve_counted(ok(&png(8, 8), true), delay);
        let stations = (0..5)
            .map(|i| {
                Station::new("Slow", format!("http://slow.test/{i}"))
                    .with_logo(format!("{server}/{i}.png").as_str())
            })
            .collect();
        (stations, requests)
    }

    #[test]
    fn logos_are_fetched_several_at_a_time() {
        let service = LogoService::with_cache(temp_cache()).unwrap();
        let (stations, requests) = slow_logos(Duration::from_millis(600));
        let started = std::time::Instant::now();
        assert_eq!(service.prefetch(&stations), 5);
        // One after the other they would take 3 s
        assert!(started.elapsed() < Duration::from_millis(2400));
        assert_eq!(requests.load(Ordering::SeqCst), 5);
        assert!(stations.iter().all(|s| service.is_cached(s)));
    }

    #[test]
    fn background_fetches_tell_of_each_logo_as_it_arrives() {
        let service = Arc::new(LogoService::with_cache(temp_cache()).unwrap());
        let (stations, requests) = slow_logos(Duration::from_millis(100));
        let (tx, rx) = std::sync::mpsc::channel();
        for station in &stations {
            // Asked for twice (two redraws of the list): fetched once
            for _ in 0..2 {
                let tx = tx.clone();
                service.fetch_in_background(station.clone(), move |station: &Station| {
                    let _ = tx.send(station.url.clone());
                });
            }
        }
        let mut arrived: Vec<String> = (0..5)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        arrived.sort();
        let mut wanted: Vec<String> = stations.iter().map(|s| s.url.clone()).collect();
        wanted.sort();
        assert_eq!(arrived, wanted);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        assert_eq!(requests.load(Ordering::SeqCst), 5);
        assert!(service.lock_queued().is_empty());
    }

    #[test]
    fn a_refused_logo_is_not_told_of_or_fetched_again() {
        use crate::network::test_http::serve_counted;
        let service = Arc::new(LogoService::with_cache(temp_cache()).unwrap());
        let (server, requests) = serve_counted(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
            Duration::ZERO,
        );
        let station = Station::new("Gone", "http://gone.test/stream")
            .with_logo(format!("{server}/logo.png").as_str());
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..2 {
            let tx = tx.clone();
            service.fetch_in_background(station.clone(), move |_: &Station| {
                let _ = tx.send(());
            });
            assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(!service.is_missing(&station));
    }

    #[test]
    fn a_hung_logo_host_is_given_up_on_after_the_request_timeout() {
        // Checked by the value: a test waiting it out would take as long
        assert!(REQUEST_TIMEOUT <= Duration::from_secs(10));
    }

    #[test]
    fn a_glance_reads_no_files_and_follows_what_the_service_writes() {
        let service = LogoService::with_cache(temp_cache()).unwrap();
        let station =
            Station::new("Test", "http://test.com/stream").with_logo("http://test.com/logo.png");
        let key = station.logo_cache_key();

        // Nothing known until a worker asks
        assert_eq!(service.glance(&station), None);
        assert!(service.is_missing(&station));
        assert_eq!(
            service.glance(&station),
            Some(Glance {
                modified: None,
                wanted: true
            })
        );

        // A logo written through the cache is unknown again, then known
        assert!(service.cache().put_thumbnail(&key, &png(8, 8)).is_some());
        service.cache().set_source(&key, "http://test.com/logo.png");
        assert_eq!(service.glance(&station), None);
        let written = service.cached_modified(&station);
        assert!(written.is_some());
        assert_eq!(
            service.glance(&station),
            Some(Glance {
                modified: written,
                wanted: false
            })
        );

        // A new logo address shows without asking the disk again
        let moved = station.clone().with_logo("http://test.com/new.png");
        assert_eq!(
            service.glance(&moved),
            Some(Glance {
                modified: written,
                wanted: true
            })
        );

        service.delete(&station);
        assert_eq!(service.glance(&station), None);
    }

    #[test]
    fn a_cached_logo_is_shrunk_for_a_row() {
        use crate::network::browse_logos::ROW_LOGO_SIZE;
        let service = LogoService::with_cache(temp_cache()).unwrap();
        let station = Station::new("Test", "http://test.com/stream");
        service.cache().put_logo(&station, &png(256, 128)).unwrap();
        let (rgba, w, h) = service.get_cached_row_rgba(&station).unwrap();
        assert_eq!((w, h), (ROW_LOGO_SIZE, ROW_LOGO_SIZE / 2));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        // The full size stays for the header
        assert_eq!(service.get_cached_rgba(&station).unwrap().1, 256);
    }

    #[test]
    fn test_favorite_works_with_service() {
        use crate::data::types::Favorite;

        let cache = temp_cache();
        let service = LogoService::with_cache(cache).unwrap();

        let favorite = Favorite::new("Test Radio", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

        assert!(!service.is_cached(&favorite));

        // Cache it
        let data = vec![1, 2, 3, 4];
        service.cache().put_logo(&favorite, &data).unwrap();

        assert!(service.is_cached(&favorite));
        assert_eq!(service.get_cached(&favorite).unwrap(), data);
    }

    #[test]
    fn a_logo_is_stale_when_its_url_changes_or_it_ages() {
        let service = LogoService::with_cache(temp_cache()).unwrap();
        let station = Station::new("Test", "http://test.com/stream")
            .with_logo("http://test.com/logo.png?v=1");
        let key = station.logo_cache_key();
        let cache = service.cache();
        assert!(cache.put_thumbnail(&key, &png(8, 8)).is_some());

        // Cached before its URL was noted: downloaded again once
        assert!(service.is_stale(&station));

        cache.set_source(&key, "http://test.com/logo.png?v=1");
        assert!(!service.is_stale(&station));
        let now = SystemTime::now();
        assert!(!service.is_stale_at(&station, now + REFRESH_AFTER / 2));
        assert!(service.is_stale_at(&station, now + REFRESH_AFTER + Duration::from_secs(60)));

        // The station's logo moved to a new address
        let moved = station.clone().with_logo("http://test.com/logo.png?v=2");
        assert!(service.is_stale(&moved));
        assert!(service.is_missing(&moved), "prefetch picks it up");

        // No logo URL: nothing to download
        let bare = Station::new("Test", "http://test.com/stream");
        assert!(!service.is_stale(&bare));
    }

    #[test]
    fn a_logo_whose_url_failed_is_not_stale_meanwhile() {
        let service = LogoService::with_cache(temp_cache()).unwrap();
        let station =
            Station::new("Test", "http://test.com/stream").with_logo("http://test.com/new.png");
        let key = station.logo_cache_key();
        assert!(service.cache().put_thumbnail(&key, &png(8, 8)).is_some());
        service.cache().set_source(&key, "http://test.com/old.png");
        assert!(service.is_stale(&station));

        // The new one couldn't be fetched: the old one is kept for now
        service.unusable("http://test.com/new.png");
        assert!(!service.is_stale(&station));
        assert!(service.get_cached(&station).is_some());
    }
}
