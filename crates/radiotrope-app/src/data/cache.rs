//! Image cache for station logos
//!
//! Caches station logos locally using station ID as filename.
//! Uses the system cache directory for proper cache semantics.

use crate::config::app::NAME;
use crate::data::types::HasLogo;
use crate::error::{AppError, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Supported image extensions (in order of preference for lookup)
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "ico"];

/// Get the application cache directory path
///
/// Uses the system cache directory:
/// - Linux: `~/.cache/radiotrope/`
/// - macOS: `~/Library/Caches/radiotrope/`
/// - Windows: `C:\Users\<User>\AppData\Local\radiotrope\cache\`
pub fn cache_dir() -> Result<PathBuf> {
    dirs::cache_dir()
        .map(|p| p.join(NAME))
        .ok_or_else(|| {
            AppError::Config(
                "Could not determine cache directory. HOME environment variable may not be set."
                    .to_string(),
            )
        })
}

/// Ensure the cache directory exists
pub fn ensure_cache_dir() -> Result<PathBuf> {
    let dir = cache_dir()?;
    fs::create_dir_all(&dir).map_err(|e| {
        AppError::Config(format!("Failed to create cache directory {:?}: {}", dir, e))
    })?;
    Ok(dir)
}

/// Image cache manager for station logos
pub struct ImageCache {
    cache_dir: PathBuf,
}

impl ImageCache {
    /// Create a new image cache using the default cache directory
    pub fn new() -> Result<Self> {
        let cache_dir = ensure_cache_dir()?;
        Ok(Self { cache_dir })
    }

