//! Logo fetching and caching service
//!
//! Provides a unified interface for retrieving station logos,
//! handling both cache lookups and network fetching.

use crate::data::cache::ImageCache;
use crate::data::types::HasLogo;
use radiotrope::config::network::{CONNECT_TIMEOUT_SECS, READ_TIMEOUT_SECS, USER_AGENT};
use crate::error::{AppError, Result};
use std::path::PathBuf;
use std::time::Duration;

/// Service for fetching and caching station logos
///
/// Combines the image cache with HTTP fetching to provide a simple
/// interface for getting logos - checking cache first, then fetching
/// from network if needed.
pub struct LogoService {
    cache: ImageCache,
    client: reqwest::blocking::Client,
}

impl LogoService {
    /// Create a new logo service with default settings
    pub fn new() -> Result<Self> {
        let cache = ImageCache::new()?;
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(READ_TIMEOUT_SECS))
            .build()
            .map_err(AppError::from)?;

        Ok(Self { cache, client })
    }

    /// Create a logo service with a custom cache directory (for testing)
    pub fn with_cache(cache: ImageCache) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(READ_TIMEOUT_SECS))
            .build()
            .map_err(AppError::from)?;

        Ok(Self { cache, client })
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
        // Check cache first
        if let Some(data) = self.cache.get_logo(item) {
            return Some(data);
        }

        // Not cached - try to fetch
        let url = item.logo_url()?;
        let data = self.fetch_raw(url).ok()?;

        // Cache it (ignore errors - we still have the data)
        let _ = self.cache.put_logo(item, &data);

        Some(data)
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
    /// - `Ok(true)` if the logo was downloaded and cached
    /// - `Ok(false)` if the logo was already cached
    /// - `Err` if there's no logo URL or the download failed
    pub fn ensure_cached<T: HasLogo>(&self, item: &T) -> Result<bool> {
        // Already cached?
        if self.cache.has_logo(item) {
            return Ok(false);
        }

        // Get URL
        let url = item.logo_url().ok_or_else(|| {
            AppError::NotFound("Item has no logo URL".to_string())
        })?;

        // Fetch and cache
        let data = self.fetch_raw(url)?;
        self.cache.put_logo(item, &data)?;

        Ok(true)
    }

    /// Prefetch logos for multiple items
    ///
    /// Downloads and caches logos that aren't already cached.
    /// Returns the number of logos successfully fetched.
    ///
    /// This is a blocking operation - for background prefetching,
    /// call this from a separate thread.
    pub fn prefetch<T: HasLogo>(&self, items: &[T]) -> usize {
        let mut fetched = 0;

        for item in items {
            // Skip if no URL or already cached
            if item.logo_url().is_none() || self.cache.has_logo(item) {
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
    /// Useful for previews or one-time downloads.
    pub fn fetch_raw(&self, url: &str) -> Result<Vec<u8>> {
        if url.is_empty() {
            return Err(AppError::NotFound("Empty URL".to_string()));
        }

        let response = self.client.get(url).send()?;

        if !response.status().is_success() {
            return Err(response.error_for_status().unwrap_err().into());
        }

        let bytes = response.bytes()?;
        Ok(bytes.to_vec())
    }

    /// Fetch image and decode to RGBA pixels
    ///
    /// Returns (rgba_bytes, width, height) for use with UI frameworks.
    pub fn fetch_rgba(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        let data = self.fetch_raw(url)?;
        self.decode_to_rgba(&data)
    }

    /// Decode image bytes to RGBA pixels
    pub fn decode_to_rgba(&self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
        let img = image::load_from_memory(data)
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
        self.decode_to_rgba(&data).ok()
    }

    /// Get cached logo as RGBA pixels (no network request)
    pub fn get_cached_rgba<T: HasLogo>(&self, item: &T) -> Option<(Vec<u8>, u32, u32)> {
        let data = self.get_cached(item)?;
        self.decode_to_rgba(&data).ok()
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

        let station = Station::new("Test", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

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

        let station = Station::new("Test", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

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

        let station = Station::new("Test", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

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
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(1, 1, Rgba([255, 0, 0, 255]));

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

        let station = Station::new("Test", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

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

        let station = Station::new("Test", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

        // Not cached
        assert!(service.get_cached_rgba(&station).is_none());

        // Cache a valid PNG
        use image::{ImageBuffer, Rgba};
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(2, 2, Rgba([0, 255, 0, 255]));
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

        let station = Station::new("Test", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

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
