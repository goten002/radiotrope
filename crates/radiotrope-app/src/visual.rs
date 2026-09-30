//! Visualizer helpers
//!
//! A noise gate so quiet bands (like the empty top of a low-passed MP3) drop
//! to nothing instead of sitting as frozen stubs, smoothing so bars glide
//! instead of flickering, the palette sampled from a station logo that
//! colours the bars, and the colour fog put behind logos that would vanish
//! on their tile.

/// Levels at or below this are noise; the rest is rescaled to 0-1
const NOISE_FLOOR: f32 = 0.08;

/// Remove the noise floor and stretch what is left back to 0-1
pub fn gate(level: f32) -> f32 {
    ((level.clamp(0.0, 1.0) - NOISE_FLOOR) / (1.0 - NOISE_FLOOR)).max(0.0)
}

/// Time a rising spectrum bar takes to cover about two thirds of the way
/// (seconds)
pub const SPECTRUM_RISE_SECS: f32 = 0.05;
/// Time a falling spectrum bar takes to drop about two thirds of the way
/// (seconds)
pub const SPECTRUM_FALL_SECS: f32 = 0.25;
/// Rise time of the VU meters (seconds)
pub const VU_RISE_SECS: f32 = 0.05;
/// Fall time of the VU meters (seconds); quicker than the spectrum so the
/// meters dip between beats
pub const VU_FALL_SECS: f32 = 0.15;

/// Frame-to-frame smoothing of visualizer levels: quick but not instant
/// rises, and gentle falls. Based on the time between frames, so motion
/// stays even when frames arrive late.
#[derive(Debug, Clone)]
pub struct LevelSmoother {
    levels: Vec<f32>,
    rise_secs: f32,
    fall_secs: f32,
}

impl LevelSmoother {
    /// `count` levels that rise and fall with the given time constants
    pub fn new(count: usize, rise_secs: f32, fall_secs: f32) -> Self {
        Self {
            levels: vec![0.0; count],
            rise_secs,
            fall_secs,
        }
    }

    /// Advance `dt` seconds towards `targets` (0-1) and return the new levels
    pub fn update(&mut self, targets: &[f32], dt: f32) -> &[f32] {
        let rise = 1.0 - (-dt / self.rise_secs).exp();
        let fall = 1.0 - (-dt / self.fall_secs).exp();
        for (level, &target) in self.levels.iter_mut().zip(targets) {
            let target = target.clamp(0.0, 1.0);
            let k = if target > *level { rise } else { fall };
            *level += (target - *level) * k;
        }
        &self.levels
    }

    /// Drop everything to zero (playback stopped)
    pub fn reset(&mut self) {
        self.levels.fill(0.0);
    }
}

/// Columns (and rows) of the Dot Matrix mode; each column averages two bands
pub const MATRIX_COLUMNS: usize = 8;
/// How long a Dot Matrix peak dot stays at its highest point (seconds)
pub const PEAK_HOLD_SECS: f32 = 0.45;
/// How fast a peak dot falls once its hold is over (levels per second)
pub const PEAK_FALL_PER_SEC: f32 = 0.9;

/// Average neighbouring bands into `columns` levels (Dot Matrix columns)
pub fn column_levels(spectrum: &[f32], columns: usize) -> Vec<f32> {
    let per = (spectrum.len() / columns.max(1)).max(1);
    spectrum
        .chunks(per)
        .take(columns)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect()
}

/// Peak markers that jump to each new high, hang there for a moment, then
/// fall slowly, like the peak dots on a hi-fi display
#[derive(Debug, Clone)]
pub struct PeakHold {
    peaks: Vec<f32>,
    hold: Vec<f32>,
}

impl PeakHold {
    pub fn new(count: usize) -> Self {
        Self {
            peaks: vec![0.0; count],
            hold: vec![0.0; count],
        }
    }

