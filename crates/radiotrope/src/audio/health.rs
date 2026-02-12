//! Stream health monitoring
//!
//! Pure-logic state machine that tracks audio sample flow to detect
//! no-audio and stall conditions. No I/O or audio hardware dependency.

use std::time::{Duration, Instant};

use crate::config::timeouts::{BUFFERING_TIMEOUT_SECS, STREAM_STALL_TIMEOUT_SECS};

/// Reason for health monitor failure
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// Probe succeeded but no samples were ever produced
    NoAudioOutput,
    /// Stream was healthy but stalled for too long
    StreamStall,
    /// Format probe failed (timeout, decode error, or thread panic)
    ProbeFailed,
}

/// Health monitor states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// Waiting for the first audio samples after probe
    WaitingForAudio,
    /// Receiving audio normally
    Healthy,
    /// Samples stopped flowing; may recover
    Stalled,
    /// Terminal — no recovery possible
    Failed(FailureReason),
}

/// Monitors stream health by tracking sample count over time
pub struct StreamHealthMonitor {
    state: HealthState,
    last_sample_count: u64,
    state_entered: Instant,
    no_audio_timeout: Duration,
    stall_timeout: Duration,
}

impl Default for StreamHealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamHealthMonitor {
    /// Create a new monitor using default config timeouts
    pub fn new() -> Self {
        Self {
            state: HealthState::WaitingForAudio,
            last_sample_count: 0,
            state_entered: Instant::now(),
            no_audio_timeout: Duration::from_secs(BUFFERING_TIMEOUT_SECS),
            stall_timeout: Duration::from_secs(STREAM_STALL_TIMEOUT_SECS),
        }
    }

    /// Create a monitor with custom timeouts (for testing)
    pub fn with_timeouts(no_audio: Duration, stall: Duration) -> Self {
        Self {
            state: HealthState::WaitingForAudio,
            last_sample_count: 0,
            state_entered: Instant::now(),
            no_audio_timeout: no_audio,
            stall_timeout: stall,
        }
    }

