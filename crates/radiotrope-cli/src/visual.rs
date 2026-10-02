//! The spectrum's movement, as the app's visualizer moves: quiet bands
//! drop to nothing, bars rise quickly and fall gently

use std::time::Instant;

use radiotrope::audio::AudioAnalysis;

/// Columns in the spectrum
pub const COLUMNS: usize = 14;

/// Levels at or below this are noise (the app's `visual::gate`)
const NOISE_FLOOR: f32 = 0.08;
/// Time a rising bar takes to cover about two thirds of the way (seconds)
const RISE_SECS: f32 = 0.05;
/// Time a falling bar takes to drop about two thirds of the way (seconds)
const FALL_SECS: f32 = 0.25;
/// A level below this is shown as zero
const SETTLED: f32 = 0.001;

pub struct Spectrum {
    levels: [f32; COLUMNS],
    last: Option<Instant>,
}

impl Spectrum {
    pub fn new() -> Self {
        Self {
            levels: [0.0; COLUMNS],
            last: None,
        }
    }

    pub fn levels(&self) -> &[f32; COLUMNS] {
        &self.levels
    }

    /// Nothing to draw and nothing moving
    pub fn is_still(&self) -> bool {
        self.levels.iter().all(|&l| l == 0.0)
    }

    /// Move towards the engine's spectrum, or to rest when nothing plays
    pub fn update(&mut self, analysis: Option<&mut AudioAnalysis>) {
        let now = Instant::now();
        let dt = self
            .last
            .map_or(0.033, |t| now.duration_since(t).as_secs_f32())
            .min(0.25);
        self.last = Some(now);

        let targets = match analysis {
            Some(a) => {
                // The spectrum is on screen: keep it worked out
                a.mark_shown();
                columns(&a.spectrum)
            }
            None => [0.0; COLUMNS],
        };
        self.advance(&targets, dt);
    }

    fn advance(&mut self, targets: &[f32; COLUMNS], dt: f32) {
        let rise = 1.0 - (-dt / RISE_SECS).exp();
        let fall = 1.0 - (-dt / FALL_SECS).exp();
        for (level, &target) in self.levels.iter_mut().zip(targets) {
            let k = if target > *level { rise } else { fall };
            *level += (target - *level) * k;
            if *level < SETTLED {
                *level = 0.0;
            }
        }
    }
}

/// The engine's bands spread over [`COLUMNS`] columns, noise removed
fn columns(bands: &[f32]) -> [f32; COLUMNS] {
    let mut out = [0.0; COLUMNS];
    if bands.is_empty() {
        return out;
    }
    for (i, column) in out.iter_mut().enumerate() {
        // The band under this column's centre
        let pos = (i as f32 + 0.5) * bands.len() as f32 / COLUMNS as f32;
        let band = bands[(pos as usize).min(bands.len() - 1)];
        *column = gate(band);
    }
    out
}

/// Remove the noise floor and stretch what is left back to 0-1
fn gate(level: f32) -> f32 {
    ((level.clamp(0.0, 1.0) - NOISE_FLOOR) / (1.0 - NOISE_FLOOR)).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_bands_drop_to_nothing() {
        assert_eq!(gate(0.05), 0.0);
        assert_eq!(gate(1.0), 1.0);
        assert!(gate(0.5) > 0.4);
    }

    #[test]
    fn bars_rise_fast_and_settle_at_rest() {
        let mut s = Spectrum::new();
        s.advance(&[1.0; COLUMNS], 0.1);
        assert!(s.levels()[0] > 0.8);
        for _ in 0..100 {
            s.advance(&[0.0; COLUMNS], 0.1);
        }
        assert!(s.is_still());
    }

    #[test]
    fn sixteen_bands_spread_over_the_columns() {
        let bands: Vec<f32> = (0..16).map(|i| i as f32 / 15.0).collect();
        let c = columns(&bands);
        assert_eq!(c[0], 0.0);
        assert!(c[COLUMNS - 1] > 0.9);
        assert!(c.windows(2).all(|w| w[0] <= w[1]));
    }
}
