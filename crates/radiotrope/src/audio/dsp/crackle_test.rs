//! Tests for the Crackle Filter, all on generated sound: music-like tones
//! and noise with seeded clicks, so every expected value is known

use super::*;
use crate::audio::dsp::sound::SoundParams;
use rodio::buffer::SamplesBuffer;
use std::f64::consts::TAU;

const RATES: [u32; 3] = [22_050, 44_100, 48_000];

/// Deterministic noise in -1..1
struct Noise(u32);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f64 / u32::MAX as f64 * 2.0 - 1.0
    }
}

/// Music-like mono sound: a melody of harmonic notes that change every
/// 0.4 s with soft attacks and releases, a bass line and a quiet bed of
/// low-passed noise, at about -12 dBFS. Nothing in it jumps: a jump is a
/// click.
fn music(rate: u32, secs: f64, seed: u32) -> Vec<f64> {
    let mut noise = Noise(seed);
    let melody = [220.0, 247.0, 262.0, 294.0, 330.0, 294.0, 262.0, 196.0];
    let bass = [55.0, 55.0, 65.4, 73.4];
    let r = f64::from(rate);
    let n = (secs * r) as usize;
    let (mut lead, mut low, mut bed) = (0.0, 0.0, 0.0);
    (0..n)
        .map(|i| {
            let t = i as f64 / r;
            let note = (t / 0.4) as usize;
            let since = t - note as f64 * 0.4;
            let env =
                (since / 0.02).min(1.0) * ((0.4 - since) / 0.02).min(1.0) * (-since * 2.0).exp();
            lead += TAU * melody[note % melody.len()] / r;
            low += TAU * bass[(t / 0.8) as usize % bass.len()] / r;
            let tone: f64 = (1..=6)
                .map(|h| (lead * h as f64).sin() * 0.6f64.powi(h - 1))
                .sum();
            bed = 0.9 * bed + 0.1 * noise.next();
            0.12 * env * tone + 0.1 * low.sin() + 0.02 * bed
        })
        .collect()
}

/// Record clicks, shaped as in RES-3's synthetic set: `count` of them at
/// seeded places, each a burst of 1 to `widest` samples that flips sign
/// irregularly (a scratch is sharp, so its sound is broadband) and dies
/// away, `amp` at its loudest
fn clicks(len: usize, count: usize, widest: usize, amp: f64, seed: u32) -> Vec<f64> {
    let mut noise = Noise(seed);
    let mut out = vec![0.0; len];
    let mut unit = || noise.next() * 0.5 + 0.5;
    for _ in 0..count {
        let at = (unit() * (len - 2_000) as f64) as usize + 1_000;
        let width = 1 + (unit() * widest as f64) as usize;
        let sign = if unit() > 0.5 { 1.0 } else { -1.0 };
        let a = sign * amp * (0.3 + 0.7 * unit());
        let (w, phase) = (0.5 + 2.0 * unit(), 6.0 * unit());
        for k in 0..width {
            let k = k as f64;
            let flip = ((k * w + phase).sin() + 0.3).signum();
            out[at + k as usize] += a * (-k / (width as f64 / 3.0)).exp() * flip;
        }
    }
    out
}

fn add(a: &[f64], b: &[f64]) -> Vec<f64> {
    a.iter().zip(b).map(|(x, y)| x + y).collect()
}

fn to_f32(x: &[f64]) -> Vec<f32> {
    x.iter().map(|&v| v as f32).collect()
}

fn run(channels: u16, rate: u32, samples: Vec<f32>, level: CrackleLevel) -> (Vec<f32>, u64) {
    let mut params = SoundParams::default();
    params.set_crackle(level);
    let mut source = CrackleSource::new(
        SamplesBuffer::new(
            NonZero::new(channels).unwrap(),
            NonZero::new(rate).unwrap(),
            samples,
        ),
        params.switches(),
    );
    let out: Vec<f32> = source.by_ref().collect();
    (out, source.repairs())
}

/// Energy of `a - b`, in dB below the energy of `reference`
fn below(reference: &[f64], a: &[f32], b: &[f64]) -> f64 {
    let err: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, y)| (f64::from(x) - y).powi(2))
        .sum();
    let sig: f64 = reference.iter().map(|x| x * x).sum();
    10.0 * (sig / err.max(1e-30)).log10()
}

