//! Logos for the station browser
//!
//! Each logo is downloaded once, shrunk to a small PNG and kept in its own
//! disk cache, keyed by the logo URL. The browser only decodes the logos
//! of the rows on screen, so memory stays flat however long the list is.

use crate::data::cache::{cache_dir, ImageCache};
use crate::data::types::url_to_id;
use crate::error::Result;
use crate::network::LogoService;
use std::collections::HashSet;
use std::sync::Mutex;

/// Logos kept on disk; the oldest go first beyond this
pub const MAX_CACHED: usize = 2000;

/// Size the browser shows logos at (2x its 40px for HiDPI screens)
pub const ROW_LOGO_SIZE: u32 = 80;

/// RGBA pixels with width and height
pub type Rgba = (Vec<u8>, u32, u32);

pub struct BrowseLogos {
    cache: ImageCache,
    /// Logo URLs that failed to download or decode this session, so
    /// scrolling past them doesn't retry
    failed: Mutex<HashSet<String>>,
}

impl BrowseLogos {
    /// The cache under the app's cache folder
    pub fn open() -> Result<Self> {
        Ok(Self::with_cache(ImageCache::with_dir(
            cache_dir()?.join("browse-logos"),
        )?))
    }

    pub fn with_cache(cache: ImageCache) -> Self {
        Self {
            cache,
            failed: Mutex::new(HashSet::new()),
        }
    }

    /// The cached PNG for a logo URL, without any network request
    pub fn cached_png(&self, logo_url: &str) -> Option<Vec<u8>> {
        self.cache.get(&url_to_id(logo_url))
    }

    /// A logo shrunk for a browser row: from the disk cache, else
    /// downloaded, shrunk and cached. `None` when it can't be had.
    pub fn row_logo(&self, service: &LogoService, logo_url: &str) -> Option<Rgba> {
        if logo_url.is_empty() || self.has_failed(logo_url) {
            return None;
        }
        let png = self.cached_png(logo_url).or_else(|| {
            let data = service.fetch_raw(logo_url).ok()?;
            self.cache.put_thumbnail(&url_to_id(logo_url), &data)
        });
        let logo = png.and_then(|png| row_rgba(&png));
        if logo.is_none() {
            self.failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(logo_url.to_string());
        }
        logo
    }

    fn has_failed(&self, logo_url: &str) -> bool {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(logo_url)
    }

    /// Keep the disk cache within [`MAX_CACHED`] logos
    pub fn trim(&self) -> usize {
        self.cache.trim_to(MAX_CACHED)
    }
}

/// Decode a cached PNG to row size
fn row_rgba(png: &[u8]) -> Option<Rgba> {
    let img = image::load_from_memory(png).ok()?;
    let img = if img.width() > ROW_LOGO_SIZE || img.height() > ROW_LOGO_SIZE {
        img.thumbnail(ROW_LOGO_SIZE, ROW_LOGO_SIZE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp() -> (BrowseLogos, LogoService, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radiotrope_browse_logos_{}_{}",
            std::process::id(),
            n
        ));
        let logos = BrowseLogos::with_cache(ImageCache::with_dir(dir.join("browse")).unwrap());
        let service =
            LogoService::with_cache(ImageCache::with_dir(dir.join("fav")).unwrap()).unwrap();
        (logos, service, dir)
    }

    fn png(size: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(size, size, image::Rgba([1, 2, 3, 255]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn cached_logo_is_shrunk_to_row_size_without_network() {
        let (logos, service, dir) = temp();
        // An unreachable URL: only the cache can answer
        let url = "http://logo.invalid/big.png";
        logos
            .cache
            .put_thumbnail(&url_to_id(url), &png(400))
            .unwrap();

        let (rgba, w, h) = logos.row_logo(&service, url).unwrap();
        assert_eq!((w, h), (ROW_LOGO_SIZE, ROW_LOGO_SIZE));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(logos.cached_png(url).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn small_logo_keeps_its_size() {
        let (logos, service, dir) = temp();
        let url = "http://logo.invalid/small.png";
        logos
            .cache
            .put_thumbnail(&url_to_id(url), &png(32))
            .unwrap();
        let (_, w, h) = logos.row_logo(&service, url).unwrap();
        assert_eq!((w, h), (32, 32));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_logo_is_remembered_and_empty_url_skipped() {
        let (logos, service, dir) = temp();
        assert!(logos.row_logo(&service, "").is_none());
        let url = "http://logo.invalid/missing.png";
        assert!(logos.row_logo(&service, url).is_none());
        assert!(logos.has_failed(url));
        let _ = std::fs::remove_dir_all(dir);
    }
}
