//! Channels and balance: stereo as it comes, both channels mixed to mono,
//! or left and right swapped, then the left/right balance. Only for
//! two-channel audio; mono and surround streams pass as they are.

use std::num::NonZero;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rodio::Source;

use super::sound::{Channels, SharedSoundParams};

/// How long a change of setting takes to glide in, so it doesn't click
const GLIDE_SECS: f32 = 0.02;
/// How long the left/right correlation is averaged over
const CORRELATION_SECS: f32 = 1.0;
/// Mono mixes the difference instead of the sum below this correlation
/// (one channel's polarity flipped, which a sum would cancel), and goes
/// back above the second
const FLIPPED_BELOW: f32 = -0.5;
const UNFLIPPED_ABOVE: f32 = -0.2;

/// Where each output channel takes its sound from:
/// [left from left, left from right, right from left, right from right]
type Matrix = [f32; 4];

fn matrix(channels: Channels, balance: f32, flipped: bool) -> Matrix {
    let m = match channels {
        Channels::Stereo => [1.0, 0.0, 0.0, 1.0],
        Channels::Swap => [0.0, 1.0, 1.0, 0.0],
        Channels::Mono if flipped => [0.5, -0.5, 0.5, -0.5],
        Channels::Mono => [0.5, 0.5, 0.5, 0.5],
    };
    // Balance only ever turns a side down
    let left = if balance > 0.0 { 1.0 - balance } else { 1.0 };
    let right = if balance < 0.0 { 1.0 + balance } else { 1.0 };
    [m[0] * left, m[1] * left, m[2] * right, m[3] * right]
}

/// A `Source` adapter that applies the Channels and Balance settings
pub struct ChannelSource<S> {
    inner: S,
    params: SharedSoundParams,
    changes: Arc<AtomicU64>,
    seen: u64,
    setting: Channels,
    balance: f32,
    /// The matrix in use now, and the one it glides to
    now: Matrix,
    goal: Matrix,
    glide: f32,
    /// Running averages of L*R, L*L and R*R
    lr: f32,
    ll: f32,
    rr: f32,
    smooth: f32,
    flipped: bool,
    channels: u16,
    sample_rate: u32,
    /// The right channel's output, while the left is being returned
    pending: Option<f32>,
    /// Samples into a frame of audio that isn't stereo
    passthrough_index: usize,
}

impl<S> ChannelSource<S>
where
    S: Source<Item = f32>,
{
    pub fn new(inner: S, params: SharedSoundParams) -> Self {
        let channels = inner.channels().get();
        let sample_rate = inner.sample_rate().get();
        let (changes, seen, setting, balance) = {
            let p = params.lock().unwrap_or_else(|e| e.into_inner());
            (p.changes(), p.generation(), p.channels(), p.balance())
        };
        let goal = matrix(setting, balance, false);
        let mut s = Self {
            inner,
            params,
            changes,
            seen,
            setting,
            balance,
            now: goal,
            goal,
            glide: 0.0,
            lr: 0.0,
            ll: 0.0,
            rr: 0.0,
            smooth: 0.0,
            flipped: false,
            channels,
            sample_rate,
            pending: None,
            passthrough_index: 0,
        };
        s.set_rate(sample_rate);
        s
    }

    fn set_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1) as f32;
        self.glide = 1.0 - (-1.0 / (GLIDE_SECS * rate)).exp();
        self.smooth = 1.0 - (-1.0 / (CORRELATION_SECS * rate)).exp();
    }

    fn update_goal(&mut self) {
        self.goal = matrix(self.setting, self.balance, self.flipped);
    }

    fn reread_params(&mut self) {
        let p = self.params.lock().unwrap_or_else(|e| e.into_inner());
        self.seen = p.generation();
        self.setting = p.channels();
        self.balance = p.balance();
        drop(p);
        self.update_goal();
    }

    /// Track whether one channel's polarity is flipped, for Mono
    fn correlate(&mut self, l: f32, r: f32) {
        let k = self.smooth;
        self.lr += (l * r - self.lr) * k;
        self.ll += (l * l - self.ll) * k;
        self.rr += (r * r - self.rr) * k;
        let power = (self.ll * self.rr).sqrt();
        if power < 1e-8 {
            return;
        }
        let corr = self.lr / power;
        let flipped = if self.flipped {
            corr < UNFLIPPED_ABOVE
        } else {
            corr < FLIPPED_BELOW
        };
        if flipped != self.flipped {
            self.flipped = flipped;
            self.update_goal();
        }
    }

    /// Both outputs of one stereo frame
    fn frame(&mut self, l: f32, r: f32) -> (f32, f32) {
        if self.changes.load(Ordering::Acquire) != self.seen {
            self.reread_params();
        }
        if self.setting == Channels::Mono && l.is_finite() && r.is_finite() {
            self.correlate(l, r);
        }
        for (now, goal) in self.now.iter_mut().zip(self.goal) {
            *now += (goal - *now) * self.glide;
            // Close enough (single precision stalls short of the goal)
            if (goal - *now).abs() < 1e-4 {
                *now = goal;
            }
        }
        let m = self.now;
        (m[0] * l + m[1] * r, m[2] * l + m[3] * r)
    }
}

