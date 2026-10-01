//! Logos for the station browser
//!
//! Each logo is downloaded once, shrunk to a small PNG and kept in its own
//! disk cache, keyed by the logo URL. The browser only decodes the logos
//! of the rows on screen, so memory stays flat however long the list is.

use crate::data::cache::{decode_logo, ImageCache};
use crate::data::types::url_to_id;
use crate::network::failed_logos::FailedLogos;
use crate::network::LogoService;

/// Logos not shown for this long are removed from disk at startup
pub const UNUSED_AFTER: std::time::Duration = std::time::Duration::from_secs(90 * 24 * 60 * 60);

/// Size the browser shows logos at (2x its 40px for HiDPI screens)
pub const ROW_LOGO_SIZE: u32 = 80;

/// RGBA pixels with width and height
pub type Rgba = (Vec<u8>, u32, u32);

pub struct BrowseLogos {
    cache: ImageCache,
    /// Logo URLs that failed lately, so scrolling past them doesn't retry.
    /// A network drop is forgotten after a while; a refused or broken logo
    /// is kept for the session.
    failed: FailedLogos,
}

impl BrowseLogos {
    /// The cache under the app's cache folder (see [`ImageCache::open`]
    /// for when that can't be created)
    pub fn open() -> Self {
        Self::with_cache(ImageCache::open("browse-logos"))
    }

    pub fn with_cache(cache: ImageCache) -> Self {
        Self {
            cache,
            failed: FailedLogos::new(),
        }
    }

    /// The cached PNG for a logo URL, without any network request
    pub fn cached_png(&self, logo_url: &str) -> Option<Vec<u8>> {
        self.cache.get(&url_to_id(logo_url))
    }

    /// Remove logos not shown for [`UNUSED_AFTER`]
    pub fn remove_unused(&self) -> usize {
        self.cache.remove_unused(UNUSED_AFTER)
    }

    /// A logo shrunk for a browser row: from the disk cache, else
    /// downloaded, shrunk and cached. `None` when it can't be had.
    pub fn row_logo(&self, service: &LogoService, logo_url: &str) -> Option<Rgba> {
        if logo_url.is_empty() || self.failed.has_failed(logo_url) {
            return None;
        }
        let key = url_to_id(logo_url);
        let png = match self.cache.get(&key) {
            Some(png) => {
                self.cache.touch(&key);
                png
            }
            None => match service.fetch_raw(logo_url) {
                Ok(data) => match self.cache.put_thumbnail(&key, &data) {
                    Some(png) => png,
                    // Not an image we can show (a web page, an SVG)
                    None => {
                        self.failed.record_unusable(logo_url);
                        return None;
                    }
                },
                Err(e) => {
                    self.failed.record(logo_url, &e);
                    return None;
                }
            },
        };
        let logo = row_rgba(&png);
        match logo {
            Some(_) => self.failed.clear(logo_url),
            None => self.failed.record_unusable(logo_url),
        }
        logo
    }

    #[cfg(test)]
    fn has_failed(&self, logo_url: &str) -> bool {
        self.failed.has_failed(logo_url)
    }
}

/// Decode a cached PNG to row size
fn row_rgba(png: &[u8]) -> Option<Rgba> {
    let img = decode_logo(png).ok()?;
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
    use std::time::Duration;

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
    fn a_page_instead_of_a_logo_is_remembered() {
        use crate::network::test_http::{ok, serve};
        let (logos, service, dir) = temp();
        let server = serve(ok(b"<html>Not here</html>", true), Duration::ZERO);
        let url = format!("{server}/favicon.ico");
        assert!(logos.row_logo(&service, &url).is_none());
        assert!(logos.has_failed(&url));
        assert!(logos.cached_png(&url).is_none());
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