/// Like [`below`], on the first differences: the treble, where crackle
/// is heard
fn treble_below(reference: &[f64], a: &[f32], b: &[f64]) -> f64 {
    let d = |v: &[f64]| v.windows(2).map(|w| w[1] - w[0]).collect::<Vec<_>>();
    let a: Vec<f64> = a.iter().map(|&x| f64::from(x)).collect();
    below(&d(reference), &to_f32(&d(&a)), &d(b))
}

#[test]
fn off_gives_back_the_same_samples() {
    for rate in RATES {
        let clean = music(rate, 2.0, 1);
        let x = to_f32(&add(&clean, &clicks(clean.len(), 20, 10, 0.4, 2)));
        let (out, repairs) = run(1, rate, x.clone(), CrackleLevel::Off);
        assert_eq!(out, x, "{rate} Hz");
        assert_eq!(repairs, 0);
    }
}

#[test]
fn interpolation_rebuilds_a_sum_of_sines() {
    let rate = 44_100.0;
    let n = 2048;
    let x: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            0.5 * (TAU * 440.0 * t).sin()
                + 0.3 * (TAU * 1_234.0 * t + 1.0).sin()
                + 0.1 * (TAU * 5_000.0 * t).sin()
        })
        .collect();
    let mut work = Work::new(32, n, 4096, 64);
    work.fit(&x);
    let mut gap = x.clone();
    let (s, t) = (1_000, 1_019);
    gap[s..=t].fill(0.9);
    work.interpolate(&mut gap, s, t);
    let err: f64 = (s..=t).map(|i| (gap[i] - x[i]).powi(2)).sum::<f64>() / 20.0;
    let db = 10.0 * (err / 0.5f64.powi(2)).log10();
    assert!(db < -40.0, "{db} dB");
    // The samples around the gap are untouched
    assert_eq!(&gap[..s], &x[..s]);
    assert_eq!(&gap[t + 1..], &x[t + 1..]);
}

#[test]
fn clicks_are_found_and_repaired() {
    for rate in RATES {
        let clean = music(rate, 3.0, 3);
        let noise = clicks(clean.len(), 15, 8, 0.4, 4);
        let x = add(&clean, &noise);
        let before = below(&clean, &to_f32(&x), &clean);
        for level in [CrackleLevel::Light, CrackleLevel::Strong] {
            let (out, repairs) = run(1, rate, to_f32(&x), level);
            assert_eq!(out.len(), x.len());
            let after = below(&clean, &out, &clean);
            assert!(
                (13..=40).contains(&repairs),
                "{rate} Hz {level:?}: {repairs} repairs"
            );
            assert!(
                after > before + 10.0,
                "{rate} Hz {level:?}: {before:.1} -> {after:.1} dB"
            );
        }
    }
}

#[test]
fn clean_music_is_left_alone_at_light() {
    for rate in RATES {
        let x = to_f32(&music(rate, 3.0, 5));
        let (out, repairs) = run(1, rate, x.clone(), CrackleLevel::Light);
        assert_eq!(repairs, 0, "{rate} Hz");
        assert_eq!(out, x, "{rate} Hz");
    }
}

#[test]
fn a_drum_hit_is_left_alone() {
    for rate in RATES {
        // A snare-like hit every half second: a 1 ms attack, then noise
        // dying away over 60 ms
        let mut noise = Noise(6);
        let base = music(rate, 3.0, 7);
        let r = f64::from(rate);
        let hits: Vec<f64> = (0..base.len())
            .map(|i| {
                let since = (i as f64 / r) % 0.5;
                let attack = (since / 0.001).min(1.0);
                0.4 * attack * (-since / 0.02).exp() * noise.next()
            })
            .collect();
        let x = add(&base, &hits);
        let (out, repairs) = run(1, rate, to_f32(&x), CrackleLevel::Light);
        assert_eq!(repairs, 0, "{rate} Hz Light");
        assert_eq!(out, to_f32(&x), "{rate} Hz Light");
        // Strong may touch the odd sample of the noise, never the hit
        let (out, _) = run(1, rate, to_f32(&x), CrackleLevel::Strong);
        let damage = below(&x, &out, &x);
        assert!(damage > 35.0, "{rate} Hz Strong: {damage:.1} dB");
    }
}