impl<S> Iterator for ChannelSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if let Some(r) = self.pending.take() {
            return Some(r);
        }
        let first = self.inner.next()?;
        // The format can change between frames (see EqSource)
        if self.passthrough_index == 0 {
            let channels = self.inner.channels().get();
            let sample_rate = self.inner.sample_rate().get();
            if sample_rate != self.sample_rate {
                self.sample_rate = sample_rate;
                self.set_rate(sample_rate);
            }
            self.channels = channels;
        }
        if self.channels != 2 {
            self.passthrough_index = (self.passthrough_index + 1) % usize::from(self.channels);
            return Some(first);
        }
        let Some(second) = self.inner.next() else {
            return Some(first);
        };
        let (l, r) = self.frame(first, second);
        self.pending = Some(r);
        Some(l)
    }
}

impl<S> Source for ChannelSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        // Counts from the inner source; a right sample may be held here
        self.inner
            .current_span_len()
            .map(|n| n + usize::from(self.pending.is_some()))
    }

    fn channels(&self) -> NonZero<u16> {
        if self.pending.is_some() {
            NonZero::new(2).unwrap()
        } else {
            self.inner.channels()
        }
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
    use super::super::sound::SoundParams;
    use super::*;
    use rodio::buffer::SamplesBuffer;

    fn run(params: &SharedSoundParams, channels: u16, samples: Vec<f32>) -> Vec<f32> {
        ChannelSource::new(
            SamplesBuffer::new(
                NonZero::new(channels).unwrap(),
                NonZero::new(48_000).unwrap(),
                samples,
            ),
            params.clone(),
        )
        .collect()
    }

    /// Left 0.4, right 0.1, for a second
    fn uneven() -> Vec<f32> {
        (0..48_000).flat_map(|_| [0.4, 0.1]).collect()
    }

    fn last_frame(out: &[f32]) -> (f32, f32) {
        (out[out.len() - 2], out[out.len() - 1])
    }

    #[test]
    fn stereo_passes_unchanged() {
        let params = SoundParams::new_shared();
        let input = uneven();
        assert_eq!(run(&params, 2, input.clone()), input);
    }

    #[test]
    fn mono_mixes_both_sides() {
        let params = SoundParams::new_shared();
        params.lock().unwrap().set_channels(Channels::Mono);
        let (l, r) = last_frame(&run(&params, 2, uneven()));
        assert!(
            (l - 0.25).abs() < 1e-5 && (r - 0.25).abs() < 1e-5,
            "{l} {r}"
        );
    }

    #[test]
    fn mono_keeps_a_flipped_channel_from_cancelling() {
        let params = SoundParams::new_shared();
        params.lock().unwrap().set_channels(Channels::Mono);
        // The right channel is the left upside down
        let input: Vec<f32> = (0..96_000)
            .flat_map(|i| {
                let v = 0.5 * (2.0 * std::f32::consts::PI * 300.0 * i as f32 / 48_000.0).sin();
                [v, -v]
            })
            .collect();
        let out = run(&params, 2, input);
        let tail = &out[out.len() - 4_800..];
        let peak = tail.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.45, "cancelled: {peak}");
    }

    #[test]
    fn swap_trades_sides() {
        let params = SoundParams::new_shared();
        params.lock().unwrap().set_channels(Channels::Swap);
        assert_eq!(last_frame(&run(&params, 2, uneven())), (0.1, 0.4));
    }

    #[test]
    fn balance_turns_the_other_side_down() {
        let params = SoundParams::new_shared();
        params.lock().unwrap().set_balance(0.5);
        let (l, r) = last_frame(&run(&params, 2, uneven()));
        assert!((l - 0.2).abs() < 1e-6 && (r - 0.1).abs() < 1e-6, "{l} {r}");
        params.lock().unwrap().set_balance(-1.0);
        let (l, r) = last_frame(&run(&params, 2, uneven()));
        assert!((l - 0.4).abs() < 1e-6 && r == 0.0, "{l} {r}");
    }

    #[test]
    fn a_change_glides_in() {
        let params = SoundParams::new_shared();
        let mut s = ChannelSource::new(
            SamplesBuffer::new(
                NonZero::new(2).unwrap(),
                NonZero::new(48_000).unwrap(),
                uneven(),
            ),
            params.clone(),
        );
        for _ in 0..1_000 {
            s.next();
        }
        params.lock().unwrap().set_channels(Channels::Swap);
        let l = s.next().unwrap();
        assert!(l > 0.39, "jumped: {l}");
        let out: Vec<f32> = s.collect();
        assert_eq!(last_frame(&out), (0.1, 0.4));
    }

    #[test]
    fn other_layouts_pass_unchanged() {
        let params = SoundParams::new_shared();
        params.lock().unwrap().set_channels(Channels::Swap);
        let mono = vec![0.3; 1_000];
        assert_eq!(run(&params, 1, mono.clone()), mono);
        let six: Vec<f32> = (0..600).map(|i| i as f32 / 600.0).collect();
        assert_eq!(run(&params, 6, six.clone()), six);
    }
}
