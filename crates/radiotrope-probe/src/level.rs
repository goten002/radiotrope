//! Sound level of the decoded audio, and a model of a listener's player

/// Levels are measured in blocks of this many seconds
const BLOCK_SECS: f64 = 1.0;

/// A partial block at the end counts if it holds at least this share of one
const MIN_PARTIAL_BLOCK: f64 = 0.25;

/// Measures the decoded samples: level per block, peak, and the format they
/// came in. The decoding thread feeds it; the check reads it at the end.
#[derive(Debug, Default)]
pub(crate) struct Levels {
    /// Mean square of each finished block
    blocks: Vec<f64>,
    /// Loudest sample so far (absolute)
    peak: f32,
    /// The block being filled
    sum_sq: f64,
    count: usize,
    block_len: usize,
    /// Format of the latest samples
    pub sample_rate: u32,
    pub channels: u16,
}

impl Levels {
    /// Set the format of the samples to come
    pub fn set_format(&mut self, sample_rate: u32, channels: u16) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.block_len = ((sample_rate as f64 * channels as f64 * BLOCK_SECS) as usize).max(1);
    }

    /// Add one block's worth of samples or fewer. Returns true when a block
    /// was finished (a good time to look at the format again).
    pub fn push(&mut self, samples: &[f32]) -> bool {
        let mut finished = false;
        for &s in samples {
            // A non-finite sample is broken audio, not loud audio
            let s = if s.is_finite() { s } else { 0.0 };
            self.sum_sq += (s as f64) * (s as f64);
            self.peak = self.peak.max(s.abs());
            self.count += 1;
            if self.count >= self.block_len {
                self.finish_block();
                finished = true;
            }
        }
        finished
    }

    fn finish_block(&mut self) {
        if self.count > 0 {
            self.blocks.push(self.sum_sq / self.count as f64);
        }
        self.sum_sq = 0.0;
        self.count = 0;
    }

    /// The blocks heard, counting a partial last block if it is long enough
    fn all_blocks(&self) -> Vec<f64> {
        let mut blocks = self.blocks.clone();
        if self.count > 0 && self.count as f64 >= self.block_len as f64 * MIN_PARTIAL_BLOCK {
            blocks.push(self.sum_sq / self.count as f64);
        }
        blocks
    }

    /// Average level in dBFS, if any audio was measured
    pub fn rms_dbfs(&self) -> Option<f64> {
        let blocks = self.all_blocks();
        if blocks.is_empty() {
            return None;
        }
        let mean_sq = blocks.iter().sum::<f64>() / blocks.len() as f64;
        Some(to_db(mean_sq.sqrt()))
    }

    /// Loudest sample in dBFS, if any audio was measured
    pub fn peak_dbfs(&self) -> Option<f64> {
        (self.blocks.len() + self.count > 0).then(|| to_db(self.peak as f64))
    }

    /// Seconds of blocks quieter than `threshold_db`, and seconds measured
    pub fn silence(&self, threshold_db: f64) -> (f64, f64) {
        let blocks = self.all_blocks();
        let silent = blocks
            .iter()
            .filter(|ms| to_db(ms.sqrt()) < threshold_db)
            .count();
        (silent as f64 * BLOCK_SECS, blocks.len() as f64 * BLOCK_SECS)
    }
}

/// dB below full scale; silence is floored at -120
pub(crate) fn to_db(amplitude: f64) -> f64 {
    (20.0 * amplitude.max(1e-6).log10()).max(-120.0)
}

/// Audio a player holds before it starts, and before it starts again after
/// running dry
const PLAYER_BUFFER_SECS: f64 = 1.0;

/// A listener's player, fed the audio as fast as it arrives. It tells
/// whether the station keeps up with real time: when the audio received
/// runs out the player stops (a stall) until it has a buffer again.
///
/// The decoder's progress alone can't say this: HLS stations deliver a whole
/// segment of several seconds at once, so pauses between deliveries are
/// normal as long as the audio stays ahead of playback.
#[derive(Debug, Default)]
pub(crate) struct Player {
    played: f64,
    playing: bool,
    started: bool,
    pub stalls: u32,
    pub stalled_secs: f64,
}