#[test]
fn switching_doesnt_jump() {
    let rate = 44_100;
    let clean = music(rate, 6.0, 8);
    let x = to_f32(&add(&clean, &clicks(clean.len(), 60, 6, 0.4, 9)));
    let mut params = SoundParams::default();
    let mut source = CrackleSource::new(
        SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(rate).unwrap(),
            x.clone(),
        ),
        params.switches(),
    );
    let second = rate as usize;
    let mut out = Vec::with_capacity(x.len());
    for i in 0..x.len() {
        match i / second {
            1 if i % second == 0 => params.set_crackle(CrackleLevel::Strong),
            3 if i % second == 0 => params.set_crackle(CrackleLevel::Off),
            4 if i % second == 0 => params.set_crackle(CrackleLevel::Light),
            5 if i % second == 3_333 => params.set_crackle(CrackleLevel::Off),
            _ => {}
        }
        out.push(source.next().unwrap());
    }
    assert!(source.next().is_none());
    // Nothing is shifted: before the filter is first on, and from shortly
    // after it is last off, the output is the input
    let ahead = 2 * block_len(rate) + 2 * reach(block_len(rate), order(rate));
    assert_eq!(&out[..second], &x[..second]);
    assert_eq!(
        &out[5 * second + 3_333 + ahead..],
        &x[5 * second + 3_333 + ahead..]
    );
    // No new steps anywhere: repairs only take steps away
    let step = |v: &[f32]| v.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()));
    assert!(step(&out) <= step(&x), "{} vs {}", step(&out), step(&x));
    let on = second..3 * second;
    assert!(
        step(&out[on.clone()]) < 0.5 * step(&x[on]),
        "Strong took the clicks out"
    );
}

#[test]
fn stereo_channels_are_repaired_on_their_own() {
    for rate in RATES {
        let left = music(rate, 2.0, 10);
        let right = music(rate, 2.0, 11);
        let noisy = add(&left, &clicks(left.len(), 10, 6, 0.4, 12));
        let x: Vec<f32> = noisy
            .iter()
            .zip(&right)
            .flat_map(|(&l, &r)| [l as f32, r as f32])
            .collect();
        let (out, repairs) = run(2, rate, x.clone(), CrackleLevel::Strong);
        assert_eq!(out.len(), x.len());
        assert!(repairs >= 9, "{rate} Hz: {repairs}");
        let out_left: Vec<f32> = out.iter().step_by(2).copied().collect();
        let out_right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(out_right, to_f32(&right), "{rate} Hz: right untouched");
        let before = below(&left, &to_f32(&noisy), &left);
        let after = below(&left, &out_left, &left);
        assert!(
            after > before + 15.0,
            "{rate} Hz: {before:.1} -> {after:.1}"
        );
    }
}

#[test]
fn every_length_comes_back_whole() {
    for frames in [0, 1, 100, 1_537, 1_538, 1_539, 5_000, 44_100] {
        let x: Vec<f32> = to_f32(&music(44_100, frames as f64 / 44_100.0 + 1e-9, 13))
            .into_iter()
            .take(frames)
            .flat_map(|v| [v, -v])
            .collect();
        let (out, _) = run(2, 44_100, x.clone(), CrackleLevel::Strong);
        assert_eq!(out.len(), x.len(), "{frames} frames");
        assert!(out.chunks(2).all(|f| f[0] == -f[1]), "{frames} frames");
    }
}

#[test]
fn silence_and_bad_samples_come_out_as_silence() {
    for level in [CrackleLevel::Light, CrackleLevel::Strong] {
        let mut x = vec![0.0f32; 48_000 * 2];
        x[10_000] = f32::NAN;
        x[20_000] = f32::INFINITY;
        x[30_000] = f32::NEG_INFINITY;
        let (out, _) = run(1, 48_000, x, level);
        assert!(out.iter().all(|&v| v == 0.0), "{level:?}");
    }
}

#[test]
fn many_channels_pass_through() {
    let x: Vec<f32> = (0..10 * 20_000)
        .map(|i| ((i % 97) as f32 - 48.0) / 100.0)
        .collect();
    let (out, repairs) = run(10, 44_100, x.clone(), CrackleLevel::Strong);
    assert_eq!(out, x);
    assert_eq!(repairs, 0);
}