    /// Advance `dt` seconds with the current `levels` and return the peaks
    pub fn update(&mut self, levels: &[f32], dt: f32) -> &[f32] {
        for ((peak, hold), &level) in self.peaks.iter_mut().zip(&mut self.hold).zip(levels) {
            let level = level.clamp(0.0, 1.0);
            if level >= *peak {
                *peak = level;
                *hold = PEAK_HOLD_SECS;
            } else if *hold > 0.0 {
                *hold -= dt;
            } else {
                *peak = (*peak - PEAK_FALL_PER_SEC * dt).max(level);
            }
        }
        &self.peaks
    }

    pub fn reset(&mut self) {
        self.peaks.fill(0.0);
        self.hold.fill(0.0);
    }
}

/// Hue buckets used to group logo colours (30 degrees each)
const HUE_BINS: usize = 12;
/// Pixels darker than this (HSV value) are ignored; they count as
/// near-black, the light theme's fallback colour
const MIN_VALUE: f32 = 0.25;
/// Pixels greyer than this (HSV saturation) are ignored, which also drops white
const MIN_SATURATION: f32 = 0.3;
/// Grey pixels at least this bright (HSV value) count as near-white,
/// the dark theme's fallback colour
const MIN_LIGHT_VALUE: f32 = 0.8;
/// A colour needs this share of the colourful pixels to make the palette
const MIN_SHARE: f32 = 0.08;
/// Logos with fewer colourful pixels than this share have no palette.
/// Low enough to keep a small coloured mark on a dark logo (about 2.7% of
/// the pixels), high enough to ignore stray JPEG noise in black and white.
const MIN_COLOURFUL: f32 = 0.01;
/// Palette colours are brightened to at least this value so bars stay
/// visible on the dark tile
const PALETTE_MIN_VALUE: f32 = 0.7;

/// Up to three main colours of a logo, most common first, as RGB.
///
/// `rgba` is the image's pixels in RGBA order. Transparent, very dark and
/// grey or white pixels are skipped, the rest are grouped by hue and each
/// group is averaged. A logo with no colours falls back to its near-white
/// on the dark theme or its near-black on the light one (`dark` picks),
/// so black-and-white logos still get their own look. Returns an empty
/// list when there is nothing to use (the accent is used then).
pub fn logo_palette(rgba: &[u8], dark: bool) -> Vec<[u8; 3]> {
    let mut sums = [[0f64; 3]; HUE_BINS];
    let mut counts = [0usize; HUE_BINS];
    let mut total = 0usize;
    // Near-white (dark theme) or near-black (light theme) pixels
    let mut neutral_sum = [0f64; 3];
    let mut neutral_count = 0usize;

    for px in rgba.chunks_exact(4) {
        if px[3] < 128 {
            continue;
        }
        total += 1;
        let (h, s, v) = rgb_to_hsv(px[0], px[1], px[2]);
        let is_neutral = if dark {
            s < MIN_SATURATION && v >= MIN_LIGHT_VALUE
        } else {
            v < MIN_VALUE
        };
        if is_neutral {
            for c in 0..3 {
                neutral_sum[c] += px[c] as f64;
            }
            neutral_count += 1;
        }
        if v < MIN_VALUE || s < MIN_SATURATION {
            continue;
        }
        let bin = ((h / 360.0 * HUE_BINS as f32) as usize).min(HUE_BINS - 1);
        for c in 0..3 {
            sums[bin][c] += px[c] as f64;
        }
        counts[bin] += 1;
    }

    let enough = |n: usize| total > 0 && n as f32 >= total as f32 * MIN_COLOURFUL;
    let colourful: usize = counts.iter().sum();
    if !enough(colourful) {
        if !enough(neutral_count) {
            return Vec::new();
        }
        let n = neutral_count as f64;
        return vec![neutral_sum.map(|c| (c / n) as u8)];
    }

    let mut bins: Vec<usize> = (0..HUE_BINS)
        .filter(|&b| counts[b] as f32 >= colourful as f32 * MIN_SHARE)
        .collect();
    bins.sort_by(|a, b| counts[*b].cmp(&counts[*a]));
    bins.truncate(3);

    bins.into_iter()
        .map(|b| {
            let n = counts[b] as f64;
            let avg = [
                (sums[b][0] / n) as u8,
                (sums[b][1] / n) as u8,
                (sums[b][2] / n) as u8,
            ];
            brighten(avg)
        })
        .collect()
}

