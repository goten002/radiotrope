//! The last step before the device, run in the audio callback: the volume,
//! the fade-out when another station starts, and dither for devices that
//! take 16-bit samples. Lock-free and allocation-free, as the audio thread
//! must be.

use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rodio::Source;

use crate::config::sound::FADE_OUT_MS;

/// How long a volume change takes to glide in, so a dragged slider
/// doesn't crackle
const VOLUME_GLIDE_SECS: f32 = 0.01;
/// One step of a 16-bit sample
const LSB_16: f32 = 1.0 / 32_768.0;

/// The volume every station's output plays at, as set by the engine
#[derive(Debug)]
pub(crate) struct SharedVolume(AtomicU32);

impl SharedVolume {
    pub(crate) fn new(gain: f32) -> Arc<Self> {
        Arc::new(Self(AtomicU32::new(gain.to_bits())))
    }

    /// Linear gain
    pub(crate) fn set(&self, gain: f32) {
        self.0.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// A `Source` adapter for a station's audio on its way to the device
pub(crate) struct OutputStage<S> {
    inner: S,
    volume: Arc<SharedVolume>,
    gain: f32,
    glide: f32,
    rate: u32,
    /// Set when the station should fade out and end
    fade: Arc<AtomicBool>,
    /// Samples of the fade left, and in all, once it started
    fade_left: usize,
    fade_total: usize,
    dither: bool,
    rng: u32,
}

impl<S> OutputStage<S>
where
    S: Source<Item = f32>,
{
    /// `dither` for a device that takes 16-bit samples
    pub(crate) fn new(
        inner: S,
        volume: Arc<SharedVolume>,
        fade: Arc<AtomicBool>,
        dither: bool,
    ) -> Self {
        let rate = inner.sample_rate().get();
        Self {
            gain: volume.get(),
            inner,
            volume,
            glide: glide(rate),
            rate,
            fade,
            fade_left: 0,
            fade_total: 0,
            dither,
            rng: 0x9E37_79B9,
        }
    }

    /// Uniform in 0..1 (xorshift: cheap, and plenty for dither)
    fn random(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }
}

fn glide(rate: u32) -> f32 {
    1.0 - (-1.0 / (VOLUME_GLIDE_SECS * rate.max(1) as f32)).exp()
}

impl<S> Iterator for OutputStage<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.fade_total == 0 && self.fade.load(Ordering::Relaxed) {
            let frames = (u64::from(self.rate) * FADE_OUT_MS / 1000).max(1) as usize;
            self.fade_total = frames * usize::from(self.inner.channels().get());
            self.fade_left = self.fade_total;
        }
        let fade = if self.fade_total > 0 {
            if self.fade_left == 0 {
                return None;
            }
            self.fade_left -= 1;
            // Smoothstep down: no corner at either end
            let t = self.fade_left as f32 / self.fade_total as f32;
            t * t * (3.0 - 2.0 * t)
        } else {
            1.0
        };

        let x = self.inner.next()?;
        let rate = self.inner.sample_rate().get();
        if rate != self.rate {
            self.rate = rate;
            self.glide = glide(rate);
        }
        let target = self.volume.get();
        self.gain += (target - self.gain) * self.glide;
        if (target - self.gain).abs() < 1e-5 {
            self.gain = target;
        }
        let mut y = x * self.gain * fade;
        if self.dither && y != 0.0 {
            // Triangular dither of one step: the rounding to 16 bits
            // becomes a steady hiss far down instead of distortion that
            // follows the music. Exact silence stays silent.
            y += (self.random() - self.random()) * LSB_16;
        }
        Some(y)
    }
}

impl<S> Source for OutputStage<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> NonZero<u16> {
        self.inner.channels()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

    fn stage(samples: Vec<f32>, volume: f32, dither: bool) -> OutputStage<SamplesBuffer> {
        OutputStage::new(
            SamplesBuffer::new(
                NonZero::new(2).unwrap(),
                NonZero::new(48_000).unwrap(),
                samples,
            ),
            SharedVolume::new(volume),
            Arc::new(AtomicBool::new(false)),
            dither,
        )
    }

    #[test]
    fn plays_at_the_volume() {
        let out: Vec<f32> = stage(vec![0.5; 1_000], 0.25, false).collect();
        assert!(out.iter().all(|&x| x == 0.125));
    }

    #[test]
    fn a_volume_change_glides() {
        let mut s = stage(vec![0.5; 96_000], 1.0, false);
        for _ in 0..1_000 {
            s.next();
        }
        s.volume.set(0.0);
        let x = s.next().unwrap();
        assert!(x > 0.45, "jumped: {x}");
        let rest: Vec<f32> = s.collect();
        assert_eq!(*rest.last().unwrap(), 0.0);
    }

    #[test]
    fn fades_out_and_ends() {
        let mut s = stage(vec![0.5; 96_000 * 2], 1.0, false);
        for _ in 0..1_000 {
            s.next();
        }
        s.fade.store(true, Ordering::Relaxed);
        let rest: Vec<f32> = s.collect();
        let fade = (48_000 * FADE_OUT_MS / 1000) as usize * 2;
        assert_eq!(rest.len(), fade);
        assert!(rest[0] > 0.49);
        assert!(rest[fade / 2] > 0.2 && rest[fade / 2] < 0.3);
        assert!(*rest.last().unwrap() < 0.001);
        // Gently: no step bigger than a smooth fade takes
        assert!(rest.windows(2).all(|w| (w[0] - w[1]).abs() < 0.001));
    }

    #[test]
    fn dither_is_one_step_and_keeps_silence() {
        let out: Vec<f32> = stage(vec![0.5; 20_000], 1.0, true).collect();
        assert!(out.iter().all(|&x| (x - 0.5).abs() <= LSB_16));
        let spread = out.iter().filter(|&&x| x != 0.5).count();
        assert!(spread > 19_000, "no dither: {spread}");
        let mean = out.iter().map(|&x| x - 0.5).sum::<f32>() / out.len() as f32;
        assert!(mean.abs() < LSB_16 * 0.05, "{mean}");
        let silent: Vec<f32> = stage(vec![0.0; 1_000], 1.0, true).collect();
        assert!(silent.iter().all(|&x| x == 0.0));
    }
}
