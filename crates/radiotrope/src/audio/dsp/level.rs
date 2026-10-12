//! Station volume matching: measures how loud the station is (ITU-R
//! BS.1770 loudness, the measure broadcasters and streaming services use)
//! and turns it up or down toward the chosen level.
//!
//! The station's loudness is measured over the last minute of sound, with
//! the quiet parts gated out as BS.1770 does, so a talk break or a quiet
//! song doesn't pump the gain. The gain moves slowly: down faster than up,
//! so a loud ad is caught before a quiet intro is pushed up. Silence holds
//! it still. A station heard before starts at the gain its remembered
//! loudness calls for; a new one starts turned down and is learned.
//!
//! The new station also fades in here.

use std::collections::VecDeque;
use std::num::NonZero;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rodio::Source;

use super::filter::Biquad64;
use super::sound::{LevelPhase, LevelReadout, SharedSoundParams};
use crate::config::sound::*;

/// Channels measured; more are turned up or down but not measured
const MAX_CHANNELS: usize = 8;
/// The meter's step (BS.1770's 400 ms blocks overlap by 75%)
const SUB_BLOCK_SECS: f32 = 0.1;
/// Sub-blocks in a momentary (400 ms) and a short-term (3 s) block
const MOMENTARY_SUBS: usize = 4;
const SHORT_SUBS: usize = 30;
/// Sound needed before a new station's loudness is used at all (blocks)
const MIN_MEASURED_BLOCKS: usize = 10;

/// Loudness in LUFS from a mean weighted energy
fn lufs(energy: f64) -> f32 {
    if energy <= 0.0 {
        return f32::NEG_INFINITY;
    }
    (-0.691 + 10.0 * energy.log10()) as f32
}

fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// BS.1770's K-weighting for one channel: a high shelf for the head's
/// effect, then a high-pass. Coefficients worked out for any sample rate
/// (as libebur128 does), so 22.05 kHz and 96 kHz streams measure right.
fn k_weighting(sample_rate: f64) -> [Biquad64; 2] {
    let pre = {
        let f0 = 1_681.974_450_955_533;
        let g = 3.999_843_853_973_347;
        let q = 0.707_175_236_955_419_6;
        let k = (std::f64::consts::PI * (f0 / sample_rate).min(0.49)).tan();
        let vh = 10f64.powf(g / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / q + k * k;
        Biquad64::new(
            [
                (vh + vb * k / q + k * k) / a0,
                2.0 * (k * k - vh) / a0,
                (vh - vb * k / q + k * k) / a0,
            ],
            [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        )
    };
    let rlb = {
        let f0 = 38.135_470_876_024_44;
        let q = 0.500_327_037_323_877_3;
        let k = (std::f64::consts::PI * (f0 / sample_rate).min(0.49)).tan();
        let a0 = 1.0 + k / q + k * k;
        Biquad64::new(
            [1.0, -2.0, 1.0],
            [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        )
    };
    [pre, rlb]
}

/// BS.1770's channel weights: the LFE doesn't count, the surrounds count
/// a little more (5.1 order: L R C LFE Ls Rs)
fn channel_weight(channel: usize, channels: usize) -> f64 {
    if channels < 6 {
        return 1.0;
    }
    match channel {
        3 => 0.0,
        4 | 5 => 1.41,
        _ => 1.0,
    }
}

/// K-weighted energy in 100 ms steps
struct Meter {
    filters: Vec<[Biquad64; 2]>,
    weights: Vec<f64>,
    frames_per_sub: usize,
    /// Weighted sum of squares so far in this sub-block, and its frames
    acc: f64,
    frames: usize,
    /// Mean weighted energy of the last sub-blocks, newest last
    subs: VecDeque<f64>,
}

impl Meter {
    fn new(channels: usize, sample_rate: u32) -> Self {
        let measured = channels.min(MAX_CHANNELS);
        Self {
            filters: (0..measured)
                .map(|_| k_weighting(f64::from(sample_rate.max(1))))
                .collect(),
            weights: (0..measured).map(|c| channel_weight(c, channels)).collect(),
            frames_per_sub: ((sample_rate.max(1) as f32 * SUB_BLOCK_SECS) as usize).max(1),
            acc: 0.0,
            frames: 0,
            subs: VecDeque::with_capacity(SHORT_SUBS + 1),
        }
    }

    /// Start over in a new format, keeping what was measured so far
    fn set_format(&mut self, channels: usize, sample_rate: u32) {
        let subs = std::mem::take(&mut self.subs);
        *self = Self::new(channels, sample_rate);
        self.subs = subs;
    }

    fn sample(&mut self, channel: usize, x: f32) {
        let Some(filters) = self.filters.get_mut(channel) else {
            return;
        };
        let x = if x.is_finite() { f64::from(x) } else { 0.0 };
        let shelved = filters[0].run(x);
        let y = filters[1].run(shelved);
        if !y.is_finite() {
            filters.iter_mut().for_each(Biquad64::reset);
            return;
        }
        self.acc += self.weights[channel] * y * y;
    }

    /// At each frame's end; true when a sub-block is complete
    fn end_frame(&mut self) -> bool {
        self.frames += 1;
        if self.frames < self.frames_per_sub {
            return false;
        }
        self.subs.push_back(self.acc / self.frames as f64);
        if self.subs.len() > SHORT_SUBS {
            self.subs.pop_front();
        }
        self.acc = 0.0;
        self.frames = 0;
        true
    }

    fn mean_of_last(&self, n: usize) -> Option<f64> {
        if self.subs.len() < n {
            return None;
        }
        Some(self.subs.iter().rev().take(n).sum::<f64>() / n as f64)
    }

    /// The last 400 ms, in LUFS
    fn momentary(&self) -> Option<f32> {
        self.mean_of_last(MOMENTARY_SUBS).map(lufs)
    }

    /// The last 3 s, in LUFS
    fn short_term(&self) -> Option<f32> {
        self.mean_of_last(SHORT_SUBS).map(lufs)
    }
}

/// BS.1770's gated loudness over the momentary blocks of the last minute
/// of sound
struct Window {
    blocks: VecDeque<f64>,
    cap: usize,
}

impl Window {
    fn new() -> Self {
        let cap = (WINDOW_SECS / SUB_BLOCK_SECS) as usize;
        Self {
            blocks: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn push(&mut self, energy: f64) {
        if self.blocks.len() == self.cap {
            self.blocks.pop_front();
        }
        self.blocks.push_back(energy);
    }

    fn len(&self) -> usize {
        self.blocks.len()
    }

    fn loudness(&self) -> Option<f32> {
        let mean_above = |gate: f32| {
            let (sum, n) = self
                .blocks
                .iter()
                .filter(|&&e| lufs(e) > gate)
                .fold((0.0, 0usize), |(s, n), &e| (s + e, n + 1));
            (n > 0).then(|| sum / n as f64)
        };
        let ungated = mean_above(-70.0)?;
        mean_above(lufs(ungated) - 10.0).map(lufs)
    }
}

/// Decides the gain from the measurements, once per sub-block
struct Controller {
    readout: Arc<LevelReadout>,
    /// This station's claim on the readout
    owner: u64,
    window: Window,
    /// The loudness remembered for this station, if it was heard before
    start: Option<f32>,
    /// Sub-blocks with sound heard
    sound_blocks: usize,
    gain_db: f32,
}

impl Controller {
    fn new(readout: Arc<LevelReadout>, start: Option<f32>, on: bool, target: f32) -> Self {
        let start = start.filter(|l| l.is_finite());
        let gain_db = match (on, start) {
            (false, _) => 0.0,
            (true, Some(l)) => (target - l).clamp(MIN_GAIN_DB, MAX_GAIN_DB),
            (true, None) => UNKNOWN_START_DB,
        };
        let c = Self {
            owner: readout.claim(),
            readout,
            window: Window::new(),
            start,
            sound_blocks: 0,
            gain_db,
        };
        c.publish();
        c
    }

    fn sound_secs(&self) -> f32 {
        self.sound_blocks as f32 * SUB_BLOCK_SECS
    }

    /// Still finding out how loud a new station is
    fn learning(&self) -> bool {
        self.start.is_none() && self.sound_secs() < SETTLE_SECS
    }

    /// The station's loudness as best known now
    fn station_loudness(&self) -> Option<f32> {
        let measured = (self.window.len() >= MIN_MEASURED_BLOCKS)
            .then(|| self.window.loudness())
            .flatten();
        if self.sound_secs() >= SETTLE_SECS {
            measured.or(self.start)
        } else {
            self.start.or(measured)
        }
    }

    fn publish(&self) {
        let phase = if self.learning() {
            LevelPhase::Learning
        } else {
            LevelPhase::Settled
        };
        self.readout
            .set_if(self.owner, phase, self.gain_db, self.station_loudness());
    }

    /// One sub-block on; returns the gain to reach by the end of the next
    fn step(&mut self, meter: &Meter, on: bool, target: f32) -> f32 {
        let momentary = meter.momentary();
        let sound = momentary.is_some_and(|m| m >= SILENCE_LUFS);
        if sound {
            if let Some(e) = meter.mean_of_last(MOMENTARY_SUBS) {
                self.window.push(e);
            }
            self.sound_blocks += 1;
        }

        let mut goal = if !on {
            0.0
        } else if !sound {
            // Dead air or a quiet moment: hold still
            self.gain_db
        } else if let Some(l) = self.station_loudness() {
            (target - l).clamp(MIN_GAIN_DB, MAX_GAIN_DB)
        } else {
            self.gain_db
        };

        let mut rate = if !on || self.learning() {
            LEARNING_DB_PER_SEC
        } else if goal < self.gain_db {
            DOWN_DB_PER_SEC
        } else {
            UP_DB_PER_SEC
        };
        // Far too loud right now (a blaring ad): down fast
        if on {
            if let Some(short) = meter.short_term() {
                if short + self.gain_db > target + PANIC_LU {
                    goal = goal.min(target + PANIC_LU - short);
                    rate = rate.max(PANIC_DB_PER_SEC);
                }
            }
        }

        let max_step = rate * SUB_BLOCK_SECS;
        self.gain_db += (goal - self.gain_db).clamp(-max_step, max_step);
        self.publish();
        self.gain_db
    }
}

/// A `Source` adapter that matches the station's volume (see the module
/// docs) and fades it in
pub struct LevelSource<S> {
    inner: S,
    params: SharedSoundParams,
    changes: Arc<AtomicU64>,
    seen: u64,
    on: bool,
    target: f32,
    meter: Meter,
    controller: Controller,
    channels: u16,
    sample_rate: u32,
    channel_index: usize,
    /// Gain applied now (linear), and how much it moves each frame
    applied: f32,
    step: f32,
    /// Where `applied` lands at the end of this sub-block
    goal: f32,
    /// Frames played, for the fade-in
    played: u64,
    fade_frames: u64,
}

impl<S> LevelSource<S>
where
    S: Source<Item = f32>,
{
    /// `start_lufs` is the station's remembered loudness, if it was heard
    /// before. `readout` shows what matching is doing.
    pub fn new(
        inner: S,
        params: SharedSoundParams,
        readout: Arc<LevelReadout>,
        start_lufs: Option<f32>,
    ) -> Self {
        let channels = inner.channels().get();
        let sample_rate = inner.sample_rate().get();
        let (changes, seen, on, target) = {
            let p = params.lock().unwrap_or_else(|e| e.into_inner());
            (
                p.changes(),
                p.generation(),
                p.match_volume(),
                p.target().lufs(),
            )
        };
        let controller = Controller::new(readout, start_lufs, on, target);
        let gain = db_to_linear(controller.gain_db);
        Self {
            inner,
            params,
            changes,
            seen,
            on,
            target,
            meter: Meter::new(usize::from(channels), sample_rate),
            controller,
            channels,
            sample_rate,
            channel_index: 0,
            applied: gain,
            step: 0.0,
            goal: gain,
            played: 0,
            fade_frames: Self::fade_frames(sample_rate),
        }
    }

    fn fade_frames(sample_rate: u32) -> u64 {
        ((FADE_IN_MS / 1000.0 * sample_rate as f32) as u64).max(1)
    }

    fn reread_params(&mut self) {
        let p = self.params.lock().unwrap_or_else(|e| e.into_inner());
        self.seen = p.generation();
        self.on = p.match_volume();
        self.target = p.target().lufs();
    }

    /// Once per frame, before its first sample
    fn start_frame(&mut self) {
        let channels = self.inner.channels().get();
        let sample_rate = self.inner.sample_rate().get();
        if channels != self.channels || sample_rate != self.sample_rate {
            self.channels = channels;
            self.sample_rate = sample_rate;
            self.meter.set_format(usize::from(channels), sample_rate);
            self.fade_frames = Self::fade_frames(sample_rate);
            // Mid sub-block: land on the goal now
            self.applied = self.goal;
            self.step = 0.0;
        }
        if self.changes.load(Ordering::Acquire) != self.seen {
            self.reread_params();
        }
        self.applied += self.step;
    }

    fn end_frame(&mut self) {
        self.played += 1;
        if self.meter.end_frame() {
            // Exactly on the goal, then on to the next
            self.applied = self.goal;
            let gain_db = self.controller.step(&self.meter, self.on, self.target);
            self.goal = db_to_linear(gain_db);
            self.step = (self.goal - self.applied) / self.meter.frames_per_sub as f32;
        }
    }

    fn fade(&self) -> f32 {
        if self.played >= self.fade_frames {
            return 1.0;
        }
        // Smoothstep: no corner at either end
        let t = self.played as f32 / self.fade_frames as f32;
        t * t * (3.0 - 2.0 * t)
    }
}

impl<S> Iterator for LevelSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let x = self.inner.next()?;
        if self.channel_index == 0 {
            self.start_frame();
        }
        let ch = self.channel_index;
        self.meter.sample(ch, x);
        let out = x * self.applied * self.fade();

        self.channel_index += 1;
        if self.channel_index >= usize::from(self.channels.max(1)) {
            self.channel_index = 0;
            self.end_frame();
        }
        Some(out)
    }
}

impl<S> Source for LevelSource<S>
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
    use super::super::sound::{LevelTarget, SoundParams};
    use super::*;
    use rodio::buffer::SamplesBuffer;

    const RATE: u32 = 48_000;

    /// A stereo 1 kHz tone at `amplitude`, `secs` long
    fn tone(amplitude: f32, secs: f32) -> Vec<f32> {
        let frames = (RATE as f32 * secs) as usize;
        (0..frames)
            .flat_map(|i| {
                let v = amplitude
                    * (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / RATE as f32).sin();
                [v, v]
            })
            .collect()
    }

    fn source(samples: Vec<f32>) -> SamplesBuffer {
        SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(RATE).unwrap(),
            samples,
        )
    }

    fn run(
        samples: Vec<f32>,
        params: &SharedSoundParams,
        start: Option<f32>,
    ) -> (Vec<f32>, Arc<LevelReadout>) {
        let readout = LevelReadout::new_shared();
        let out =
            LevelSource::new(source(samples), params.clone(), readout.clone(), start).collect();
        (out, readout)
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, x| m.max(x.abs()))
    }

    #[test]
    fn the_meter_reads_a_tone_right() {
        // A stereo 1 kHz tone at -20 dBFS peak measures about -20 LUFS
        // (BS.1770: the K-weighting adds ~0.7 dB at 1 kHz, the sine's mean
        // square takes 3 dB, the two channels add 3 dB back)
        let mut meter = Meter::new(2, RATE);
        let mut window = Window::new();
        for frame in tone(0.1, 5.0).chunks(2) {
            meter.sample(0, frame[0]);
            meter.sample(1, frame[1]);
            if meter.end_frame() {
                if let Some(e) = meter.mean_of_last(MOMENTARY_SUBS) {
                    window.push(e);
                }
            }
        }
        let l = window.loudness().unwrap();
        assert!((l - -20.0).abs() < 0.5, "{l}");
        let m = meter.momentary().unwrap();
        assert!((m - l).abs() < 0.1, "{m} vs {l}");
    }

    #[test]
    fn the_meter_works_at_other_rates() {
        for rate in [22_050, 44_100, 96_000] {
            let mut meter = Meter::new(1, rate);
            let frames = rate as usize * 4;
            for i in 0..frames {
                let v = 0.1 * (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / rate as f32).sin();
                meter.sample(0, v);
                meter.end_frame();
            }
            // One channel: 3 dB below the stereo tone
            let m = meter.short_term().unwrap();
            assert!((m - -23.0).abs() < 0.5, "{rate} Hz: {m}");
        }
    }

    #[test]
    fn gating_ignores_the_quiet_parts() {
        let mut window = Window::new();
        // Loud and quiet blocks: the quiet ones (20 LU down) are gated out
        let loud = 10f64.powf((-14.0 + 0.691) / 10.0);
        let quiet = 10f64.powf((-34.0 + 0.691) / 10.0);
        for i in 0..400 {
            window.push(if i % 2 == 0 { loud } else { quiet });
        }
        let l = window.loudness().unwrap();
        assert!((l - -14.0).abs() < 0.01, "{l}");
    }

    #[test]
    fn a_loud_new_station_is_turned_down_to_the_level() {
        let params = SoundParams::new_shared();
        // -6 LUFS, far too loud
        let (out, readout) = run(tone(0.5, 40.0), &params, None);
        let target = LevelTarget::Normal.lufs();
        assert_eq!(readout.phase(), LevelPhase::Settled);
        let station = readout.loudness_lufs().unwrap();
        assert!((station - -6.0).abs() < 0.6, "{station}");
        let want = target - station;
        assert!(
            (readout.gain_db() - want).abs() < 0.3,
            "{}",
            readout.gain_db()
        );
        // The output is at the level: 10 dB down on 0.5
        let tail = peak(&out[out.len() - 9_600..]);
        let expected = 0.5 * db_to_linear(want);
        assert!((tail - expected).abs() < expected * 0.05, "{tail}");
    }

    #[test]
    fn a_new_station_starts_turned_down_and_learns() {
        let params = SoundParams::new_shared();
        let readout = LevelReadout::new_shared();
        let mut s = LevelSource::new(source(tone(0.05, 5.0)), params, readout.clone(), None);
        assert_eq!(readout.phase(), LevelPhase::Learning);
        assert_eq!(readout.gain_db(), UNKNOWN_START_DB);
        // After a few seconds it is on its way up: the station is quiet
        for _ in 0..RATE * 2 * 3 {
            s.next();
        }
        assert_eq!(readout.phase(), LevelPhase::Learning);
        assert!(readout.gain_db() > UNKNOWN_START_DB + 3.0);
        assert_eq!(readout.loudness_lufs(), None);
    }

    #[test]
    fn a_known_station_starts_at_its_gain() {
        let params = SoundParams::new_shared();
        let readout = LevelReadout::new_shared();
        let _s = LevelSource::new(source(tone(0.5, 1.0)), params, readout.clone(), Some(-8.0));
        assert_eq!(readout.phase(), LevelPhase::Settled);
        assert_eq!(readout.gain_db(), -8.0);
        assert_eq!(readout.loudness_lufs(), Some(-8.0));
    }

    #[test]
    fn the_gain_stays_in_its_limits() {
        let params = SoundParams::new_shared();
        // -46 LUFS-ish: barely sound, it would need +30 dB
        let (_, readout) = run(tone(0.0071, 40.0), &params, Some(-46.0));
        assert!(readout.gain_db() <= MAX_GAIN_DB);
        let (_, readout) = run(tone(0.99, 10.0), &params, Some(0.0));
        assert!(readout.gain_db() >= MIN_GAIN_DB);
    }

    #[test]
    fn silence_holds_the_gain() {
        let params = SoundParams::new_shared();
        let mut samples = tone(0.5, 30.0);
        samples.extend(std::iter::repeat_n(0.0, RATE as usize * 2 * 20));
        let readout = LevelReadout::new_shared();
        let mut s = LevelSource::new(source(samples), params, readout.clone(), None);
        // A second into the silence: the last sound is out of the meter
        for _ in 0..RATE * 2 * 31 {
            s.next();
        }
        let before = readout.gain_db();
        for _ in s.by_ref() {}
        assert!((readout.gain_db() - before).abs() < 0.01);
    }

    #[test]
    fn off_brings_the_gain_back_to_nothing() {
        let params = SoundParams::new_shared();
        params.lock().unwrap().set_match_volume(false);
        let (out, readout) = run(tone(0.5, 2.0), &params, Some(-6.0));
        assert_eq!(readout.gain_db(), 0.0);
        assert!((peak(&out[out.len() - 9_600..]) - 0.5).abs() < 0.01);
    }

    #[test]
    fn turning_it_off_mid_stream_glides_back() {
        let params = SoundParams::new_shared();
        let readout = LevelReadout::new_shared();
        let mut s = LevelSource::new(
            source(tone(0.5, 10.0)),
            params.clone(),
            readout.clone(),
            Some(-6.0),
        );
        for _ in 0..RATE * 2 {
            s.next();
        }
        params.lock().unwrap().set_match_volume(false);
        let rest: Vec<f32> = s.collect();
        assert!(readout.gain_db().abs() < 0.01);
        // No jump: neighbouring samples of the 1 kHz tone stay close
        let biggest_jump = rest
            .chunks(2)
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(biggest_jump < 0.07, "{biggest_jump}");
    }

    #[test]
    fn a_blast_is_turned_down_fast() {
        let params = SoundParams::new_shared();
        // Known as a quiet station, then a loud ad
        let readout = LevelReadout::new_shared();
        let mut s = LevelSource::new(source(tone(0.9, 6.0)), params, readout.clone(), Some(-26.0));
        assert_eq!(readout.gain_db(), MAX_GAIN_DB);
        for _ in 0..RATE * 2 * 6 {
            s.next();
        }
        // Normal speed would be 2 dB/s; panic gets it well down in 6 s
        assert!(readout.gain_db() < -5.0, "{}", readout.gain_db());
    }

    #[test]
    fn it_fades_in() {
        let params = SoundParams::new_shared();
        params.lock().unwrap().set_match_volume(false);
        let (out, _) = run(vec![0.5; RATE as usize * 2], &params, None);
        assert_eq!(out[0], 0.0);
        let half = (FADE_IN_MS / 2000.0 * RATE as f32) as usize * 2;
        assert!((out[half] - 0.25).abs() < 0.01, "{}", out[half]);
        assert_eq!(out[out.len() - 1], 0.5);
    }
}