/// Mono first, then stereo: each sample comes with the format it is in
#[test]
fn a_format_change_plays_out_and_starts_over() {
    struct Two {
        left: usize,
    }
    impl Iterator for Two {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            if self.left == 0 {
                return None;
            }
            self.left -= 1;
            Some(if self.left >= 1_000 {
                0.1
            } else if self.left.is_multiple_of(2) {
                0.2
            } else {
                -0.2
            })
        }
    }
    impl Source for Two {
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn channels(&self) -> NonZero<u16> {
            NonZero::new(if self.left >= 1_000 { 1 } else { 2 }).unwrap()
        }
        fn sample_rate(&self) -> NonZero<u32> {
            NonZero::new(48_000).unwrap()
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }
    let mut params = SoundParams::default();
    params.set_crackle(CrackleLevel::Strong);
    let mut source = CrackleSource::new(Two { left: 3_000 }, params.switches());
    let mut out = Vec::new();
    while let Some(x) = source.next() {
        let want = if out.len() < 2_000 { 1 } else { 2 };
        assert_eq!(source.channels().get(), want, "sample {}", out.len());
        out.push(x);
    }
    assert_eq!(out.len(), 3_000);
    assert!(out[..2_000].iter().all(|&x| x == 0.1));
    assert!(out[2_000..].windows(2).all(|w| w[0] * w[1] < 0.0));
}

/// The regression test: a 4 s generated "78" (band-limited music with
/// big clicks and fine crackle, as in RES-3's synthetic set) at the
/// owner's station's 22.05 kHz and at 44.1 kHz. How much it repairs and
/// how close it gets to the clean record stay within bounds, and on the
/// clean record neither strength does more than a trace of harm.
#[test]
fn a_generated_record() {
    for rate in [22_050, 44_100] {
        let r = f64::from(rate);
        // Band-limit the music to about 5 kHz, as on a 78
        let mut lp = [0.0; 4];
        let a = (-TAU * 5_000.0 / r).exp();
        let clean: Vec<f64> = music(rate, 4.0, 14)
            .into_iter()
            .map(|x| {
                let mut v = x;
                for s in &mut lp {
                    *s = (1.0 - a) * v + a * *s;
                    v = *s;
                }
                v
            })
            .collect();
        let peak = clean.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        let len = clean.len();
        // 6 big clicks and 60 small ones a second
        let big = clicks(len, 24, (2e-3 * r) as usize, 0.5 * peak, 15);
        let small = clicks(len, 240, (0.3e-3 * r) as usize + 1, 0.5 * peak / 6.0, 16);
        let x = add(&add(&clean, &big), &small);
        let input = below(&clean, &to_f32(&x), &clean);

        let (light, light_repairs) = run(1, rate, to_f32(&x), CrackleLevel::Light);
        let (strong, strong_repairs) = run(1, rate, to_f32(&x), CrackleLevel::Strong);
        let light_db = below(&clean, &light, &clean);
        let strong_db = below(&clean, &strong, &clean);
        let treble = [
            treble_below(&clean, &to_f32(&x), &clean),
            treble_below(&clean, &light, &clean),
            treble_below(&clean, &strong, &clean),
        ];
        eprintln!(
            "{rate} Hz: input {input:.1} dB, Light {light_db:.1} dB ({light_repairs}), \
             Strong {strong_db:.1} dB ({strong_repairs}); treble {treble:.1?}"
        );
        assert!(light_db > input + 1.0, "{rate} Hz Light");
        assert!(strong_db > input, "{rate} Hz Strong");
        // Strong takes out more of the fine crackle
        assert!(treble[1] > treble[0] + 1.0, "{rate} Hz Light treble");
        assert!(treble[2] > treble[1] + 1.0, "{rate} Hz Strong treble");
        assert!(
            (30..=200).contains(&light_repairs),
            "{rate} Hz Light {light_repairs}"
        );
        assert!(
            (120..=350).contains(&strong_repairs),
            "{rate} Hz Strong {strong_repairs}"
        );

        // The clean record: Light repairs nothing, Strong little and
        // nothing audible
        let (out, repairs) = run(1, rate, to_f32(&clean), CrackleLevel::Light);
        assert_eq!(repairs, 0, "{rate} Hz clean Light");
        assert_eq!(out, to_f32(&clean));
        let (out, repairs) = run(1, rate, to_f32(&clean), CrackleLevel::Strong);
        let damage = below(&clean, &out, &clean);
        assert!(
            damage > 40.0,
            "{rate} Hz clean Strong: {damage:.1} dB, {repairs} repairs"
        );
    }
}
