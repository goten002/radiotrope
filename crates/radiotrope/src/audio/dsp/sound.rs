//! Settings for the Sound filters: station volume matching, channels and
//! balance. What matching is doing to the station playing now is read
//! back through [`LevelReadout`].

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::sound::*;

/// How loud matched stations play
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LevelTarget {
    Quiet,
    #[default]
    Normal,
    Loud,
}

impl LevelTarget {
    /// The loudness stations are brought to, in LUFS
    pub fn lufs(self) -> f32 {
        match self {
            LevelTarget::Quiet => TARGET_QUIET_LUFS,
            LevelTarget::Normal => TARGET_NORMAL_LUFS,
            LevelTarget::Loud => TARGET_LOUD_LUFS,
        }
    }

    /// "quiet", "normal" or "loud", as settings and the Remote API name it
    pub fn as_str(self) -> &'static str {
        match self {
            LevelTarget::Quiet => "quiet",
            LevelTarget::Normal => "normal",
            LevelTarget::Loud => "loud",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "quiet" => Some(LevelTarget::Quiet),
            "normal" => Some(LevelTarget::Normal),
            "loud" => Some(LevelTarget::Loud),
            _ => None,
        }
    }
}

/// How the two channels reach the speakers
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Channels {
    #[default]
    Stereo,
    /// Both channels mixed, on both speakers
    Mono,
    /// Left and right swapped
    Swap,
}

impl Channels {
    /// "stereo", "mono" or "swap"
    pub fn as_str(self) -> &'static str {
        match self {
            Channels::Stereo => "stereo",
            Channels::Mono => "mono",
            Channels::Swap => "swap",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stereo" => Some(Channels::Stereo),
            "mono" => Some(Channels::Mono),
            "swap" => Some(Channels::Swap),
            _ => None,
        }
    }
}

/// Thread-safe handle to the Sound settings
pub type SharedSoundParams = Arc<Mutex<SoundParams>>;

/// The Sound settings. Like the equalizer's, they change only through the
/// setters, which bump a counter the sources read without the lock.
#[derive(Debug)]
pub struct SoundParams {
    match_volume: bool,
    target: LevelTarget,
    channels: Channels,
    /// -1 (left only) to 1 (right only)
    balance: f32,
    changes: Arc<AtomicU64>,
}

