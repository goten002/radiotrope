//! Logo fetching and caching service
//!
//! Provides a unified interface for retrieving station logos,
//! handling both cache lookups and network fetching.

use crate::config::logos::MAX_BYTES;
use crate::data::cache::{decode_logo, CacheState, ImageCache};
use crate::data::types::HasLogo;
use crate::error::{AppError, Result};
use crate::network::failed_logos::FailedLogos;
use radiotrope::config::network::{CONNECT_TIMEOUT_SECS, READ_TIMEOUT_SECS, USER_AGENT};
use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
    /// A background prefetch is running, and whether another was asked for
    /// meanwhile
    prefetching: Mutex<(bool, bool)>,
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
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(READ_TIMEOUT_SECS))
            .build()
            .map_err(AppError::from)?;

        Ok(Self {
            cache,
            client,
            failed: FailedLogos::new(),
            prefetching: Mutex::new((false, false)),
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
            Some(thumb) => Some(thumb.png),
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

    /// Ensure a logo is cached, downloading if necessary
    ///
    /// Returns:
    /// - `Ok(true)` if the logo was downloaded (or converted) and cached,
    ///   here or by another fetch this one waited for
    /// - `Ok(false)` if the logo was already cached
    /// - `Err` if there's no logo URL or the download failed
    pub fn ensure_cached<T: HasLogo>(&self, item: &T) -> Result<bool> {
        let key = item.logo_cache_key();
        if self.cache.state(&key) == CacheState::Ready {
            return Ok(false);
        }
        let _claim = self.claim(&key);
        match self.cache.state(&key) {
            // Another fetch brought it meanwhile
            CacheState::Ready => return Ok(true),
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

        Ok(true)
    }

    /// `url` gave data that isn't an image we can show (a web page, an
    /// SVG): nothing is cached, and it isn't fetched again this session
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
    pub fn is_missing<T: HasLogo>(&self, item: &T) -> bool {
        match self.cache.state(&item.logo_cache_key()) {
            CacheState::Ready => false,
            CacheState::NeedsConversion => true,
            CacheState::Absent => item
                .logo_url()
                .is_some_and(|url| !url.is_empty() && !self.failed.has_failed(url)),
        }
    }

    /// [`prefetch`](Self::prefetch) `items` on a thread of its own, then
    /// call `done` if any logo arrived. While one runs, another call only
    /// notes that it was asked for: the running one then calls its `done`
    /// as well, so the caller looks again, instead of a second thread
    /// fetching the same logos.
    pub fn prefetch_in_background<T, F>(self: &Arc<Self>, items: Vec<T>, done: F)
    where
        T: HasLogo + Send + 'static,
        F: FnOnce() + Send + 'static,
    {
        {
            let mut state = self.prefetching.lock().unwrap_or_else(|e| e.into_inner());
            if state.0 {
                state.1 = true;
                return;
            }
            *state = (true, false);
        }
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("logo-prefetch".into())
            .spawn(move || {
                let fetched = this.prefetch(&items);
                let again = {
                    let mut state = this.prefetching.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::take(&mut *state).1
                };
                if fetched > 0 || again {
                    done();
                }
            });
        if spawned.is_err() {
            *self.prefetching.lock().unwrap_or_else(|e| e.into_inner()) = (false, false);
        }
    }

    /// Prefetch logos for multiple items
    ///
    /// Downloads and caches logos that aren't already cached, and converts
    /// the ones an older build cached. Returns the number of logos
    /// successfully fetched or converted.
    ///
    /// This is a blocking operation - for background prefetching,
    /// call this from a separate thread.
    pub fn prefetch<T: HasLogo>(&self, items: &[T]) -> usize {
        let mut fetched = 0;

        for item in items {
            // Skip if no URL, already cached, or failed lately
            if !self.is_missing(item) {
                continue;
            }

            // Try to fetch and cache
            if self.ensure_cached(item).is_ok() {
                fetched += 1;
            }
        }

        fetched
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
        let img = decode_logo(data)
            .map_err(|e| AppError::Image(format!("Failed to decode image: {}", e)))?;

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

    #[test]
    fn a_prefetch_asked_for_while_one_runs_is_folded_into_it() {
        use crate::network::test_http::serve;
        let service = Arc::new(LogoService::with_cache(temp_cache()).unwrap());
        let slow = serve(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
            Duration::from_millis(500),
        );
        let station = Station::new("Slow", "http://slow.test/stream")
            .with_logo(format!("{slow}/logo.png").as_str());
        let (tx, rx) = std::sync::mpsc::channel();

        let first = tx.clone();
        service.prefetch_in_background(vec![station.clone()], move || {
            let _ = first.send("first");
        });
        let second = tx.clone();
        service.prefetch_in_background(vec![station.clone()], move || {
            let _ = second.send("second");
        });
        // Nothing arrived, but another look was asked for meanwhile
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok("first"));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());

        // The refused logo isn't fetched again, and nothing calls back
        service.prefetch_in_background(vec![station], move || {
            let _ = tx.send("third");
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
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
}
