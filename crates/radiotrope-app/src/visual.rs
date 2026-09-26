//! Visualizer helpers
//!
//! A noise gate so quiet bands (like the empty top of a low-passed MP3) drop
//! to nothing instead of sitting as frozen stubs, smoothing so bars glide
//! instead of flickering, and the palette sampled from a station logo that
//! colours the bars.

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

/// Hue buckets used to group logo colours (30 degrees each)
const HUE_BINS: usize = 12;
/// Pixels darker than this (HSV value) are ignored
const MIN_VALUE: f32 = 0.25;
/// Pixels greyer than this (HSV saturation) are ignored, which also drops white
const MIN_SATURATION: f32 = 0.3;
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
/// group is averaged. Returns an empty list for black-and-white logos.
pub fn logo_palette(rgba: &[u8]) -> Vec<[u8; 3]> {
    let mut sums = [[0f64; 3]; HUE_BINS];
    let mut counts = [0usize; HUE_BINS];
    let mut total = 0usize;

    for px in rgba.chunks_exact(4) {
        if px[3] < 128 {
            continue;
        }
        total += 1;
        let (h, s, v) = rgb_to_hsv(px[0], px[1], px[2]);
        if v < MIN_VALUE || s < MIN_SATURATION {
            continue;
        }
        let bin = ((h / 360.0 * HUE_BINS as f32) as usize).min(HUE_BINS - 1);
        for c in 0..3 {
            sums[bin][c] += px[c] as f64;
        }
        counts[bin] += 1;
    }

    let colourful: usize = counts.iter().sum();
    if total == 0 || (colourful as f32) < total as f32 * MIN_COLOURFUL {
        return Vec::new();
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
        let p = logo_palette(&img);
        assert_eq!(p.len(), 1);
        let [r, g, b] = p[0];
        assert!(g > r && g > b);
    }

    #[test]
    fn colours_ordered_by_share() {
        let img = image(&[([220, 30, 30], 100), ([30, 60, 220], 300), ([0, 0, 0], 400)]);
        let p = logo_palette(&img);
        assert_eq!(p.len(), 2);
        assert!(p[0][2] > p[0][0], "blue first");
        assert!(p[1][0] > p[1][2], "red second");
    }

    #[test]
    fn black_and_white_logo_has_no_palette() {
        let img = image(&[
            ([0, 0, 0], 300),
            ([255, 255, 255], 300),
            ([128, 128, 128], 50),
        ]);
        assert!(logo_palette(&img).is_empty());
    }

    #[test]
    fn small_mark_on_dark_logo_gives_its_colour() {
        // A red mark on a mostly black and grey logo
        let img = image(&[
            ([12, 12, 12], 870),
            ([200, 200, 200], 100),
            ([220, 20, 30], 27),
        ]);
        let p = logo_palette(&img);
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
        assert_eq!(logo_palette(&img).len(), 3);
    }

    #[test]
    fn transparent_pixels_ignored() {
        let mut img = image(&[([200, 40, 40], 50)]);
        img.extend([30, 200, 30, 0].repeat(1000));
        let p = logo_palette(&img);
        assert_eq!(p.len(), 1);
        assert!(p[0][0] > p[0][1]);
    }

    #[test]
    fn dark_colours_brightened() {
        let img = image(&[([20, 20, 120], 100)]);
        let p = logo_palette(&img);
        assert_eq!(p.len(), 1);
        assert!(p[0][2] as f32 / 255.0 >= PALETTE_MIN_VALUE - 0.01);
    }

    #[test]
    fn empty_image() {
        assert!(logo_palette(&[]).is_empty());
    }
}
