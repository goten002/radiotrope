//! Visualizer display state
//!
//! Turns the analyzer's spectrum into what the visualizer draws: bars that
//! ease down between frames, peak dots that hold briefly and then fall, and
//! a noise gate so quiet bands (like the empty top of a low-passed MP3)
//! drop to nothing instead of sitting as frozen stubs.

/// Share of the gap a falling bar closes per frame (higher = slower fall)
const BAR_RELEASE: f32 = 0.82;
/// Frames a peak dot stays put before falling
const PEAK_HOLD_FRAMES: u32 = 12;
/// How far a peak dot falls per frame once released
const PEAK_FALL: f32 = 0.025;
/// Levels at or below this are noise; the rest is rescaled to 0-1
const NOISE_FLOOR: f32 = 0.08;

/// Remove the noise floor and stretch what is left back to 0-1
fn gate(level: f32) -> f32 {
    ((level.clamp(0.0, 1.0) - NOISE_FLOOR) / (1.0 - NOISE_FLOOR)).max(0.0)
}

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
            let level = gate(level);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_jump_up_immediately() {
        let mut d = SpectrumDisplay::new(2);
        d.update(&[1.0, 0.54]);
        assert_eq!(d.bars()[0], 1.0);
        assert!((d.bars()[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn noise_floor_is_zero() {
        let mut d = SpectrumDisplay::new(3);
        d.update(&[0.0, 0.03, NOISE_FLOOR]);
        assert_eq!(d.bars(), &[0.0, 0.0, 0.0]);
        assert_eq!(d.peaks(), &[0.0, 0.0, 0.0]);
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
        d.update(&[0.6]);
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
}
