//! A biquad filter in double precision, for filters tuned far below the
//! sample rate (25 Hz at 48 kHz), where single precision drifts

/// One second-order section (direct form I), coefficients normalised so
/// a0 is 1
#[derive(Clone, Copy, Debug)]
pub(crate) struct Biquad64 {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad64 {
    /// From b0..b2 and a1, a2 (a0 already divided out)
    pub(crate) fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self {
            b,
            a,
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    /// A Butterworth high-pass at `freq`
    pub(crate) fn high_pass(freq: f64, sample_rate: f64) -> Self {
        let w = 2.0 * std::f64::consts::PI * (freq / sample_rate).min(0.49);
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self::new(
            [
                (1.0 + cos) / 2.0 / a0,
                -(1.0 + cos) / a0,
                (1.0 + cos) / 2.0 / a0,
            ],
            [-2.0 * cos / a0, (1.0 - alpha) / a0],
        )
    }

    pub(crate) fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        // Values this small slow some CPUs down (denormals) and are silence
        self.y = [if y.abs() < 1e-25 { 0.0 } else { y }, self.y[0]];
        self.y[0]
    }

    pub(crate) fn reset(&mut self) {
        self.x = [0.0; 2];
        self.y = [0.0; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain_at(filter: Biquad64, freq: f64, rate: f64) -> f64 {
        let mut f = filter;
        let n = (rate * 2.0) as usize;
        let mut peak = 0.0f64;
        for i in 0..n {
            let y = f.run((2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin());
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn high_pass_keeps_bass_and_drops_rumble() {
        let hp = Biquad64::high_pass(25.0, 48_000.0);
        assert!(gain_at(hp, 100.0, 48_000.0) > 0.97);
        assert!(gain_at(hp, 1_000.0, 48_000.0) > 0.999);
        assert!(gain_at(hp, 8.0, 48_000.0) < 0.15);
    }

    #[test]
    fn high_pass_removes_dc() {
        let mut hp = Biquad64::high_pass(25.0, 44_100.0);
        let mut last = 1.0;
        for _ in 0..44_100 {
            last = hp.run(0.5);
        }
        assert!(last.abs() < 1e-4, "{last}");
    }
}