/// Logo tile colour on the dark theme (Theme.elevated in ui/defaults.slint)
const TILE_DARK: [u8; 3] = [0x1b, 0x1c, 0x1f];
/// Logo tile colour on the light theme (Theme.elevated in ui/defaults.slint)
const TILE_LIGHT: [u8; 3] = [0xf0, 0xf0, 0xf4];
/// Share of transparent pixels that makes a logo see-through. Logos with
/// their own background (JPEGs, filled squares) never get a fog.
const MIN_TRANSPARENT: f32 = 0.10;
/// Pixels with less contrast than this against the tile are hard to see
/// (WCAG contrast ratio; 1 is the same colour, 21 black on white)
const MIN_CONTRAST: f32 = 1.8;
/// Share of the logo's outline (visible pixels next to transparent ones,
/// where the logo meets the tile) that must be hard to see before the logo
/// gets a fog. A white mark inside a coloured shape never touches the tile,
/// so it does not count. Low enough for thin lettering under a big coloured
/// mark (Fly 104: about a quarter of the outline at list size).
const MIN_HIDDEN_OUTLINE: f32 = 0.2;
/// Those hard-to-see outline pixels must also make up this share of the
/// whole visible logo, so a stray dark speck or a tiny tagline is left
/// alone (Fly 104's lettering: about 3.5%)
const MIN_HIDDEN_EDGE: f32 = 0.02;
/// Fog colour of logos with no colours of their own, per theme
const FOG_GREY_DARK: [u8; 3] = [0xc8, 0xc9, 0xcc];
const FOG_GREY_LIGHT: [u8; 3] = [0x3a, 0x3c, 0x41];

/// The colour fog behind a logo: a soft glow in each of two corners over a
/// flat ground colour
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backdrop {
    /// Glow in the top left corner
    pub first: [u8; 3],
    /// Glow in the bottom right corner
    pub second: [u8; 3],
    pub ground: [u8; 3],
}

/// The fog a see-through logo needs behind it so it does not vanish on the
/// tile, on the dark theme (`dark`) or the light one. `None` when the logo
/// shows well as it is.
///
/// `rgba` is the image's pixels in RGBA order, `width` pixels to a row.
/// A logo gets one when it is partly transparent and much of it, above all
/// where it meets the tile, is too close to the tile colour: black
/// lettering on the dark theme, white lettering on the light one. The fog
/// uses the
/// logo's own colours (as the visualizer does), made pale on the dark theme
/// and deep on the light one so the lettering stands out; logos with no
/// colours get a grey fog.
pub fn logo_backdrop(rgba: &[u8], width: usize, dark: bool) -> Option<Backdrop> {
    let pixels = rgba.len() / 4;
    if width == 0 || pixels == 0 {
        return None;
    }
    let height = pixels / width;
    // Beyond the image's edge is the tile too
    let clear =
        |x: usize, y: usize| x >= width || y >= height || rgba[(y * width + x) * 4 + 3] < 128;
    let tile = relative_luminance(if dark { TILE_DARK } else { TILE_LIGHT });
    let (mut see_through, mut outline, mut hidden_outline) = (0, 0, 0);
    for y in 0..height {
        for x in 0..width {
            if clear(x, y) {
                see_through += 1;
                continue;
            }
            let px = &rgba[(y * width + x) * 4..];
            let lum = relative_luminance([px[0], px[1], px[2]]);
            let (hi, lo) = if lum > tile { (lum, tile) } else { (tile, lum) };
            let is_hidden = (hi + 0.05) / (lo + 0.05) < MIN_CONTRAST;
            let on_outline = x == 0
                || y == 0
                || clear(x - 1, y)
                || clear(x + 1, y)
                || clear(x, y - 1)
                || clear(x, y + 1);
            outline += on_outline as usize;
            hidden_outline += (is_hidden && on_outline) as usize;
        }
    }
    let visible = width * height - see_through;
    let share = |part: usize, whole: usize| part as f32 / whole.max(1) as f32;
    if visible == 0
        || share(see_through, width * height) < MIN_TRANSPARENT
        || share(hidden_outline, outline) < MIN_HIDDEN_OUTLINE
        || share(hidden_outline, visible) < MIN_HIDDEN_EDGE
    {
        return None;
    }

    // Colourful palette entries only; the near-white or near-black fallback
    // for black-and-white logos is grey, which gets the grey fog instead
    let colors: Vec<[u8; 3]> = logo_palette(rgba, true)
        .into_iter()
        .filter(|&[r, g, b]| rgb_to_hsv(r, g, b).1 >= MIN_SATURATION)
        .collect();
    let grey = if dark { FOG_GREY_DARK } else { FOG_GREY_LIGHT };
    let first = colors.first().copied().unwrap_or(grey);
    let second = colors.get(1).copied().unwrap_or(first);
    Some(if dark {
        let white = [255; 3];
        Backdrop {
            first: mix(first, white, 0.58),
            second: mix(second, white, 0.70),
            ground: mix(first, white, 0.80),
        }
    } else {
        let black = [0; 3];
        Backdrop {
            first: mix(first, black, 0.50),
            second: mix(second, black, 0.62),
            ground: mix(first, black, 0.72),
        }
    })
}