    /// Update with the current sample count. Returns Some(reason) on failure.
    pub fn update(&mut self, sample_count: u64) -> Option<FailureReason> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.state_entered);
        let samples_changed = sample_count > self.last_sample_count;

        match self.state {
            HealthState::WaitingForAudio => {
                if samples_changed {
                    self.last_sample_count = sample_count;
                    self.transition(HealthState::Healthy, now);
                    None
                } else if elapsed >= self.no_audio_timeout {
                    self.transition(HealthState::Failed(FailureReason::NoAudioOutput), now);
                    Some(FailureReason::NoAudioOutput)
                } else {
                    None
                }
            }
            HealthState::Healthy => {
                if samples_changed {
                    self.last_sample_count = sample_count;
                    None
                } else if elapsed >= self.stall_timeout {
                    self.transition(HealthState::Stalled, now);
                    None
                } else {
                    None
                }
            }
            HealthState::Stalled => {
                if samples_changed {
                    self.last_sample_count = sample_count;
                    self.transition(HealthState::Healthy, now);
                    None
                } else if elapsed >= self.stall_timeout {
                    // Signal stall but stay in Stalled — allow recovery.
                    // The stream's reconnection logic (ICY/HLS) may still
                    // recover; sink.empty() handles permanent stream death.
                    self.state_entered = now;
                    Some(FailureReason::StreamStall)
                } else {
                    None
                }
            }
            HealthState::Failed(_) => {
                // Terminal state — no further transitions
                None
            }
        }
    }

    /// Get the current health state
    pub fn state(&self) -> &HealthState {
        &self.state
    }

    /// Check if the monitor has reached a terminal failure state
    pub fn is_failed(&self) -> bool {
        matches!(self.state, HealthState::Failed(_))
    }

    /// Reset the stall timer (call while adaptive buffer is rebuffering
    /// to prevent false stall detection during intentional buffering).
    pub fn reset_stall_timer(&mut self) {
        self.state_entered = Instant::now();
    }

    fn transition(&mut self, new_state: HealthState, now: Instant) {
        self.state = new_state;
        self.state_entered = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn short_monitor() -> StreamHealthMonitor {
        StreamHealthMonitor::with_timeouts(Duration::from_millis(100), Duration::from_millis(100))
    }

    // --- Initial state ---

    #[test]
    fn initial_state_is_waiting() {
        let monitor = StreamHealthMonitor::new();
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);
        assert!(!monitor.is_failed());
    }

    // --- WaitingForAudio transitions ---

    #[test]
    fn waiting_to_healthy_on_samples() {
        let mut monitor = short_monitor();
        assert!(monitor.update(100).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    #[test]
    fn waiting_stays_if_no_samples_and_no_timeout() {
        let mut monitor = short_monitor();
        assert!(monitor.update(0).is_none());
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);
    }

    #[test]
    fn waiting_to_failed_no_audio_on_timeout() {
        let mut monitor = short_monitor();
        thread::sleep(Duration::from_millis(150));
        let result = monitor.update(0);
        assert_eq!(result, Some(FailureReason::NoAudioOutput));
        assert_eq!(
            *monitor.state(),
            HealthState::Failed(FailureReason::NoAudioOutput)
        );
        assert!(monitor.is_failed());
    }

    #[test]
    fn waiting_to_healthy_just_before_timeout() {
        let mut monitor = short_monitor();
        thread::sleep(Duration::from_millis(80));
        assert!(monitor.update(50).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    // --- Healthy transitions ---

    #[test]
    fn healthy_stays_healthy_with_samples() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        assert!(monitor.update(200).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    #[test]
    fn healthy_to_stalled_when_samples_stop() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        thread::sleep(Duration::from_millis(150));
        assert!(monitor.update(100).is_none()); // same count
        assert_eq!(*monitor.state(), HealthState::Stalled);
    }

    #[test]
    fn healthy_stays_if_samples_stop_briefly() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        thread::sleep(Duration::from_millis(50));
        assert!(monitor.update(100).is_none()); // same count but not timed out
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    // --- Stalled transitions ---

    #[test]
    fn stalled_recovers_on_new_samples() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // → Stalled
        assert_eq!(*monitor.state(), HealthState::Stalled);

        assert!(monitor.update(200).is_none()); // recovery
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    #[test]
    fn stalled_signals_but_stays_stalled() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // → Stalled
        assert_eq!(*monitor.state(), HealthState::Stalled);

        thread::sleep(Duration::from_millis(150));
        let result = monitor.update(100); // same count, second timeout
        assert_eq!(result, Some(FailureReason::StreamStall));
        // Stays Stalled (not Failed) — allows recovery
        assert_eq!(*monitor.state(), HealthState::Stalled);
        assert!(!monitor.is_failed());
    }

    #[test]
    fn stalled_stays_stalled_if_not_timed_out() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // → Stalled

        thread::sleep(Duration::from_millis(50));
        assert!(monitor.update(100).is_none());
        assert_eq!(*monitor.state(), HealthState::Stalled);
    }

    // --- Failed is terminal ---

    #[test]
    fn failed_no_audio_is_terminal() {
        let mut monitor = short_monitor();
        thread::sleep(Duration::from_millis(150));
        monitor.update(0); // → Failed(NoAudioOutput)
        assert!(monitor.is_failed());

        // Further updates do nothing
        assert!(monitor.update(1000).is_none());
        assert_eq!(
            *monitor.state(),
            HealthState::Failed(FailureReason::NoAudioOutput)
        );
    }

    #[test]
    fn stalled_recovers_after_signal() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // → Stalled
        thread::sleep(Duration::from_millis(150));
        let result = monitor.update(100); // emits StreamStall, stays Stalled
        assert_eq!(result, Some(FailureReason::StreamStall));
        assert_eq!(*monitor.state(), HealthState::Stalled);

        // New samples arrive — recovery
        assert!(monitor.update(200).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
        assert!(!monitor.is_failed());
    }

    // --- Default timeouts ---

    #[test]
    fn default_new_uses_config_values() {
        let monitor = StreamHealthMonitor::new();
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);
        // Just verify it doesn't panic and starts in correct state
        assert!(!monitor.is_failed());
    }

    // --- Edge cases ---

    #[test]
    fn sample_count_must_increase_to_be_healthy() {
        let mut monitor = short_monitor();
        // Update with 0 repeatedly — never transitions to Healthy
        monitor.update(0);
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);
        monitor.update(0);
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);
    }

    #[test]
    fn large_sample_count_jump() {
        let mut monitor = short_monitor();
        assert!(monitor.update(u64::MAX).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    #[test]
    fn multiple_healthy_cycles() {
        let mut monitor = short_monitor();
        // Go through Healthy → Stalled → Healthy multiple times
        for i in 1..=3u64 {
            monitor.update(i * 100); // → Healthy
            assert_eq!(*monitor.state(), HealthState::Healthy);
            thread::sleep(Duration::from_millis(150));
            monitor.update(i * 100); // → Stalled
            assert_eq!(*monitor.state(), HealthState::Stalled);
        }
        // Recover one more time
        monitor.update(500);
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    #[test]
    fn with_timeouts_custom_values() {
        let monitor =
            StreamHealthMonitor::with_timeouts(Duration::from_secs(60), Duration::from_secs(120));
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);
    }

    // --- FailureReason traits ---

    #[test]
    fn failure_reason_debug_and_eq() {
        assert_eq!(FailureReason::NoAudioOutput, FailureReason::NoAudioOutput);
        assert_ne!(FailureReason::NoAudioOutput, FailureReason::StreamStall);
        let _ = format!("{:?}", FailureReason::StreamStall);
    }

    #[test]
    fn health_state_debug_and_eq() {
        assert_eq!(HealthState::WaitingForAudio, HealthState::WaitingForAudio);
        assert_ne!(HealthState::Healthy, HealthState::Stalled);
        assert_eq!(
            HealthState::Failed(FailureReason::StreamStall),
            HealthState::Failed(FailureReason::StreamStall)
        );
        assert_ne!(
            HealthState::Failed(FailureReason::NoAudioOutput),
            HealthState::Failed(FailureReason::StreamStall)
        );
        let _ = format!("{:?}", HealthState::Healthy);
    }

    // --- Rapid updates ---

    #[test]
    fn rapid_updates_same_count_before_timeout() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
                             // Rapid updates with same count shouldn't stall if within timeout
        for _ in 0..10 {
            assert!(monitor.update(100).is_none());
        }
        // Still Healthy because we haven't exceeded the stall timeout
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    #[test]
    fn rapid_updates_increasing_count_stays_healthy() {
        let mut monitor = short_monitor();
        for i in 1..=100u64 {
            assert!(monitor.update(i * 10).is_none());
            // First update transitions to Healthy, rest stay Healthy
            assert_eq!(*monitor.state(), HealthState::Healthy);
        }
    }

    #[test]
    fn rapid_updates_after_stall_all_return_none() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // → Stalled
        assert_eq!(*monitor.state(), HealthState::Stalled);
        // Rapid updates with same count while stalled but before second timeout
        for _ in 0..5 {
            assert!(monitor.update(100).is_none());
            assert_eq!(*monitor.state(), HealthState::Stalled);
        }
    }

    // --- Zero/instant timeouts ---

    #[test]
    fn zero_no_audio_timeout_fails_immediately() {
        let mut monitor = StreamHealthMonitor::with_timeouts(
            Duration::from_millis(0),
            Duration::from_millis(100),
        );
        // Even with zero timeout, first update with count=0 should fail
        let result = monitor.update(0);
        assert_eq!(result, Some(FailureReason::NoAudioOutput));
        assert!(monitor.is_failed());
    }

    #[test]
    fn zero_stall_timeout_stalls_immediately_after_healthy() {
        let mut monitor = StreamHealthMonitor::with_timeouts(
            Duration::from_millis(100),
            Duration::from_millis(0),
        );
        monitor.update(100); // → Healthy
                             // With zero stall timeout, same count should immediately stall
        monitor.update(100); // → Stalled
        assert_eq!(*monitor.state(), HealthState::Stalled);
        // Emits StreamStall signal but stays Stalled
        let result = monitor.update(100);
        assert_eq!(result, Some(FailureReason::StreamStall));
        assert_eq!(*monitor.state(), HealthState::Stalled);
    }

    #[test]
    fn zero_timeout_but_samples_arrive_goes_healthy() {
        let mut monitor =
            StreamHealthMonitor::with_timeouts(Duration::from_millis(0), Duration::from_millis(0));
        // If samples arrive immediately, should still go Healthy
        assert!(monitor.update(100).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    // --- is_failed across all states ---

    #[test]
    fn is_failed_false_in_waiting() {
        let monitor = short_monitor();
        assert!(!monitor.is_failed());
    }

    #[test]
    fn is_failed_false_in_healthy() {
        let mut monitor = short_monitor();
        monitor.update(100);
        assert!(!monitor.is_failed());
    }

    #[test]
    fn is_failed_false_in_stalled() {
        let mut monitor = short_monitor();
        monitor.update(100);
        thread::sleep(Duration::from_millis(150));
        monitor.update(100);
        assert_eq!(*monitor.state(), HealthState::Stalled);
        assert!(!monitor.is_failed());
    }

    #[test]
    fn is_failed_true_for_no_audio() {
        let mut monitor = short_monitor();
        thread::sleep(Duration::from_millis(150));
        monitor.update(0);
        assert!(monitor.is_failed());
    }

    #[test]
    fn stall_does_not_reach_failed() {
        let mut monitor = short_monitor();
        monitor.update(100);
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // → Stalled
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // signals StreamStall, stays Stalled
        assert!(!monitor.is_failed());
        assert_eq!(*monitor.state(), HealthState::Stalled);
    }

    // --- Full lifecycle ---

    #[test]
    fn full_lifecycle_waiting_healthy_stalled_recovered() {
        let mut monitor = short_monitor();

        // Phase 1: WaitingForAudio
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);
        assert!(!monitor.is_failed());
        assert!(monitor.update(0).is_none()); // still waiting
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);

        // Phase 2: Healthy
        assert!(monitor.update(100).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
        assert!(!monitor.is_failed());

        // Phase 3: Stay healthy with increasing samples
        assert!(monitor.update(200).is_none());
        assert!(monitor.update(300).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);

        // Phase 4: Stalled
        thread::sleep(Duration::from_millis(150));
        assert!(monitor.update(300).is_none()); // stall timeout exceeded
        assert_eq!(*monitor.state(), HealthState::Stalled);
        assert!(!monitor.is_failed());

        // Phase 5: StreamStall signal emitted, stays Stalled
        thread::sleep(Duration::from_millis(150));
        let result = monitor.update(300);
        assert_eq!(result, Some(FailureReason::StreamStall));
        assert_eq!(*monitor.state(), HealthState::Stalled);
        assert!(!monitor.is_failed());

        // Phase 6: Recovery — new samples arrive
        assert!(monitor.update(400).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
        assert!(!monitor.is_failed());
    }

    #[test]
    fn full_lifecycle_with_recovery() {
        let mut monitor = short_monitor();

        // WaitingForAudio → Healthy
        monitor.update(100);
        assert_eq!(*monitor.state(), HealthState::Healthy);

        // Healthy → Stalled
        thread::sleep(Duration::from_millis(150));
        monitor.update(100);
        assert_eq!(*monitor.state(), HealthState::Stalled);

        // Stalled → Healthy (recovery!)
        monitor.update(200);
        assert_eq!(*monitor.state(), HealthState::Healthy);

        // Stay healthy
        monitor.update(300);
        assert_eq!(*monitor.state(), HealthState::Healthy);

        // Stall again
        thread::sleep(Duration::from_millis(150));
        monitor.update(300);
        assert_eq!(*monitor.state(), HealthState::Stalled);

        // Recover again
        monitor.update(400);
        assert_eq!(*monitor.state(), HealthState::Healthy);
        assert!(!monitor.is_failed());
    }

    // --- Healthy timer resets on sample increase ---

    #[test]
    fn healthy_resets_stall_timer_on_sample_increase() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy

        // Wait 80ms (below 100ms stall timeout)
        thread::sleep(Duration::from_millis(80));
        monitor.update(200); // reset timer

        // Wait another 80ms (160ms total, but timer was reset)
        thread::sleep(Duration::from_millis(80));
        monitor.update(300); // reset timer again

        // Should still be Healthy because timer keeps resetting
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    // --- Failed returns None on many repeated calls ---

    #[test]
    fn failed_returns_none_on_many_repeated_calls() {
        let mut monitor = short_monitor();
        thread::sleep(Duration::from_millis(150));
        let result = monitor.update(0);
        assert_eq!(result, Some(FailureReason::NoAudioOutput));

        // Call update 100 times — should always return None
        for i in 0..100u64 {
            assert!(
                monitor.update(i).is_none(),
                "Failed monitor should return None on call {}",
                i
            );
        }
    }

    // --- Failure reason distinction ---

    #[test]
    fn no_audio_and_stall_are_distinct_failure_paths() {
        // Path 1: NoAudioOutput (never get samples) — terminal
        let mut m1 = short_monitor();
        thread::sleep(Duration::from_millis(150));
        assert_eq!(m1.update(0), Some(FailureReason::NoAudioOutput));
        assert!(m1.is_failed());

        // Path 2: StreamStall (got samples then lost them) — recoverable
        let mut m2 = short_monitor();
        m2.update(100); // → Healthy
        thread::sleep(Duration::from_millis(150));
        m2.update(100); // → Stalled
        thread::sleep(Duration::from_millis(150));
        assert_eq!(m2.update(100), Some(FailureReason::StreamStall));
        assert!(!m2.is_failed()); // stays Stalled, not Failed

        // Verify they're different
        assert_ne!(FailureReason::NoAudioOutput, FailureReason::StreamStall);
    }

    // --- Stalled recovery timing ---

    #[test]
    fn stalled_recovery_resets_stall_timer() {
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy

        // Stall
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // → Stalled

        // Recover
        monitor.update(200); // → Healthy
        assert_eq!(*monitor.state(), HealthState::Healthy);

        // Wait 80ms — should still be Healthy (timer was reset on recovery)
        thread::sleep(Duration::from_millis(80));
        monitor.update(200); // same count, but within timeout
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    // --- Clone/Copy on FailureReason ---

    #[test]
    fn failure_reason_copy_and_clone() {
        let reason = FailureReason::NoAudioOutput;
        let copied = reason; // Copy
        let cloned = reason.clone();
        assert_eq!(reason, copied);
        assert_eq!(reason, cloned);
    }

    #[test]
    fn health_state_copy_and_clone() {
        let state = HealthState::Healthy;
        let copied = state; // Copy
        let cloned = state.clone();
        assert_eq!(state, copied);
        assert_eq!(state, cloned);

        let failed = HealthState::Failed(FailureReason::StreamStall);
        let failed_copy = failed;
        assert_eq!(failed, failed_copy);
    }

    // --- WaitingForAudio with exact count=0 vs count=1 ---

    #[test]
    fn waiting_with_count_0_stays_waiting() {
        let mut monitor = short_monitor();
        // count=0 means no samples — stay waiting
        assert!(monitor.update(0).is_none());
        assert_eq!(*monitor.state(), HealthState::WaitingForAudio);
    }

    #[test]
    fn waiting_with_count_1_goes_healthy() {
        let mut monitor = short_monitor();
        // count=1 means at least one sample — go healthy
        assert!(monitor.update(1).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    // --- reset_stall_timer interaction ---

    #[test]
    fn reset_once_allows_stall_detection() {
        // Simulates the fix: reset_stall_timer fires once on buffering transition,
        // then the monitor is left alone so the stall timer can accumulate.
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy

        // Single reset (as on buffering transition)
        monitor.reset_stall_timer();

        // Wait for stall timeout to elapse
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // same count → should detect stall
        assert_eq!(*monitor.state(), HealthState::Stalled);

        // Wait again → emits StreamStall signal, stays Stalled
        thread::sleep(Duration::from_millis(150));
        let result = monitor.update(100);
        assert_eq!(result, Some(FailureReason::StreamStall));
        assert_eq!(*monitor.state(), HealthState::Stalled);
    }

    #[test]
    fn stalled_emits_repeated_signals() {
        // StreamStall is emitted every stall_timeout while stalled,
        // allowing the engine to track how long the stall persists.
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy
        thread::sleep(Duration::from_millis(150));
        monitor.update(100); // → Stalled

        for _ in 0..3 {
            thread::sleep(Duration::from_millis(150));
            let result = monitor.update(100);
            assert_eq!(result, Some(FailureReason::StreamStall));
            assert_eq!(*monitor.state(), HealthState::Stalled);
        }

        // Still recoverable
        assert!(monitor.update(200).is_none());
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }

    #[test]
    fn continuous_reset_prevents_stall_detection() {
        // Regression guard: documents that continuously resetting the stall timer
        // (the old bug) prevents stall detection even after a long time.
        let mut monitor = short_monitor();
        monitor.update(100); // → Healthy

        // Simulate the old bug: reset every 50ms for 500ms (5x the stall timeout)
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(50));
            monitor.reset_stall_timer();
            monitor.update(100); // same count
        }

        // Still Healthy because the timer was continuously reset
        assert_eq!(*monitor.state(), HealthState::Healthy);
    }
}
