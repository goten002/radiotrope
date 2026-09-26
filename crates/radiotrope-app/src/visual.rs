//! Visualizer display state
//!
//! Turns the analyzer's spectrum into what the visualizer draws: bars that
//! ease down between frames, peak dots that hold briefly and then fall, and
//! an SVG path for the filled-curve mode.

/// Share of the gap a falling bar closes per frame (higher = slower fall)
const BAR_RELEASE: f32 = 0.82;
/// Frames a peak dot stays put before falling
const PEAK_HOLD_FRAMES: u32 = 12;
/// How far a peak dot falls per frame once released
const PEAK_FALL: f32 = 0.025;

/// Per-frame spectrum state for the visualizer
#[derive(Debug, Clone)]
pub struct SpectrumDisplay {
    bars: Vec<f32>,
    peaks: Vec<f32>,
    hold: Vec<u32>,
}

impl SpectrumDisplay {
    /// Create the state for `bands` spectrum bands, all at zero
    pub fn new(bands: usize) -> Self {
        Self {
            bars: vec![0.0; bands],
            peaks: vec![0.0; bands],
            hold: vec![0; bands],
        }
    }

    /// Advance one frame with the latest analyzer levels (0.0-1.0)
    pub fn update(&mut self, levels: &[f32]) {
        for (i, &level) in levels.iter().enumerate().take(self.bars.len()) {
            let level = level.clamp(0.0, 1.0);
            let bar = &mut self.bars[i];
            *bar = if level >= *bar {
                level
            } else {
                *bar * BAR_RELEASE + level * (1.0 - BAR_RELEASE)
            };

            if *bar >= self.peaks[i] {
                self.peaks[i] = *bar;
                self.hold[i] = PEAK_HOLD_FRAMES;
            } else if self.hold[i] > 0 {
                self.hold[i] -= 1;
            } else {
                self.peaks[i] = (self.peaks[i] - PEAK_FALL).max(*bar);
            }
        }
    }

    /// Drop everything to zero (playback stopped)
    pub fn reset(&mut self) {
        self.bars.fill(0.0);
        self.peaks.fill(0.0);
        self.hold.fill(0);
    }

    pub fn bars(&self) -> &[f32] {
        &self.bars
    }

    pub fn peaks(&self) -> &[f32] {
        &self.peaks
    }

    /// Closed SVG path of the spectrum as a smooth filled curve, in a
    /// 100 x 100 viewbox with the baseline at the bottom
    pub fn curve_path(&self) -> String {
        curve_path(&self.bars)
    }
}

/// Smooth filled curve through `levels`, spread across a 100 x 100 viewbox.
/// Uses quadratic segments through the midpoints between bands.
pub fn curve_path(levels: &[f32]) -> String {
    use std::fmt::Write as _;

    let n = levels.len();
    if n == 0 {
        return String::new();
    }
    let point = |i: usize| {
        let x = if n == 1 {
            50.0
        } else {
            i as f32 * 100.0 / (n - 1) as f32
        };
        let y = 100.0 - levels[i].clamp(0.0, 1.0) * 100.0;
        (x, y)
    };

    let (x0, y0) = point(0);
    let mut path = format!("M 0 100 L {x0:.1} {y0:.1}");
    for i in 1..n {
        let (px, py) = point(i - 1);
        let (x, y) = point(i);
        let (mx, my) = ((px + x) / 2.0, (py + y) / 2.0);
        let _ = write!(path, " Q {px:.1} {py:.1} {mx:.1} {my:.1}");
        if i == n - 1 {
            let _ = write!(path, " L {x:.1} {y:.1}");
        }
    }
    path.push_str(" L 100 100 Z");
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_jump_up_immediately() {
        let mut d = SpectrumDisplay::new(2);
        d.update(&[0.8, 0.2]);
        assert_eq!(d.bars(), &[0.8, 0.2]);
    }

    #[test]
    fn bars_ease_down() {
        let mut d = SpectrumDisplay::new(1);
        d.update(&[1.0]);
        d.update(&[0.0]);
        assert!((d.bars()[0] - BAR_RELEASE).abs() < 1e-6);
    }

    #[test]
    fn peak_holds_then_falls() {
        let mut d = SpectrumDisplay::new(1);
        d.update(&[1.0]);
        for _ in 0..PEAK_HOLD_FRAMES {
            d.update(&[0.0]);
            assert_eq!(d.peaks()[0], 1.0);
        }
        d.update(&[0.0]);
        assert!(d.peaks()[0] < 1.0);
        assert!(d.peaks()[0] >= d.bars()[0]);
    }

    #[test]
    fn peak_never_below_bar() {
        let mut d = SpectrumDisplay::new(1);
        d.update(&[0.5]);
        for _ in 0..200 {
            d.update(&[0.3]);
            assert!(d.peaks()[0] >= d.bars()[0]);
        }
    }

    #[test]
    fn levels_are_clamped() {
        let mut d = SpectrumDisplay::new(2);
        d.update(&[5.0, -1.0]);
        assert_eq!(d.bars(), &[1.0, 0.0]);
    }

    #[test]
    fn reset_zeroes_everything() {
        let mut d = SpectrumDisplay::new(2);
        d.update(&[0.9, 0.9]);
        d.reset();
        assert_eq!(d.bars(), &[0.0, 0.0]);
        assert_eq!(d.peaks(), &[0.0, 0.0]);
    }

    #[test]
    fn curve_path_is_closed_and_spans_viewbox() {
        let p = curve_path(&[0.0, 1.0, 0.5]);
        assert!(p.starts_with("M 0 100 L 0.0 100.0"));
        assert!(p.contains("L 100.0 50.0"));
        assert!(p.ends_with("L 100 100 Z"));
    }

    #[test]
    fn curve_path_empty() {
        assert_eq!(curve_path(&[]), "");
    }
}