/// Relative luminance of an sRGB colour (0 black to 1 white), as WCAG
/// defines it for contrast ratios
fn relative_luminance(rgb: [u8; 3]) -> f32 {
    let [r, g, b] = rgb.map(|c| {
        let c = c as f32 / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// `a` moved towards `b` by `t` (0 keeps `a`, 1 gives `b`)
fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    [0, 1, 2].map(|i| (a[i] as f32 * (1.0 - t) + b[i] as f32 * t).round() as u8)
}

/// Hue (0-360), saturation and value (0-1)
fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

/// Scale a colour up so its brightest channel reaches PALETTE_MIN_VALUE
fn brighten(rgb: [u8; 3]) -> [u8; 3] {
    let max = *rgb.iter().max().unwrap_or(&0) as f32 / 255.0;
    if max >= PALETTE_MIN_VALUE || max == 0.0 {
        return rgb;
    }
    let k = PALETTE_MIN_VALUE / max;
    rgb.map(|c| ((c as f32 * k).round()).min(255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(pixels: &[([u8; 3], usize)]) -> Vec<u8> {
        pixels
            .iter()
            .flat_map(|&(rgb, n)| std::iter::repeat([rgb[0], rgb[1], rgb[2], 255]).take(n))
            .flatten()
            .collect()
    }

    #[test]
    fn gate_drops_noise_and_rescales() {
        assert_eq!(gate(0.0), 0.0);
        assert_eq!(gate(NOISE_FLOOR), 0.0);
        assert_eq!(gate(1.0), 1.0);
        assert!((gate(0.54) - 0.5).abs() < 1e-6);
        assert_eq!(gate(5.0), 1.0);
    }

    #[test]
    fn smoother_rises_quickly_and_falls_gently() {
        let mut s = LevelSmoother::new(1, SPECTRUM_RISE_SECS, SPECTRUM_FALL_SECS);
        let up = s.update(&[1.0], SPECTRUM_RISE_SECS)[0];
        assert!((up - (1.0 - (-1f32).exp())).abs() < 1e-6);
        let top = s.update(&[1.0], 0.5)[0];
        assert!(top > 0.99);
        let down = s.update(&[0.0], SPECTRUM_RISE_SECS)[0];
        assert!(down > 0.7, "falls slower than it rises");
        let gone = s.update(&[0.0], 2.0)[0];
        assert!(gone < 0.01);
    }

    #[test]
    fn smoother_same_motion_at_any_frame_rate() {
        let mut fast = LevelSmoother::new(1, SPECTRUM_RISE_SECS, SPECTRUM_FALL_SECS);
        let mut slow = LevelSmoother::new(1, SPECTRUM_RISE_SECS, SPECTRUM_FALL_SECS);
        for _ in 0..4 {
            fast.update(&[1.0], 0.0125);
        }
        slow.update(&[1.0], 0.05);
        assert!((fast.update(&[1.0], 0.0)[0] - slow.update(&[1.0], 0.0)[0]).abs() < 1e-5);
    }

    #[test]
    fn smoother_reset_and_clamp() {
        let mut s = LevelSmoother::new(2, SPECTRUM_RISE_SECS, SPECTRUM_FALL_SECS);
        s.update(&[5.0, -1.0], 0.1);
        assert!(s.update(&[5.0, -1.0], 0.1)[0] <= 1.0);
        assert_eq!(s.update(&[5.0, -1.0], 0.1)[1], 0.0);
        s.reset();
        assert_eq!(s.update(&[0.0, 0.0], 0.1), &[0.0, 0.0]);
    }

    #[test]
    fn green_logo_gives_green() {
        let img = image(&[([255, 255, 255], 500), ([90, 190, 90], 300)]);
        let p = logo_palette(&img, true);
        assert_eq!(p.len(), 1);
        let [r, g, b] = p[0];
        assert!(g > r && g > b);
    }

    #[test]
    fn colours_ordered_by_share() {
        let img = image(&[([220, 30, 30], 100), ([30, 60, 220], 300), ([0, 0, 0], 400)]);
        let p = logo_palette(&img, true);
        assert_eq!(p.len(), 2);
        assert!(p[0][2] > p[0][0], "blue first");
        assert!(p[1][0] > p[1][2], "red second");
    }

    #[test]
    fn grey_logo_has_no_palette() {
        let img = image(&[([128, 128, 128], 300), ([150, 150, 150], 300)]);
        assert!(logo_palette(&img, true).is_empty());
        assert!(logo_palette(&img, false).is_empty());
    }

    #[test]
    fn black_and_white_logo_falls_back_to_the_theme_neutral() {
        let img = image(&[
            ([5, 5, 5], 300),
            ([245, 245, 245], 200),
            ([128, 128, 128], 50),
        ]);
        assert_eq!(
            logo_palette(&img, true),
            vec![[245, 245, 245]],
            "white on dark"
        );
        assert_eq!(logo_palette(&img, false), vec![[5, 5, 5]], "black on light");
    }

    #[test]
    fn neutrals_only_when_there_are_no_colours() {
        // Mostly white with a small red mark: the red wins on both themes
        let img = image(&[
            ([250, 250, 250], 900),
            ([10, 10, 10], 50),
            ([220, 30, 30], 50),
        ]);
        for dark in [true, false] {
            let p = logo_palette(&img, dark);
            assert_eq!(p.len(), 1);
            assert!(p[0][0] > p[0][1] && p[0][0] > p[0][2]);
        }
    }

    #[test]
    fn white_only_logo_has_no_palette_on_the_light_theme() {
        let img = image(&[([250, 250, 250], 400), ([120, 120, 120], 100)]);
        assert_eq!(logo_palette(&img, true), vec![[250, 250, 250]]);
        assert!(logo_palette(&img, false).is_empty());
    }

    #[test]
    fn small_mark_on_dark_logo_gives_its_colour() {
        // A red mark on a mostly black and grey logo
        let img = image(&[
            ([12, 12, 12], 870),
            ([200, 200, 200], 100),
            ([220, 20, 30], 27),
        ]);
        let p = logo_palette(&img, true);
        assert_eq!(p.len(), 1);
        assert!(p[0][0] > p[0][1] && p[0][0] > p[0][2]);
    }

    #[test]
    fn at_most_three_colours() {
        let img = image(&[
            ([230, 30, 30], 100),
            ([30, 230, 30], 100),
            ([30, 30, 230], 100),
            ([230, 230, 30], 100),
        ]);
        assert_eq!(logo_palette(&img, true).len(), 3);
    }

    #[test]
    fn transparent_pixels_ignored() {
        let mut img = image(&[([200, 40, 40], 50)]);
        img.extend([30, 200, 30, 0].repeat(1000));
        let p = logo_palette(&img, true);
        assert_eq!(p.len(), 1);
        assert!(p[0][0] > p[0][1]);
    }

    #[test]
    fn dark_colours_brightened() {
        let img = image(&[([20, 20, 120], 100)]);
        let p = logo_palette(&img, true);
        assert_eq!(p.len(), 1);
        assert!(p[0][2] as f32 / 255.0 >= PALETTE_MIN_VALUE - 0.01);
    }

    #[test]
    fn empty_image() {
        assert!(logo_palette(&[], true).is_empty());
    }

    /// Width of the test logos below
    const W: usize = 10;

    /// A see-through logo, `W` pixels wide: `n` transparent pixels, then
    /// the given pixels
    fn see_through(n: usize, pixels: &[([u8; 3], usize)]) -> Vec<u8> {
        let mut img = vec![0u8; n * 4];
        img.extend(image(pixels));
        img
    }

    /// Rows of `W` pixels: ' ' transparent, 'k' near-black, 'w' white,
    /// 'g' green
    fn drawn(rows: &[&str]) -> Vec<u8> {
        rows.iter()
            .flat_map(|row| row.chars())
            .flat_map(|c| match c {
                'k' => [17, 17, 17, 255],
                'w' => [255, 255, 255, 255],
                'g' => [47, 191, 90, 255],
                _ => [0, 0, 0, 0],
            })
            .collect()
    }

    #[test]
    fn backdrop_for_black_lettering_on_dark_only() {
        // Black text under a green mark, on a transparent background
        let img = drawn(&[
            "          ",
            "   gggg   ",
            "   gggg   ",
            "   gggg   ",
            "          ",
            " k k kk k ",
            " k k k  k ",
            "          ",
        ]);
        let fog = logo_backdrop(&img, W, true).expect("black text needs a fog on dark");
        // Pale, and tinted by the green mark
        assert!(fog.ground.iter().all(|&c| c > 180), "{fog:?}");
        assert!(fog.first[1] > fog.first[0] && fog.first[1] > fog.first[2]);
        let on_light = logo_backdrop(&img, W, false);
        assert_eq!(on_light, None, "black text shows on light");
    }

    #[test]
    fn backdrop_for_white_lettering_on_light_only() {
        let img = see_through(60, &[([255, 255, 255], 25), ([255, 122, 26], 15)]);
        let fog = logo_backdrop(&img, W, false).expect("white text needs a fog on light");
        // Deep, and tinted by the orange mark
        assert!(fog.ground.iter().all(|&c| c < 80), "{fog:?}");
        assert!(fog.first[0] > fog.first[2]);
        let on_dark = logo_backdrop(&img, W, true);
        assert_eq!(on_dark, None, "white text shows on dark");
    }

    #[test]
    fn backdrop_grey_for_logos_without_colour() {
        let img = see_through(60, &[([20, 20, 20], 40)]);
        let fog = logo_backdrop(&img, W, true).unwrap();
        assert_eq!(fog.first, mix(FOG_GREY_DARK, [255; 3], 0.58));
        assert_eq!(fog.second, mix(FOG_GREY_DARK, [255; 3], 0.70));
        let img = see_through(60, &[([240, 240, 240], 40)]);
        let fog = logo_backdrop(&img, W, false).unwrap();
        assert_eq!(fog.ground, mix(FOG_GREY_LIGHT, [0; 3], 0.72));
    }

    /// A 60x60 logo: a large green square above a strip where `dark(x, y)`
    /// picks the near-black pixels, on a transparent background
    fn square_with(dark: impl Fn(usize, usize) -> bool) -> Vec<u8> {
        (0..60 * 60)
            .flat_map(|i| {
                let (x, y) = (i % 60, i / 60);
                if (2..58).contains(&x) && (2..40).contains(&y) {
                    [47, 191, 90, 255]
                } else if (2..58).contains(&x) && dark(x, y) {
                    [17, 17, 17, 255]
                } else {
                    [0, 0, 0, 0]
                }
            })
            .collect()
    }

    #[test]
    fn backdrop_for_thin_lettering_under_a_big_mark() {
        // Like Fly 104: a big green mark, thin dark lettering below it
        let img = square_with(|x, y| (44..47).contains(&y) && x % 4 != 0);
        assert!(logo_backdrop(&img, 60, true).is_some());
        assert_eq!(logo_backdrop(&img, 60, false), None);
    }

    #[test]
    fn no_backdrop_for_opaque_or_enclosed_parts() {
        // Opaque: the logo has its own background
        let opaque = image(&[([17, 17, 17], 100)]);
        assert_eq!(logo_backdrop(&opaque, W, true), None);
        // A white mark inside a green shape never touches the light tile
        let inside = drawn(&[
            "          ",
            " gggggggg ",
            " ggwwwwgg ",
            " ggwwwwgg ",
            " ggwwwwgg ",
            " gggggggg ",
            "          ",
        ]);
        assert_eq!(logo_backdrop(&inside, W, false), None);
        // A dark speck on the edge of a large green shape is left alone
        let speck = square_with(|x, y| x < 5 && (44..47).contains(&y));
        assert_eq!(logo_backdrop(&speck, 60, true), None);
        // Fully transparent, or nothing at all
        assert_eq!(logo_backdrop(&see_through(10, &[]), W, true), None);
        assert_eq!(logo_backdrop(&[], W, true), None);
        assert_eq!(logo_backdrop(&opaque, 0, true), None);
    }

    #[test]
    fn contrast_measure() {
        let ratio = |a: [u8; 3], b: [u8; 3]| {
            let (a, b) = (relative_luminance(a), relative_luminance(b));
            (a.max(b) + 0.05) / (a.min(b) + 0.05)
        };
        assert!((ratio([0; 3], [255; 3]) - 21.0).abs() < 0.01);
        assert!(ratio([17; 3], TILE_DARK) < MIN_CONTRAST);
        assert!(ratio([255; 3], TILE_LIGHT) < MIN_CONTRAST);
        assert!(ratio([255; 3], TILE_DARK) > MIN_CONTRAST);
    }

    #[test]
    fn column_levels_average_pairs() {
        let spectrum: Vec<f32> = (0..16).map(|i| i as f32 / 16.0).collect();
        let cols = column_levels(&spectrum, MATRIX_COLUMNS);
        assert_eq!(cols.len(), 8);
        assert!((cols[0] - 0.5 / 16.0).abs() < 1e-6);
        assert!((cols[7] - 14.5 / 16.0).abs() < 1e-6);
    }

    #[test]
    fn peak_holds_then_falls() {
        let mut p = PeakHold::new(1);
        assert_eq!(p.update(&[0.8], 0.03)[0], 0.8);
        // Held while the hold time runs
        assert_eq!(p.update(&[0.1], 0.2)[0], 0.8);
        assert_eq!(p.update(&[0.1], 0.2)[0], 0.8);
        assert_eq!(p.update(&[0.1], 0.1)[0], 0.8);
        // Then falls, but never below the level
        let fallen = p.update(&[0.1], 0.1)[0];
        assert!((fallen - (0.8 - PEAK_FALL_PER_SEC * 0.1)).abs() < 1e-6);
        for _ in 0..50 {
            p.update(&[0.1], 0.1);
        }
        assert_eq!(p.update(&[0.1], 0.1)[0], 0.1);
        // A new high jumps straight up
        assert_eq!(p.update(&[0.9], 0.01)[0], 0.9);
        p.reset();
        assert_eq!(p.update(&[0.0], 0.01)[0], 0.0);
    }
}
