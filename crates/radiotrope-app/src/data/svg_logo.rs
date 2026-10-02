//! SVG station logos, drawn to pixels with resvg
//!
//! A logo URL can point at an SVG, which the `image` crate can't decode.
//! It is drawn here to fit a square, and from then on is handled like any
//! other logo (shrunk, cached as a PNG, coloured, fogged).
//!
//! An SVG can name other files: only pictures embedded in it (`data:`
//! URLs) are drawn, within the decode limits; files and web addresses are
//! never read. Fonts are loaded from the system once, and only for a logo
//! that has text.

use crate::config::logos::{MAX_DECODE_BYTES, MAX_DIMENSION};
use image::{ImageFormat, ImageReader, RgbaImage};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{self, fontdb, ImageHrefResolver, ImageKind};
use std::io::Cursor;
use std::sync::{Arc, OnceLock};

/// How far into the data to look for the `<svg` tag (an XML declaration,
/// a doctype and comments can come first)
const SNIFF_BYTES: usize = 4096;

/// Fonts tried, in order, for "sans-serif": text with no font
const SANS_SERIF_FONTS: &[&str] = &[
    "Arial",
    "Segoe UI",
    "Liberation Sans",
    "DejaVu Sans",
    "Noto Sans",
    "Cantarell",
];

/// Fonts tried, in order, for "serif": text naming a font the system lacks
/// falls back to it. The "sans-serif" font when none is found.
const SERIF_FONTS: &[&str] = &[
    "Times New Roman",
    "Liberation Serif",
    "DejaVu Serif",
    "Noto Serif",
];

/// Whether `data` looks like an SVG document
pub fn is_svg(data: &[u8]) -> bool {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    let start = data.iter().position(|b| !b.is_ascii_whitespace());
    let Some(data) = start.map(|i| &data[i..]) else {
        return false;
    };
    data.starts_with(b"<") && contains(&data[..data.len().min(SNIFF_BYTES)], b"<svg")
}

