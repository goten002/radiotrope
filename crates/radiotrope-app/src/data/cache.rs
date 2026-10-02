//! Image cache for station logos
//!
//! Caches station logos locally using station ID as filename.
//! Uses the system cache directory for proper cache semantics.
//!
//! Decodable logos are stored as PNG thumbnails no larger than
//! [`LOGO_MAX_SIZE`], so loading one never decodes a full-size image and the
//! lookup hits `<id>.png` first. Data the `image` crate cannot decode (e.g.
//! SVG) is stored as-is with an extension guessed from its content.

use crate::config::app::NAME;
use crate::config::caches::TEMP_FILE_MAX_AGE;
use crate::config::logos::{MAX_DECODE_BYTES, MAX_DIMENSION};
use crate::data::types::HasLogo;
use crate::error::{AppError, Result};
use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};

/// Supported image extensions (in order of preference for lookup)
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "ico"];

/// Largest width or height of a cached logo, in pixels.
///
/// Logos are displayed at 70px at most; 160px keeps them sharp at 2x scale.
pub const LOGO_MAX_SIZE: u32 = 160;

/// Get the application cache directory path
///
/// Uses the system cache directory:
/// - Linux: `~/.cache/radiotrope/`
/// - macOS: `~/Library/Caches/radiotrope/`
/// - Windows: `C:\Users\<User>\AppData\Local\radiotrope\cache\`
pub fn cache_dir() -> Result<PathBuf> {
    dirs::cache_dir().map(|p| p.join(NAME)).ok_or_else(|| {
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
    /// Create a new image cache using the default cache directory (`logos/` subfolder)
    pub fn new() -> Result<Self> {
        let cache_dir = ensure_cache_dir()?.join("logos");
        fs::create_dir_all(&cache_dir).map_err(|e| {
            AppError::Config(format!(
                "Failed to create logos cache directory {:?}: {}",
                cache_dir, e
            ))
        })?;
        Ok(Self { cache_dir })
    }

    /// The cache in the `name` folder under the app's cache folder. When
    /// that folder can't be created (no home folder, a read-only or full
    /// disk, a file in the way), one in the temp folder is used instead.
    /// Failing that too, the cache keeps nothing (lookups find nothing,
    /// saves fail) and the app runs without it.
    pub fn open(name: &str) -> Self {
        let preferred = cache_dir().map(|dir| dir.join(name));
        let problem = match &preferred {
            Ok(dir) => match fs::create_dir_all(dir) {
                Ok(()) => {
                    return Self {
                        cache_dir: dir.clone(),
                    }
                }
                Err(e) => format!("Failed to create {}: {e}", dir.display()),
            },
            Err(e) => e.to_string(),
        };
        let fallback = fallback_cache_dir().join(name);
        if let Err(e) = fallback_dir_ready(&fallback) {
            eprintln!("{problem}, and {e}; images won't be kept on disk");
            // A folder that doesn't exist, and that nobody else can have
            // made in its place
            let missing = preferred.unwrap_or_else(|_| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                std::env::temp_dir().join(format!("{NAME}-{}-{nanos}", std::process::id()))
            });
            return Self { cache_dir: missing };
        }
        eprintln!("{problem}; caching in {}", fallback.display());
        Self {
            cache_dir: fallback,
        }
    }

    /// Create a new image cache with a custom directory (for testing)
    pub fn with_dir(cache_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&cache_dir).map_err(|e| {
            AppError::Config(format!(
                "Failed to create cache directory {:?}: {}",
                cache_dir, e
            ))
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
    ///
    /// Entries cached before logos were stored as thumbnails (any non-PNG
    /// format, or larger than [`LOGO_MAX_SIZE`]) are converted on first read.
    pub fn get(&self, id: &str) -> Option<Vec<u8>> {
        let path = self.find_cached_path(id)?;
        let data = fs::read(&path).ok()?;
        if needs_thumbnail(&data) {
            if let Some(thumb) = make_thumbnail(&data) {
                let _ = self.write_png(id, &thumb);
                return Some(thumb);
            }
        }
        Some(data)
    }

    /// Save image data to cache
    ///
    /// Decodable images are stored as a PNG thumbnail (see [`LOGO_MAX_SIZE`]).
    /// Anything else is stored as-is, with the extension determined from the
    /// content or URL, falling back to "png".
    pub fn put(&self, id: &str, data: &[u8], url_or_hint: Option<&str>) -> Result<PathBuf> {
        if let Some(thumb) = make_thumbnail(data) {
            return self.write_png(id, &thumb);
        }

        let extension = self.determine_extension(data, url_or_hint);
        let path = self.cache_dir.join(format!("{}.{}", id, extension));

        // Remove any existing cached image with different extension
        self.delete(id);

        fs::write(&path, data).map_err(|e| {
            AppError::Config(format!("Failed to write cached image {:?}: {}", path, e))
        })?;

        Ok(path)
    }

    /// Write a PNG thumbnail as `<id>.png`, replacing any other cached file
    fn write_png(&self, id: &str, png: &[u8]) -> Result<PathBuf> {
        let path = self.cache_dir.join(format!("{}.png", id));
        // Unique temp name so concurrent writers of the same ID don't collide
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .cache_dir
            .join(format!("{}.{}.{}.tmp", id, std::process::id(), n));

        fs::write(&tmp, png).map_err(|e| {
            AppError::Config(format!("Failed to write cached image {:?}: {}", tmp, e))
        })?;

        // Remove files with other extensions left by earlier versions
        for ext in IMAGE_EXTENSIONS.iter().filter(|e| **e != "png") {
            let _ = fs::remove_file(self.cache_dir.join(format!("{}.{}", id, ext)));
        }

        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
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
    /// Returns the number of files removed. Temp files a write left behind
    /// are removed too (not counted).
    pub fn cleanup_orphaned(&self, valid_ids: &HashSet<String>) -> usize {
        let entries = match fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };

        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if remove_if_stale_temp(&entry) {
                continue;
            }

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

    /// Rename the cached image `<from>.<ext>` to `<to>.<ext>`, unless `<to>`
    /// is already cached. Returns whether a file was moved.
    pub fn rename_id(&self, from: &str, to: &str) -> bool {
        if from == to || self.has(to) {
            return false;
        }
        let Some(path) = self.find_cached_path(from) else {
            return false;
        };
        let Some(ext) = path.extension() else {
            return false;
        };
        let target = self.cache_dir.join(to).with_extension(ext);
        fs::rename(&path, target).is_ok()
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

    /// Save `data` as a PNG thumbnail (see [`LOGO_MAX_SIZE`]) and return it
    ///
    /// Unlike [`put`](Self::put), data the `image` crate can't decode is not
    /// stored, and `None` is returned.
    pub fn put_thumbnail(&self, id: &str, data: &[u8]) -> Option<Vec<u8>> {
        let png = make_thumbnail(data)?;
        let _ = self.write_png(id, &png);
        Some(png)
    }

    /// Mark a cached image as just used, so [`remove_unused`](Self::remove_unused)
    /// keeps it
    pub fn touch(&self, id: &str) {
        if let Some(path) = self.find_cached_path(id) {
            if let Ok(file) = fs::File::options().write(true).open(path) {
                let _ = file.set_modified(std::time::SystemTime::now());
            }
        }
    }

    /// Delete cached images not written or touched for longer than `age`
    ///
    /// Returns how many were removed. Temp files a write left behind are
    /// removed too (not counted).
    pub fn remove_unused(&self, age: std::time::Duration) -> usize {
        let Ok(entries) = fs::read_dir(&self.cache_dir) else {
            return 0;
        };
        let Some(cutoff) = std::time::SystemTime::now().checked_sub(age) else {
            return 0;
        };
        entries
            .flatten()
            .filter(|entry| {
                if remove_if_stale_temp(entry) {
                    return false;
                }
                let path = entry.path();
                let is_image = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()));
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|modified| modified < cutoff);
                is_image && old && fs::remove_file(&path).is_ok()
            })
            .count()
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
        let entries = fs::read_dir(&self.cache_dir)
            .map_err(|e| AppError::Config(format!("Failed to read cache directory: {}", e)))?;

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

/// Whether cached data is a decodable image not yet stored as a thumbnail
///
/// Only reads the image header, so it is cheap for data that is already fine.
fn needs_thumbnail(data: &[u8]) -> bool {
    let Ok(reader) = ImageReader::new(Cursor::new(data)).with_guessed_format() else {
        return false;
    };
    let is_png = reader.format() == Some(ImageFormat::Png);
    match reader.into_dimensions() {
        Ok((w, h)) => !is_png || w > LOGO_MAX_SIZE || h > LOGO_MAX_SIZE,
        Err(_) => false,
    }
}

/// Whether `entry` is a temp file of [`ImageCache::write_png`]. One older
/// than [`TEMP_FILE_MAX_AGE`] was left by a crash before its rename, and is
/// removed.
fn remove_if_stale_temp(entry: &fs::DirEntry) -> bool {
    let path = entry.path();
    if path.extension().is_none_or(|e| e != "tmp") {
        return false;
    }
    let stale = entry
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|modified| {
            std::time::SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age >= TEMP_FILE_MAX_AGE)
        });
    if stale {
        let _ = fs::remove_file(&path);
    }
    true
}

/// Decode a logo, refusing anything larger than [`MAX_DIMENSION`] a side
/// or [`MAX_DECODE_BYTES`] of memory: a small file can unpack into a huge
/// image, and several are decoded at once.
pub fn decode_logo(data: &[u8]) -> image::ImageResult<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    reader.decode()
}

/// The folder in the temp folder used when the cache folder can't be
/// created: the user's own (the temp folder is shared on Linux)
fn fallback_cache_dir() -> PathBuf {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir().join(format!("{NAME}-cache-{uid}"))
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join(format!("{NAME}-cache"))
    }
}