impl Player {
    /// `received` seconds of audio have come in so far; `dt` seconds of wall
    /// time passed since the last call
    pub fn advance(&mut self, received: f64, dt: f64) {
        let available = received - self.played;
        if self.playing {
            if available <= 0.0 {
                self.playing = false;
                self.stalls += 1;
                self.stalled_secs += dt;
            } else {
                // It plays what it has, and runs dry within this tick if
                // that was less than `dt`
                let played = dt.min(available);
                self.played += played;
                if played < dt {
                    self.playing = false;
                    self.stalls += 1;
                    self.stalled_secs += dt - played;
                }
            }
        } else if available >= PLAYER_BUFFER_SECS {
            self.playing = true;
            self.started = true;
        } else if self.started {
            self.stalled_secs += dt;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amplitude: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| amplitude * (i as f32 * 0.05).sin())
            .collect()
    }

    #[test]
    fn a_full_scale_square_wave_is_zero_db() {
        let mut levels = Levels::default();
        levels.set_format(1000, 1);
        let square: Vec<f32> = (0..2000)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        levels.push(&square);
        assert!(levels.rms_dbfs().unwrap().abs() < 0.01);
        assert!(levels.peak_dbfs().unwrap().abs() < 0.01);
        assert_eq!(levels.silence(-50.0), (0.0, 2.0));
    }

    #[test]
    fn silence_is_counted_by_the_second() {
        let mut levels = Levels::default();
        levels.set_format(1000, 2);
        levels.push(&tone(0.5, 2000)); // 1 s of stereo
        levels.push(&vec![0.0; 4000]); // 2 s of silence
        levels.push(&tone(0.0001, 2000)); // 1 s far below -50 dBFS
        let (silent, heard) = levels.silence(-50.0);
        assert_eq!((silent, heard), (3.0, 4.0));
    }

    #[test]
    fn nothing_heard_has_no_level() {
        let levels = Levels::default();
        assert_eq!(levels.rms_dbfs(), None);
        assert_eq!(levels.peak_dbfs(), None);
        assert_eq!(levels.silence(-50.0), (0.0, 0.0));
    }

    #[test]
    fn a_short_last_block_counts_only_when_long_enough() {
        let mut levels = Levels::default();
        levels.set_format(1000, 1);
        levels.push(&vec![0.0; 1000]);
        levels.push(&vec![0.0; 100]); // 0.1 s: too short
        assert_eq!(levels.silence(-50.0), (1.0, 1.0));
        levels.push(&vec![0.0; 300]); // now 0.4 s
        assert_eq!(levels.silence(-50.0), (2.0, 2.0));
    }

    #[test]
    fn broken_samples_dont_count_as_loud() {
        let mut levels = Levels::default();
        levels.set_format(10, 1);
        levels.push(&[
            f32::NAN,
            f32::INFINITY,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ]);
        assert_eq!(levels.peak_dbfs(), Some(-120.0));
    }

    /// Feed the player audio arriving at `rate` times real time, after a
    /// `burst` of seconds, for `secs` seconds in 100 ms ticks
    fn run(player: &mut Player, burst: f64, rate: f64, secs: f64) {
        let dt = 0.1;
        let mut t = 0.0;
        while t < secs {
            t += dt;
            player.advance(burst + rate * t, dt);
        }
    }

    #[test]
    fn a_station_in_real_time_never_stalls() {
        let mut player = Player::default();
        run(&mut player, 2.0, 1.0, 10.0);
        assert_eq!(player.stalls, 0);
        assert_eq!(player.stalled_secs, 0.0);
    }

    #[test]
    fn a_station_at_half_speed_stalls() {
        let mut player = Player::default();
        run(&mut player, 0.0, 0.5, 10.0);
        assert!(player.stalls >= 2, "{player:?}");
        assert!(player.stalled_secs > 3.0, "{player:?}");
    }

    #[test]
    fn segments_in_bursts_dont_stall_while_they_stay_ahead() {
        // An HLS station: 6 s segments, three at the start, then one every 6 s
        let mut player = Player::default();
        let dt = 0.1;
        let mut t: f64 = 0.0;
        while t < 20.0 {
            t += dt;
            let segments = 3.0 + (t / 6.0).floor();
            player.advance(segments * 6.0, dt);
        }
        assert_eq!(player.stalls, 0, "{player:?}");
    }

    #[test]
    fn audio_that_stops_stalls_once() {
        let mut player = Player::default();
        let dt = 0.1;
        let mut t: f64 = 0.0;
        while t < 10.0 {
            t += dt;
            player.advance((2.0 + t).min(5.0), dt);
        }
        assert_eq!(player.stalls, 1, "{player:?}");
        assert!(player.stalled_secs > 4.5, "{player:?}");
    }
}