/// Draw an SVG to fit a `size` x `size` square, keeping its shape. Small
/// drawings are scaled up: they are vectors, so they stay sharp. An SVG
/// that draws nothing visible is refused.
pub fn render(data: &[u8], size: u32) -> Result<RgbaImage, String> {
    let options = usvg::Options {
        image_href_resolver: ImageHrefResolver {
            resolve_data: Box::new(|_, data, _| embedded_image(data)),
            resolve_string: Box::new(|_, _| None),
        },
        fontdb: if contains(data, b"<text") {
            system_fonts()
        } else {
            Arc::new(fontdb::Database::new())
        },
        font_family: "sans-serif".to_string(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(data, &options).map_err(|e| e.to_string())?;
    let drawing = tree.size();
    let scale = (size as f32 / drawing.width()).min(size as f32 / drawing.height());
    let width = ((drawing.width() * scale).round() as u32).clamp(1, size);
    let height = ((drawing.height() * scale).round() as u32).clamp(1, size);
    let mut pixmap = Pixmap::new(width, height).ok_or("empty drawing")?;
    resvg::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    if pixmap.pixels().iter().all(|p| p.alpha() == 0) {
        return Err("the drawing is empty".to_string());
    }
    RgbaImage::from_raw(width, height, pixmap.take_demultiplied())
        .ok_or_else(|| "bad pixel buffer".to_string())
}

/// A picture embedded in the SVG, if it is one we decode anyway and within
/// the decode limits (a small file can claim a huge picture). Nested SVGs
/// aren't drawn.
fn embedded_image(data: Arc<Vec<u8>>) -> Option<ImageKind> {
    let reader = ImageReader::new(Cursor::new(data.as_slice()))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) * 4 > MAX_DECODE_BYTES
    {
        return None;
    }
    match format {
        ImageFormat::Png => Some(ImageKind::PNG(data)),
        ImageFormat::Jpeg => Some(ImageKind::JPEG(data)),
        ImageFormat::Gif => Some(ImageKind::GIF(data)),
        ImageFormat::WebP => Some(ImageKind::WEBP(data)),
        _ => None,
    }
}

/// The system's fonts, loaded on first use, with "sans-serif" and "serif"
/// set to fonts the system has (see [`SANS_SERIF_FONTS`], [`SERIF_FONTS`])
fn system_fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            let first_found = |names: &[&'static str]| {
                names.iter().copied().find(|name| {
                    db.faces()
                        .any(|face| face.families.iter().any(|(family, _)| family == name))
                })
            };
            let sans_serif = first_found(SANS_SERIF_FONTS);
            let serif = first_found(SERIF_FONTS).or(sans_serif);
            if let Some(name) = sans_serif {
                db.set_sans_serif_family(name);
            }
            if let Some(name) = serif {
                db.set_serif_family(name);
            }
            Arc::new(db)
        })
        .clone()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><rect width="20" height="20" fill="#ff0000"/></svg>"##;

    #[test]
    fn svg_is_told_from_other_data() {
        assert!(is_svg(SQUARE));
        assert!(is_svg(
            b"\xEF\xBB\xBF\n<?xml version=\"1.0\"?>\n<!-- logo -->\n<svg/>"
        ));
        assert!(!is_svg(
            b"<!DOCTYPE html><html><body>Not found</body></html>"
        ));
        assert!(!is_svg(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_svg(b"   "));
    }

    #[test]
    fn a_small_drawing_is_scaled_up_to_fit_keeping_its_shape() {
        let wide = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 10"><rect width="40" height="10" fill="#00ff00"/></svg>"##;
        let img = render(wide, 160).unwrap();
        assert_eq!(img.dimensions(), (160, 40));
        assert_eq!(img.get_pixel(80, 20).0, [0, 255, 0, 255]);

        let img = render(SQUARE, 160).unwrap();
        assert_eq!(img.dimensions(), (160, 160));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn half_see_through_colours_come_out_unpremultiplied() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#ff0000" fill-opacity="0.5"/></svg>"##;
        let px = render(svg, 8).unwrap().get_pixel(4, 4).0;
        assert_eq!(px[0], 255);
        assert!((127..=128).contains(&px[3]), "{px:?}");
    }

    #[test]
    fn an_empty_or_broken_svg_is_refused() {
        assert!(render(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>", 160).is_err());
        assert!(render(b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect", 160).is_err());
    }

    #[test]
    fn text_is_drawn_with_a_system_font() {
        if system_fonts().is_empty() {
            eprintln!("no system fonts: skipped");
            return;
        }
        // No font, and a font no system has (falls back to "serif")
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="40"><text x="4" y="30" font-family="No Such Font" font-size="28" fill="#000">FM</text></svg>"##;
        let img = render(svg, 80).unwrap();
        assert!(img.pixels().filter(|p| p.0[3] > 0).count() > 50);
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="40"><text x="4" y="30" font-size="28" fill="#000">FM</text></svg>"##;
        let img = render(svg, 80).unwrap();
        assert!(img.pixels().filter(|p| p.0[3] > 0).count() > 50);
    }

    #[test]
    fn files_named_by_the_svg_are_not_read() {
        let dir = std::env::temp_dir().join(format!("radiotrope-svg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("secret.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 255, 255]))
            .save(&png)
            .unwrap();
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4"><image width="4" height="4" xlink:href="{}"/></svg>"#,
            png.display()
        );
        let drawn = render(svg.as_bytes(), 4);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(drawn.is_err(), "the file was drawn");
    }

    #[test]
    fn an_embedded_picture_is_drawn_unless_it_claims_to_be_huge() {
        use base64_png::encode;
        let small = image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 255, 255]));
        let svg = |png: &[u8]| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4"><image width="4" height="4" xlink:href="data:image/png;base64,{}"/></svg>"#,
                encode(png)
            )
        };
        let mut png = Vec::new();
        small
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let img = render(svg(&png).as_bytes(), 4).unwrap();
        assert_eq!(img.get_pixel(2, 2).0, [0, 0, 255, 255]);

        // Wider than MAX_DIMENSION: not drawn, so nothing is
        let mut huge = Vec::new();
        image::RgbaImage::from_pixel(MAX_DIMENSION + 1, 1, image::Rgba([0, 0, 255, 255]))
            .write_to(&mut Cursor::new(&mut huge), ImageFormat::Png)
            .unwrap();
        assert!(render(svg(&huge).as_bytes(), 4).is_err());
    }

    /// Base64 for the test data URLs
    mod base64_png {
        pub fn encode(data: &[u8]) -> String {
            const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for chunk in data.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for i in 0..4 {
                    if i <= chunk.len() {
                        out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        }
    }
}