    /// Create a new image cache with a custom directory (for testing)
    pub fn with_dir(cache_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&cache_dir).map_err(|e| {
            AppError::Config(format!("Failed to create cache directory {:?}: {}", cache_dir, e))
        })?;
        Ok(Self { cache_dir })
    }

    /// Get the cache directory path
    pub fn dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Check if a cached image exists for the given ID
    pub fn has(&self, id: &str) -> bool {
        self.find_cached_path(id).is_some()
    }

    /// Get the path to a cached image (if it exists)
    ///
    /// Searches for the image with any supported extension.
    pub fn get_path(&self, id: &str) -> Option<PathBuf> {
        self.find_cached_path(id)
    }

    /// Load cached image data
    pub fn get(&self, id: &str) -> Option<Vec<u8>> {
        let path = self.find_cached_path(id)?;
        fs::read(&path).ok()
    }

    /// Save image data to cache
    ///
    /// The extension is determined from the URL or content type.
    /// Falls back to "png" if extension cannot be determined.
    pub fn put(&self, id: &str, data: &[u8], url_or_hint: Option<&str>) -> Result<PathBuf> {
        let extension = self.determine_extension(data, url_or_hint);
        let path = self.cache_dir.join(format!("{}.{}", id, extension));

        // Remove any existing cached image with different extension
        self.delete(id);

        fs::write(&path, data).map_err(|e| {
            AppError::Config(format!("Failed to write cached image {:?}: {}", path, e))
        })?;

        Ok(path)
    }

    /// Delete cached image for the given ID
    ///
    /// Removes any file matching the ID regardless of extension.
    pub fn delete(&self, id: &str) {
        if let Some(path) = self.find_cached_path(id) {
            let _ = fs::remove_file(path);
        }
    }

    // =========================================================================
    // Generic methods for HasLogo types (Station, Favorite, etc.)
    // =========================================================================

    /// Check if a cached logo exists for the given item
    pub fn has_logo<T: HasLogo>(&self, item: &T) -> bool {
        self.has(&item.logo_cache_key())
    }

    /// Get the path to a cached logo (if it exists)
    pub fn get_logo_path<T: HasLogo>(&self, item: &T) -> Option<PathBuf> {
        self.get_path(&item.logo_cache_key())
    }

    /// Load cached logo data for the given item
    pub fn get_logo<T: HasLogo>(&self, item: &T) -> Option<Vec<u8>> {
        self.get(&item.logo_cache_key())
    }

    /// Save logo data to cache for the given item
    ///
    /// Uses the item's logo_url as a hint for determining the file extension.
    pub fn put_logo<T: HasLogo>(&self, item: &T, data: &[u8]) -> Result<PathBuf> {
        self.put(&item.logo_cache_key(), data, item.logo_url())
    }

    /// Delete cached logo for the given item
    pub fn delete_logo<T: HasLogo>(&self, item: &T) {
        self.delete(&item.logo_cache_key())
    }

    // =========================================================================
    // Maintenance operations
    // =========================================================================

    /// Clean up orphaned cached images
    ///
    /// Removes cached images that don't belong to any of the provided valid IDs.
    /// Returns the number of files removed.
    pub fn cleanup_orphaned(&self, valid_ids: &HashSet<String>) -> usize {
        let entries = match fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };

        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();

            // Only process image files
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
                    // Extract ID from filename (filename without extension)
                    if let Some(filename) = path.file_stem().and_then(|s| s.to_str()) {
                        if !valid_ids.contains(filename) && fs::remove_file(&path).is_ok() {
                            removed += 1;
                        }
                    }
                }
            }
        }

        removed
    }

    /// Get all cached image IDs
    pub fn list_ids(&self) -> Vec<String> {
        let entries = match fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let ext = path.extension()?.to_str()?;
                if IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
                    path.file_stem()?.to_str().map(String::from)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get total cache size in bytes
    pub fn total_size(&self) -> u64 {
        let entries = match fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };

        entries
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|m| m.len())
            .sum()
    }

    /// Clear all cached images
    pub fn clear(&self) -> Result<usize> {
        let entries = fs::read_dir(&self.cache_dir).map_err(|e| {
            AppError::Config(format!("Failed to read cache directory: {}", e))
        })?;

        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }

        Ok(removed)
    }

    /// Find the cached file path for an ID (checking all extensions)
    fn find_cached_path(&self, id: &str) -> Option<PathBuf> {
        for ext in IMAGE_EXTENSIONS {
            let path = self.cache_dir.join(format!("{}.{}", id, ext));
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    /// Determine the best extension for the image data
    fn determine_extension(&self, data: &[u8], url_or_hint: Option<&str>) -> &'static str {
        // First, try to detect from magic bytes
        if let Some(ext) = self.detect_format_from_magic(data) {
            return ext;
        }

        // Then, try to extract from URL
        if let Some(url) = url_or_hint {
            if let Some(ext) = self.extract_extension_from_url(url) {
                return ext;
            }
        }

        // Default to PNG
        "png"
    }

    /// Detect image format from magic bytes
    fn detect_format_from_magic(&self, data: &[u8]) -> Option<&'static str> {
        if data.len() < 8 {
            return None;
        }

        // PNG: 89 50 4E 47 0D 0A 1A 0A
        if data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
            return Some("png");
        }

        // JPEG: FF D8 FF
        if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some("jpg");
        }

        // GIF: GIF87a or GIF89a
        if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
            return Some("gif");
        }

        // WebP: RIFF....WEBP
        if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
            return Some("webp");
        }

        // ICO: 00 00 01 00
        if data.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
            return Some("ico");
        }

        // SVG: Check for XML/SVG markers
        if data.starts_with(b"<?xml") || data.starts_with(b"<svg") {
            return Some("svg");
        }

        None
    }

    /// Extract extension from URL
    fn extract_extension_from_url(&self, url: &str) -> Option<&'static str> {
        // Remove query string and fragment
        let path = url.split('?').next()?.split('#').next()?;

        // Get the last path component
        let filename = path.rsplit('/').next()?;

        // Get extension
        let ext = filename.rsplit('.').next()?.to_lowercase();

        // Map to supported extension
        match ext.as_str() {
            "png" => Some("png"),
            "jpg" | "jpeg" => Some("jpg"),
            "gif" => Some("gif"),
            "webp" => Some("webp"),
            "svg" => Some("svg"),
            "ico" => Some("ico"),
            _ => None,
        }
    }
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new().expect("Failed to create default image cache")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_cache_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        temp_dir().join(format!("radiotrope_cache_test_{}", id))
    }

    fn cleanup_dir(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_cache_creation() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();
        assert!(cache.dir().exists());
        cleanup_dir(&dir);
    }

    #[test]
    fn test_put_and_get() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let id = "test_station_123";

        // Put
        let path = cache.put(id, &data, None).unwrap();
        assert!(path.exists());
        assert!(path.to_string_lossy().ends_with(".png"));

        // Has
        assert!(cache.has(id));

        // Get
        let loaded = cache.get(id).unwrap();
        assert_eq!(loaded, data);

        // Get path
        let found_path = cache.get_path(id).unwrap();
        assert_eq!(found_path, path);

        cleanup_dir(&dir);
    }

    #[test]
    fn test_delete() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = vec![1, 2, 3, 4];
        let id = "to_delete";

        cache.put(id, &data, Some("test.png")).unwrap();
        assert!(cache.has(id));

        cache.delete(id);
        assert!(!cache.has(id));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_format_detection_png() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // PNG magic bytes
        let png_data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let path = cache.put("png_test", &png_data, None).unwrap();
        assert!(path.to_string_lossy().ends_with(".png"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_format_detection_jpg() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // JPEG magic bytes
        let jpg_data = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
        let path = cache.put("jpg_test", &jpg_data, None).unwrap();
        assert!(path.to_string_lossy().ends_with(".jpg"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_format_detection_gif() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // GIF magic bytes
        let gif_data = b"GIF89a\x00\x00".to_vec();
        let path = cache.put("gif_test", &gif_data, None).unwrap();
        assert!(path.to_string_lossy().ends_with(".gif"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_format_detection_webp() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // WebP magic bytes
        let webp_data = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        let path = cache.put("webp_test", &webp_data, None).unwrap();
        assert!(path.to_string_lossy().ends_with(".webp"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_format_from_url() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // Unknown magic bytes but URL has extension
        let data = vec![0, 0, 0, 0, 0, 0, 0, 0];
        let path = cache
            .put("url_test", &data, Some("https://example.com/logo.webp"))
            .unwrap();
        assert!(path.to_string_lossy().ends_with(".webp"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_cleanup_orphaned() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = vec![1, 2, 3, 4];

        // Add some cached images
        cache.put("keep1", &data, Some("a.png")).unwrap();
        cache.put("keep2", &data, Some("b.png")).unwrap();
        cache.put("orphan1", &data, Some("c.png")).unwrap();
        cache.put("orphan2", &data, Some("d.png")).unwrap();

        // Only keep1 and keep2 are valid
        let valid_ids: HashSet<String> =
            ["keep1".to_string(), "keep2".to_string()].into_iter().collect();

        let removed = cache.cleanup_orphaned(&valid_ids);
        assert_eq!(removed, 2);

        assert!(cache.has("keep1"));
        assert!(cache.has("keep2"));
        assert!(!cache.has("orphan1"));
        assert!(!cache.has("orphan2"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_list_ids() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = vec![1, 2, 3, 4];
        cache.put("station_a", &data, Some("a.png")).unwrap();
        cache.put("station_b", &data, Some("b.jpg")).unwrap();

        let ids = cache.list_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"station_a".to_string()));
        assert!(ids.contains(&"station_b".to_string()));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_total_size() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data1 = vec![1; 100];
        let data2 = vec![2; 200];

        cache.put("size_test_1", &data1, Some("a.png")).unwrap();
        cache.put("size_test_2", &data2, Some("b.png")).unwrap();

        let size = cache.total_size();
        assert_eq!(size, 300);

        cleanup_dir(&dir);
    }

    #[test]
    fn test_clear() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = vec![1, 2, 3, 4];
        cache.put("clear_test_1", &data, Some("a.png")).unwrap();
        cache.put("clear_test_2", &data, Some("b.png")).unwrap();

        let removed = cache.clear().unwrap();
        assert_eq!(removed, 2);
        assert!(!cache.has("clear_test_1"));
        assert!(!cache.has("clear_test_2"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_overwrite_different_extension() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // First save as PNG
        let png_data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let png_path = cache.put("overwrite_test", &png_data, None).unwrap();
        assert!(png_path.exists());
        assert!(png_path.to_string_lossy().ends_with(".png"));

        // Then save as JPEG (should delete the PNG)
        let jpg_data = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
        let jpg_path = cache.put("overwrite_test", &jpg_data, None).unwrap();
        assert!(jpg_path.exists());
        assert!(jpg_path.to_string_lossy().ends_with(".jpg"));

        // PNG should be gone
        assert!(!png_path.exists());

        // Only one file should exist
        assert_eq!(cache.list_ids().len(), 1);

        cleanup_dir(&dir);
    }

    // =========================================================================
    // Format detection tests for remaining formats
    // =========================================================================

    #[test]
    fn test_format_detection_svg_xml() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let svg_data = b"<?xml version=\"1.0\"?><svg></svg>".to_vec();
        let path = cache.put("svg_test", &svg_data, None).unwrap();
        assert!(path.to_string_lossy().ends_with(".svg"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_format_detection_svg_direct() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let svg_data = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
        let path = cache.put("svg_direct_test", &svg_data, None).unwrap();
        assert!(path.to_string_lossy().ends_with(".svg"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_format_detection_ico() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // ICO magic bytes
        let ico_data = vec![0x00, 0x00, 0x01, 0x00, 0, 0, 0, 0];
        let path = cache.put("ico_test", &ico_data, None).unwrap();
        assert!(path.to_string_lossy().ends_with(".ico"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_format_unknown_defaults_to_png() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // Unknown magic bytes, no URL hint
        let unknown_data = vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
        let path = cache.put("unknown_test", &unknown_data, None).unwrap();
        assert!(path.to_string_lossy().ends_with(".png"));

        cleanup_dir(&dir);
    }

    // =========================================================================
    // HasLogo generic method tests
    // =========================================================================

    #[test]
    fn test_has_logo_with_station() {
        use crate::data::types::Station;

        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let station = Station::new("Test Radio", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

        // Initially not cached
        assert!(!cache.has_logo(&station));

        // Add to cache
        let data = vec![1, 2, 3, 4];
        cache.put_logo(&station, &data).unwrap();

        // Now cached
        assert!(cache.has_logo(&station));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_get_logo_with_station() {
        use crate::data::types::Station;

        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let station = Station::new("Test Radio", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

        let data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        cache.put_logo(&station, &data).unwrap();

        let retrieved = cache.get_logo(&station).unwrap();
        assert_eq!(retrieved, data);

        cleanup_dir(&dir);
    }

    #[test]
    fn test_delete_logo_with_station() {
        use crate::data::types::Station;

        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let station = Station::new("Test Radio", "http://test.com/stream");

        let data = vec![1, 2, 3, 4];
        cache.put_logo(&station, &data).unwrap();
        assert!(cache.has_logo(&station));

        cache.delete_logo(&station);
        assert!(!cache.has_logo(&station));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_has_logo_with_favorite() {
        use crate::data::types::Favorite;

        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let favorite = Favorite::new("Test Radio", "http://test.com/stream")
            .with_logo("http://test.com/logo.png");

        assert!(!cache.has_logo(&favorite));

        let data = vec![1, 2, 3, 4];
        cache.put_logo(&favorite, &data).unwrap();

        assert!(cache.has_logo(&favorite));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_station_and_favorite_share_cache() {
        use crate::data::types::{Favorite, Station};

        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // Same URL = same cache key
        let station = Station::new("Test Radio", "http://test.com/stream");
        let favorite = Favorite::new("Test Radio", "http://test.com/stream");

        // Cache for station
        let data = vec![1, 2, 3, 4];
        cache.put_logo(&station, &data).unwrap();

        // Should be available for favorite too (same URL = same ID)
        assert!(cache.has_logo(&favorite));
        assert_eq!(cache.get_logo(&favorite).unwrap(), data);

        cleanup_dir(&dir);
    }

    #[test]
    fn test_get_logo_path_with_station() {
        use crate::data::types::Station;

        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let station = Station::new("Test Radio", "http://test.com/stream")
            .with_logo("http://test.com/logo.jpg");

        // Not cached yet
        assert!(cache.get_logo_path(&station).is_none());

        // Cache it
        let jpg_data = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
        cache.put_logo(&station, &jpg_data).unwrap();

        // Should have path now
        let path = cache.get_logo_path(&station).unwrap();
        assert!(path.exists());
        assert!(path.to_string_lossy().ends_with(".jpg"));

        cleanup_dir(&dir);
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn test_empty_cache_operations() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // Operations on empty cache should work
        assert!(!cache.has("nonexistent"));
        assert!(cache.get("nonexistent").is_none());
        assert!(cache.get_path("nonexistent").is_none());
        assert_eq!(cache.list_ids().len(), 0);
        assert_eq!(cache.total_size(), 0);

        // Delete on nonexistent should not panic
        cache.delete("nonexistent");

        cleanup_dir(&dir);
    }

    #[test]
    fn test_url_with_query_string() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = vec![0, 0, 0, 0, 0, 0, 0, 0];
        let path = cache
            .put("query_test", &data, Some("http://example.com/logo.webp?size=large&v=2"))
            .unwrap();
        assert!(path.to_string_lossy().ends_with(".webp"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_url_with_fragment() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = vec![0, 0, 0, 0, 0, 0, 0, 0];
        let path = cache
            .put("fragment_test", &data, Some("http://example.com/logo.gif#section"))
            .unwrap();
        assert!(path.to_string_lossy().ends_with(".gif"));

        cleanup_dir(&dir);
    }
}