/// Create `dir` in the fallback folder, which must be the user's own
fn fallback_dir_ready(dir: &Path) -> std::io::Result<()> {
    let base = dir.parent().unwrap_or(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        match fs::DirBuilder::new().mode(0o700).create(base) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        // In a shared temp folder someone else could have made it first
        let meta = fs::symlink_metadata(base)?;
        // SAFETY: getuid has no preconditions and cannot fail
        if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} belongs to another user", base.display()),
            ));
        }
    }
    #[cfg(not(unix))]
    fs::create_dir_all(base)?;
    fs::create_dir_all(dir)
}

/// Decode an image and re-encode it as a PNG no larger than [`LOGO_MAX_SIZE`]
///
/// Returns `None` if the data is not an image the `image` crate can decode.
fn make_thumbnail(data: &[u8]) -> Option<Vec<u8>> {
    let img = decode_logo(data).ok()?;
    let img = if img.width() > LOGO_MAX_SIZE || img.height() > LOGO_MAX_SIZE {
        img.resize(LOGO_MAX_SIZE, LOGO_MAX_SIZE, FilterType::CatmullRom)
    } else {
        img
    };

    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(png)
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

    fn png(size: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(size, size, image::Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn the_fallback_folder_is_made_private_and_refused_when_not_a_folder() {
        let dir = temp_cache_dir();
        fs::create_dir_all(&dir).unwrap();
        let logos = dir.join("fallback").join("logos");
        fallback_dir_ready(&logos).unwrap();
        assert!(logos.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("fallback"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // A file where the folder should be
        fs::write(dir.join("file"), b"x").unwrap();
        assert!(fallback_dir_ready(&dir.join("file").join("logos")).is_err());
        cleanup_dir(&dir);
    }

    #[test]
    fn test_put_thumbnail_shrinks_and_skips_non_images() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let thumb = cache.put_thumbnail("big", &png(400)).unwrap();
        let img = image::load_from_memory(&thumb).unwrap();
        assert_eq!((img.width(), img.height()), (LOGO_MAX_SIZE, LOGO_MAX_SIZE));
        assert_eq!(cache.get("big").unwrap(), thumb);

        assert!(cache
            .put_thumbnail("html", b"<html>not found</html>")
            .is_none());
        assert!(!cache.has("html"));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_remove_unused_keeps_recent_and_touched() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(200 * 86400);
        for id in ["old", "touched", "new"] {
            cache.put_thumbnail(id, &png(8)).unwrap();
        }
        for id in ["old", "touched"] {
            fs::File::options()
                .write(true)
                .open(dir.join(format!("{id}.png")))
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        cache.touch("touched");

        assert_eq!(
            cache.remove_unused(std::time::Duration::from_secs(90 * 86400)),
            1
        );
        assert!(!cache.has("old"));
        assert!(cache.has("touched") && cache.has("new"));

        cleanup_dir(&dir);
    }

    #[test]
    fn stale_temp_files_are_removed_by_both_cleanups() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();
        let long_ago = std::time::SystemTime::now() - TEMP_FILE_MAX_AGE * 2;
        let make = |name: &str, old: bool| {
            let path = dir.join(name);
            fs::write(&path, b"x").unwrap();
            if old {
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(long_ago)
                    .unwrap();
            }
            path
        };

        let (old, new) = (make("a.1.0.tmp", true), make("a.1.1.tmp", false));
        assert_eq!(cache.cleanup_orphaned(&HashSet::new()), 0);
        assert!(!old.exists());
        // A write still in progress
        assert!(new.exists());

        let old = make("b.1.0.tmp", true);
        assert_eq!(cache.remove_unused(std::time::Duration::from_secs(3600)), 0);
        assert!(!old.exists() && new.exists());

        cleanup_dir(&dir);
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
    fn rename_id_moves_a_logo_and_keeps_its_extension() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();
        cache.put("old", b"<svg/>", Some("logo.svg")).unwrap();

        assert!(cache.rename_id("old", "new"));
        assert!(!cache.has("old"));
        assert_eq!(cache.get_path("new").unwrap(), dir.join("new.svg"));
        // Nothing left to move
        assert!(!cache.rename_id("old", "new"));

        cleanup_dir(&dir);
    }

    #[test]
    fn rename_id_never_replaces_a_logo_already_there() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();
        cache.put_thumbnail("old", &png(8)).unwrap();
        let current = cache.put_thumbnail("new", &png(16)).unwrap();

        assert!(!cache.rename_id("old", "new"));
        assert_eq!(cache.get("new").unwrap(), current);

        cleanup_dir(&dir);
    }

    #[test]
    fn logos_under_the_legacy_id_survive_the_startup_cleanup() {
        use crate::data::types::{legacy_url_to_id, url_to_id};
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();
        let url = "http://stream.example/live.mp3";
        // What an older build left in the cache
        cache
            .put_thumbnail(&legacy_url_to_id(url), &png(8))
            .unwrap();

        // Startup: rename, then clean up
        assert!(cache.rename_id(&legacy_url_to_id(url), &url_to_id(url)));
        let valid: HashSet<String> = [url_to_id(url)].into_iter().collect();
        assert_eq!(cache.cleanup_orphaned(&valid), 0);
        assert!(cache.has(&url_to_id(url)));

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
        let valid_ids: HashSet<String> = ["keep1".to_string(), "keep2".to_string()]
            .into_iter()
            .collect();

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
            .put(
                "query_test",
                &data,
                Some("http://example.com/logo.webp?size=large&v=2"),
            )
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
            .put(
                "fragment_test",
                &data,
                Some("http://example.com/logo.gif#section"),
            )
            .unwrap();
        assert!(path.to_string_lossy().ends_with(".gif"));

        cleanup_dir(&dir);
    }

    // =========================================================================
    // Thumbnail tests
    // =========================================================================

    fn encode_image(size: u32, format: ImageFormat) -> Vec<u8> {
        let img = image::RgbImage::from_fn(size, size, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut data = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut data), format)
            .unwrap();
        data
    }

    fn dimensions(data: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory_with_format(data, ImageFormat::Png).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn test_put_large_image_stores_png_thumbnail() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = encode_image(600, ImageFormat::Jpeg);
        let path = cache.put("thumb_large", &data, Some("logo.jpg")).unwrap();

        assert_eq!(path, dir.join("thumb_large.png"));
        let stored = cache.get("thumb_large").unwrap();
        assert_eq!(dimensions(&stored), (LOGO_MAX_SIZE, LOGO_MAX_SIZE));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_put_keeps_aspect_ratio() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let img = image::RgbImage::new(800, 200);
        let mut data = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut data), ImageFormat::Png)
            .unwrap();
        cache.put("thumb_wide", &data, None).unwrap();

        let stored = cache.get("thumb_wide").unwrap();
        assert_eq!(dimensions(&stored), (LOGO_MAX_SIZE, LOGO_MAX_SIZE / 4));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_put_small_image_keeps_size() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let data = encode_image(64, ImageFormat::Gif);
        let path = cache.put("thumb_small", &data, None).unwrap();

        assert!(path.to_string_lossy().ends_with(".png"));
        assert_eq!(dimensions(&cache.get("thumb_small").unwrap()), (64, 64));

        cleanup_dir(&dir);
    }

    #[test]
    fn test_put_replaces_legacy_extension() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        fs::write(dir.join("thumb_replace.webp"), b"old").unwrap();
        cache
            .put("thumb_replace", &encode_image(32, ImageFormat::Png), None)
            .unwrap();

        assert!(!dir.join("thumb_replace.webp").exists());
        assert!(dir.join("thumb_replace.png").exists());
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .count();
        assert_eq!(leftovers, 0);

        cleanup_dir(&dir);
    }

    #[test]
    fn test_get_converts_legacy_entry() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        // Written directly, as an older version of the cache would have
        fs::write(
            dir.join("thumb_legacy.jpg"),
            encode_image(500, ImageFormat::Jpeg),
        )
        .unwrap();

        let data = cache.get("thumb_legacy").unwrap();
        assert_eq!(dimensions(&data), (LOGO_MAX_SIZE, LOGO_MAX_SIZE));
        assert!(!dir.join("thumb_legacy.jpg").exists());
        assert_eq!(fs::read(dir.join("thumb_legacy.png")).unwrap(), data);

        cleanup_dir(&dir);
    }

    #[test]
    fn test_get_converts_legacy_large_png() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        fs::write(
            dir.join("thumb_big_png.png"),
            encode_image(400, ImageFormat::Png),
        )
        .unwrap();

        let data = cache.get("thumb_big_png").unwrap();
        assert_eq!(dimensions(&data), (LOGO_MAX_SIZE, LOGO_MAX_SIZE));
        assert_eq!(fs::read(dir.join("thumb_big_png.png")).unwrap(), data);

        cleanup_dir(&dir);
    }

    #[test]
    fn test_get_leaves_small_png_untouched() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let original = encode_image(100, ImageFormat::Png);
        fs::write(dir.join("thumb_ok.png"), &original).unwrap();

        assert_eq!(cache.get("thumb_ok").unwrap(), original);

        cleanup_dir(&dir);
    }

    #[test]
    fn test_undecodable_svg_stored_as_is() {
        let dir = temp_cache_dir();
        let cache = ImageCache::with_dir(dir.clone()).unwrap();

        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
        let path = cache.put("thumb_svg", &svg, None).unwrap();

        assert!(path.to_string_lossy().ends_with(".svg"));
        assert_eq!(cache.get("thumb_svg").unwrap(), svg);

        cleanup_dir(&dir);
    }
}