impl Default for SoundParams {
    fn default() -> Self {
        Self {
            match_volume: true,
            target: LevelTarget::Normal,
            channels: Channels::Stereo,
            balance: 0.0,
            changes: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl SoundParams {
    pub fn new_shared() -> SharedSoundParams {
        Arc::new(Mutex::new(Self::default()))
    }

    pub fn match_volume(&self) -> bool {
        self.match_volume
    }

    pub fn target(&self) -> LevelTarget {
        self.target
    }

    pub fn channels(&self) -> Channels {
        self.channels
    }

    pub fn balance(&self) -> f32 {
        self.balance
    }

    /// How many times the settings have changed
    pub fn generation(&self) -> u64 {
        self.changes.load(Ordering::Acquire)
    }

    pub(crate) fn changes(&self) -> Arc<AtomicU64> {
        self.changes.clone()
    }

    fn changed(&mut self) {
        self.changes.fetch_add(1, Ordering::Release);
    }

    pub fn set_match_volume(&mut self, on: bool) {
        self.match_volume = on;
        self.changed();
    }

    pub fn set_target(&mut self, target: LevelTarget) {
        self.target = target;
        self.changed();
    }

    pub fn set_channels(&mut self, channels: Channels) {
        self.channels = channels;
        self.changed();
    }

    /// Clamped to -1..1; NaN and infinities are ignored. Close enough to
    /// the centre snaps to it.
    pub fn set_balance(&mut self, balance: f32) {
        if !balance.is_finite() {
            return;
        }
        let b = balance.clamp(-1.0, 1.0);
        self.balance = if b.abs() < BALANCE_SNAP { 0.0 } else { b };
        self.changed();
    }
}

/// Where matching is with the station playing now
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelPhase {
    /// Nothing playing
    None,
    /// A station heard for the first time: still measuring it
    Learning,
    /// Its loudness is known
    Settled,
}

/// What station volume matching is doing now, written by the playing
/// station's [`LevelSource`](super::level::LevelSource) and read by the
/// app. Lock-free.
#[derive(Debug, Default)]
pub struct LevelReadout {
    /// The station allowed to write: one that is fading out or stopped
    /// can't
    owner: AtomicU64,
    phase: AtomicU8,
    /// Gain applied now, in dB (f32 bits)
    gain_db: AtomicU32,
    /// The station's measured loudness in LUFS (f32 bits); NaN until known
    loudness: AtomicU32,
}

impl LevelReadout {
    pub fn new_shared() -> Arc<Self> {
        let r = Arc::new(Self::default());
        r.clear();
        r
    }

    pub fn phase(&self) -> LevelPhase {
        match self.phase.load(Ordering::Relaxed) {
            1 => LevelPhase::Learning,
            2 => LevelPhase::Settled,
            _ => LevelPhase::None,
        }
    }

    /// The gain matching applies now, in dB
    pub fn gain_db(&self) -> f32 {
        f32::from_bits(self.gain_db.load(Ordering::Relaxed))
    }

    /// The station's loudness, once it is settled
    pub fn loudness_lufs(&self) -> Option<f32> {
        let l = f32::from_bits(self.loudness.load(Ordering::Relaxed));
        (self.phase() == LevelPhase::Settled && l.is_finite()).then_some(l)
    }

    /// Make a new station the writer
    pub(crate) fn claim(&self) -> u64 {
        self.owner.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Write, if `owner` is still the writer
    pub(crate) fn set_if(
        &self,
        owner: u64,
        phase: LevelPhase,
        gain_db: f32,
        loudness: Option<f32>,
    ) {
        if self.owner.load(Ordering::Acquire) == owner {
            self.set(phase, gain_db, loudness);
        }
    }

    fn set(&self, phase: LevelPhase, gain_db: f32, loudness: Option<f32>) {
        let p = match phase {
            LevelPhase::None => 0,
            LevelPhase::Learning => 1,
            LevelPhase::Settled => 2,
        };
        self.phase.store(p, Ordering::Relaxed);
        self.gain_db.store(gain_db.to_bits(), Ordering::Relaxed);
        self.loudness
            .store(loudness.unwrap_or(f32::NAN).to_bits(), Ordering::Relaxed);
    }

    /// Nothing playing: and the last station can no longer write
    pub(crate) fn clear(&self) {
        self.owner.fetch_add(1, Ordering::AcqRel);
        self.set(LevelPhase::None, 0.0, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_at_normal_in_stereo() {
        let p = SoundParams::default();
        assert!(p.match_volume());
        assert_eq!(p.target(), LevelTarget::Normal);
        assert_eq!(p.channels(), Channels::Stereo);
        assert_eq!(p.balance(), 0.0);
    }

    #[test]
    fn names_round_trip() {
        for t in [LevelTarget::Quiet, LevelTarget::Normal, LevelTarget::Loud] {
            assert_eq!(LevelTarget::parse(t.as_str()), Some(t));
        }
        for c in [Channels::Stereo, Channels::Mono, Channels::Swap] {
            assert_eq!(Channels::parse(c.as_str()), Some(c));
        }
        assert_eq!(LevelTarget::parse("LOUD"), None);
    }

    #[test]
    fn balance_clamps_snaps_and_ignores_nan() {
        let mut p = SoundParams::default();
        p.set_balance(3.0);
        assert_eq!(p.balance(), 1.0);
        p.set_balance(0.02);
        assert_eq!(p.balance(), 0.0);
        p.set_balance(-0.4);
        assert_eq!(p.balance(), -0.4);
        p.set_balance(f32::NAN);
        assert_eq!(p.balance(), -0.4);
    }

    #[test]
    fn every_setter_counts_a_change() {
        let mut p = SoundParams::default();
        let g = p.generation();
        p.set_match_volume(false);
        p.set_target(LevelTarget::Loud);
        p.set_channels(Channels::Mono);
        p.set_balance(0.5);
        assert_eq!(p.generation(), g + 4);
    }

    #[test]
    fn loudness_reads_back_only_once_settled() {
        let r = LevelReadout::new_shared();
        assert_eq!(r.phase(), LevelPhase::None);
        let me = r.claim();
        r.set_if(me, LevelPhase::Learning, -3.0, Some(-10.0));
        assert_eq!(r.loudness_lufs(), None);
        r.set_if(me, LevelPhase::Settled, -6.0, Some(-10.0));
        assert_eq!(r.loudness_lufs(), Some(-10.0));
        assert_eq!(r.gain_db(), -6.0);
    }

    #[test]
    fn only_the_latest_station_writes() {
        let r = LevelReadout::new_shared();
        let old = r.claim();
        let new = r.claim();
        r.set_if(old, LevelPhase::Settled, -6.0, Some(-10.0));
        assert_eq!(r.phase(), LevelPhase::None);
        r.set_if(new, LevelPhase::Learning, -7.0, None);
        assert_eq!(r.phase(), LevelPhase::Learning);
        r.clear();
        r.set_if(new, LevelPhase::Settled, -6.0, Some(-10.0));
        assert_eq!(r.phase(), LevelPhase::None);
    }
}
