//! Settings for the filters: the Volume Leveler (station volume matching)
//! and the switches of Clip Guard, the Rumble Filter and Smooth Switching.
//! What the leveler is doing to the station playing now is read back
//! through [`LevelReadout`].

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
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

/// The filters' on/off switches, read without the lock by the sources and
/// the engine
#[derive(Debug)]
pub struct FilterSwitches {
    /// Clip Guard as it runs: on while it is switched on, and always while
    /// the Volume Leveler is on (the leveler can raise a station 9 dB)
    clip_guard: AtomicBool,
    rumble_filter: AtomicBool,
    smooth_switching: AtomicBool,
}

impl FilterSwitches {
    pub fn clip_guard(&self) -> bool {
        self.clip_guard.load(Ordering::Relaxed)
    }

    pub fn rumble_filter(&self) -> bool {
        self.rumble_filter.load(Ordering::Relaxed)
    }

    pub fn smooth_switching(&self) -> bool {
        self.smooth_switching.load(Ordering::Relaxed)
    }
}

impl Default for FilterSwitches {
    fn default() -> Self {
        Self {
            clip_guard: AtomicBool::new(true),
            rumble_filter: AtomicBool::new(true),
            smooth_switching: AtomicBool::new(true),
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
    /// Clip Guard as switched (see [`FilterSwitches`] for how it runs)
    clip_guard: bool,
    rumble_filter: bool,
    smooth_switching: bool,
    switches: Arc<FilterSwitches>,
    changes: Arc<AtomicU64>,
}

impl Default for SoundParams {
    fn default() -> Self {
        Self {
            match_volume: true,
            target: LevelTarget::Normal,
            clip_guard: true,
            rumble_filter: true,
            smooth_switching: true,
            switches: Arc::new(FilterSwitches::default()),
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

    /// Clip Guard as switched; it runs anyway while the leveler is on
    pub fn clip_guard(&self) -> bool {
        self.clip_guard
    }

    pub fn rumble_filter(&self) -> bool {
        self.rumble_filter
    }

    pub fn smooth_switching(&self) -> bool {
        self.smooth_switching
    }

    /// The switches as the sources read them
    pub fn switches(&self) -> Arc<FilterSwitches> {
        self.switches.clone()
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
        self.update_clip_guard();
        self.changed();
    }

    fn update_clip_guard(&self) {
        self.switches
            .clip_guard
            .store(self.clip_guard || self.match_volume, Ordering::Relaxed);
    }

    pub fn set_target(&mut self, target: LevelTarget) {
        self.target = target;
        self.changed();
    }

    pub fn set_clip_guard(&mut self, on: bool) {
        self.clip_guard = on;
        self.update_clip_guard();
        self.changed();
    }

    pub fn set_rumble_filter(&mut self, on: bool) {
        self.rumble_filter = on;
        self.switches.rumble_filter.store(on, Ordering::Relaxed);
        self.changed();
    }

    pub fn set_smooth_switching(&mut self, on: bool) {
        self.smooth_switching = on;
        self.switches.smooth_switching.store(on, Ordering::Relaxed);
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
    fn everything_is_on_at_first() {
        let p = SoundParams::default();
        assert!(p.match_volume());
        assert_eq!(p.target(), LevelTarget::Normal);
        assert!(p.clip_guard() && p.rumble_filter() && p.smooth_switching());
        let s = p.switches();
        assert!(s.clip_guard() && s.rumble_filter() && s.smooth_switching());
    }

    #[test]
    fn names_round_trip() {
        for t in [LevelTarget::Quiet, LevelTarget::Normal, LevelTarget::Loud] {
            assert_eq!(LevelTarget::parse(t.as_str()), Some(t));
        }
        assert_eq!(LevelTarget::parse("LOUD"), None);
    }

    #[test]
    fn clip_guard_runs_while_the_leveler_is_on() {
        let mut p = SoundParams::default();
        let s = p.switches();
        p.set_clip_guard(false);
        assert!(!p.clip_guard());
        assert!(s.clip_guard(), "the leveler still needs it");
        p.set_match_volume(false);
        assert!(!s.clip_guard());
        p.set_match_volume(true);
        assert!(s.clip_guard());
    }

    #[test]
    fn switches_follow_the_settings() {
        let mut p = SoundParams::default();
        let s = p.switches();
        p.set_rumble_filter(false);
        p.set_smooth_switching(false);
        assert!(!s.rumble_filter() && !s.smooth_switching());
    }

    #[test]
    fn every_setter_counts_a_change() {
        let mut p = SoundParams::default();
        let g = p.generation();
        p.set_match_volume(false);
        p.set_target(LevelTarget::Loud);
        p.set_clip_guard(false);
        p.set_rumble_filter(false);
        p.set_smooth_switching(false);
        assert_eq!(p.generation(), g + 5);
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
