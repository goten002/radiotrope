//! Audio output device handling
//!
//! Opens the output device with an error callback that notices when the
//! device goes away (unplugged USB or Bluetooth headphones, a disabled
//! device), and moves the playing source to a newly opened device without
//! restarting the stream.

use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::cpal::traits::HostTrait;
use rodio::cpal::StreamError;
use rodio::{DeviceSinkBuilder, DeviceTrait, MixerDeviceSink, Source};

/// Samples a [`Relay`] takes from its source per lock
const RELAY_BATCH: usize = 1024;

/// Errors that aren't a loss on their own, within one second, that mean the
/// device is gone: ALSA reports an unplugged device as a stream of
/// backend errors rather than `DeviceNotAvailable`
const BACKEND_ERROR_BURST: u32 = 20;

/// The playing source, shared between the engine and the player's relay
pub(crate) type SharedSource = Arc<Mutex<Box<dyn Source + Send>>>;

/// Plays a source through whichever output device is current.
///
/// The player gets a `Relay`, which pulls samples from the shared source in
/// batches. When the device is replaced, [`Relay::resume`] makes a new relay
/// over the same source for the new player, and the old relay goes with the
/// old player. The engine keeps the source (see [`Relay::source`]) while it
/// plays: a device that fails can take its player's queue, and the relay in
/// it, down with its audio thread.
///
/// Spans, channels and sample rate mirror the source's exactly, so rodio
/// sees the same format changes it would see without the relay.
pub(crate) struct Relay {
    source: SharedSource,
    batch: Vec<f32>,
    pos: usize,
    channels: NonZero<u16>,
    sample_rate: NonZero<u32>,
    /// What the source said was left of its span after this batch
    span_rest: Option<usize>,
}

impl Relay {
    pub(crate) fn new(source: impl Source + Send + 'static) -> Self {
        let source: Box<dyn Source + Send> = Box::new(source);
        Self::over(Arc::new(Mutex::new(source)))
    }

    fn over(source: SharedSource) -> Self {
        let (channels, sample_rate) = {
            let s = lock(&source);
            (s.channels(), s.sample_rate())
        };
        Self {
            source,
            batch: Vec::with_capacity(RELAY_BATCH),
            pos: 0,
            channels,
            sample_rate,
            span_rest: None,
        }
    }

    /// The source this relay plays, for [`Relay::resume`]
    pub(crate) fn source(&self) -> SharedSource {
        self.source.clone()
    }

    /// A new relay over `source`, for another player. Samples an earlier
    /// relay had taken but not played yet are skipped.
    pub(crate) fn resume(source: &SharedSource) -> Self {
        Self::over(source.clone())
    }

    /// Take the next batch from the source, within its current span so the
    /// batch has one format. Returns false at the end of the source.
    fn refill(&mut self) -> bool {
        let mut source = lock(&self.source);
        self.batch.clear();
        self.pos = 0;
        // The first sample can start a new span, so read the format after it
        let Some(first) = source.next() else {
            return false;
        };
        self.batch.push(first);
        self.channels = source.channels();
        self.sample_rate = source.sample_rate();
        let span_left = source.current_span_len();
        let want = RELAY_BATCH.min(span_left.map_or(usize::MAX, |n| n + 1));
        while self.batch.len() < want {
            match source.next() {
                Some(sample) => self.batch.push(sample),
                None => break,
            }
        }
        self.span_rest = source.current_span_len();
        true
    }

    fn batch_left(&self) -> usize {
        self.batch.len() - self.pos
    }
}

impl Iterator for Relay {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.batch_left() == 0 && !self.refill() {
            return None;
        }
        let sample = self.batch[self.pos];
        self.pos += 1;
        Some(sample)
    }
}

impl Source for Relay {
    fn current_span_len(&self) -> Option<usize> {
        match self.batch_left() {
            0 => lock(&self.source).current_span_len(),
            left => self.span_rest.map(|rest| left + rest),
        }
    }

    fn channels(&self) -> NonZero<u16> {
        match self.batch_left() {
            0 => lock(&self.source).channels(),
            _ => self.channels,
        }
    }

    fn sample_rate(&self) -> NonZero<u32> {
        match self.batch_left() {
            0 => lock(&self.source).sample_rate(),
            _ => self.sample_rate,
        }
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

fn lock(source: &SharedSource) -> std::sync::MutexGuard<'_, Box<dyn Source + Send>> {
    source.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tells from an output stream's errors whether its device is gone, and
/// raises `lost` if this is still the engine's current device.
#[derive(Clone)]
pub(crate) struct OutputErrors {
    lost: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    /// The device generation this stream belongs to
    mine: u64,
    burst_start: Instant,
    burst: u32,
    last_logged: Option<Instant>,
}

impl OutputErrors {
    pub(crate) fn new(lost: Arc<AtomicBool>, generation: Arc<AtomicU64>, mine: u64) -> Self {
        Self {
            lost,
            generation,
            mine,
            burst_start: Instant::now(),
            burst: 0,
            last_logged: None,
        }
    }

    /// Called by cpal on the audio thread
    pub(crate) fn report(&mut self, err: StreamError) {
        let gone = self.is_gone(&err, Instant::now());
        if !matches!(err, StreamError::BufferUnderrun)
            && self
                .last_logged
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            eprintln!("Audio output error: {err}");
            self.last_logged = Some(Instant::now());
        }
        // An old device's stream can still report after it was replaced
        if gone && self.generation.load(Ordering::SeqCst) == self.mine {
            self.lost.store(true, Ordering::SeqCst);
        }
    }

    fn is_gone(&mut self, err: &StreamError, now: Instant) -> bool {
        match err {
            StreamError::DeviceNotAvailable | StreamError::StreamInvalidated => true,
            StreamError::BufferUnderrun => false,
            StreamError::BackendSpecific { .. } => {
                if now.duration_since(self.burst_start) > Duration::from_secs(1) {
                    self.burst_start = now;
                    self.burst = 0;
                }
                self.burst += 1;
                self.burst >= BACKEND_ERROR_BURST
            }
        }
    }
}

/// Open the default output device, or failing that any other real one (as
/// rodio's `open_default_sink` does), reporting its loss through `lost`.
/// Each call starts a new device generation: errors from earlier devices no
/// longer count.
pub(crate) fn open_output(
    lost: &Arc<AtomicBool>,
    generation: &Arc<AtomicU64>,
) -> Result<MixerDeviceSink, String> {
    let mine = generation.fetch_add(1, Ordering::SeqCst) + 1;
    lost.store(false, Ordering::SeqCst);
    let errors = OutputErrors::new(lost.clone(), generation.clone(), mine);
    let open = |builder: DeviceSinkBuilder| {
        let mut errors = errors.clone();
        builder
            .with_error_callback(move |err| errors.report(err))
            .open_sink_or_fallback()
    };

    let mut sink = DeviceSinkBuilder::from_default_device()
        .and_then(open)
        .or_else(|original| {
            let Ok(devices) = rodio::cpal::default_host().output_devices() else {
                return Err(original);
            };
            devices
                .filter(|device| {
                    device
                        .description()
                        .is_ok_and(|d| d.driver().is_some_and(|driver| driver != "null"))
                })
                .find_map(|device| DeviceSinkBuilder::from_device(device).and_then(open).ok())
                .ok_or(original)
        })
        .map_err(|e| e.to_string())?;
    sink.log_on_drop(false);
    Ok(sink)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source made of spans of the given (channels, rate, samples); sample
    /// values count up from 0
    struct Spans {
        spans: Vec<(u16, u32, usize)>,
        span: usize,
        left: usize,
        next_value: f32,
    }

    impl Spans {
        fn new(spans: &[(u16, u32, usize)]) -> Self {
            Self {
                spans: spans.to_vec(),
                span: 0,
                left: spans[0].2,
                next_value: 0.0,
            }
        }
    }

    impl Iterator for Spans {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            while self.left == 0 {
                self.span += 1;
                self.left = self.spans.get(self.span)?.2;
            }
            self.left -= 1;
            self.next_value += 1.0;
            Some(self.next_value - 1.0)
        }
    }

    impl Source for Spans {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.left)
        }
        fn channels(&self) -> NonZero<u16> {
            let i = self.span.min(self.spans.len() - 1);
            NonZero::new(self.spans[i].0).unwrap()
        }
        fn sample_rate(&self) -> NonZero<u32> {
            let i = self.span.min(self.spans.len() - 1);
            NonZero::new(self.spans[i].1).unwrap()
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    /// Every sample with the format and span length reported just before it
    fn trace(source: &mut dyn Source) -> Vec<(f32, u16, u32, Option<usize>)> {
        let mut out = Vec::new();
        loop {
            let (ch, rate, span) = (
                source.channels().get(),
                source.sample_rate().get(),
                source.current_span_len(),
            );
            match source.next() {
                Some(s) => out.push((s, ch, rate, span)),
                None => return out,
            }
        }
    }

    const SPANS: &[(u16, u32, usize)] = &[(1, 44_100, 3000), (2, 48_000, 700), (2, 22_050, 2500)];

    #[test]
    fn relay_is_indistinguishable_from_its_source() {
        let direct = trace(&mut Spans::new(SPANS));
        let relayed = trace(&mut Relay::new(Spans::new(SPANS)));
        assert_eq!(direct.len(), 6200);
        assert!(
            direct == relayed,
            "the relay changed samples, format or spans"
        );
    }

    #[test]
    fn resumed_relay_continues_where_the_source_is() {
        let mut first = Relay::new(Spans::new(SPANS));
        let played: Vec<f32> = first.by_ref().take(10).collect();
        assert_eq!(played, (0..10).map(|i| i as f32).collect::<Vec<_>>());

        // The device goes away: a new relay picks up the same source
        let mut second = Relay::resume(&first.source());
        drop(first);
        let rest: Vec<f32> = second.by_ref().collect();
        // What the first relay had taken but not played is skipped, nothing
        // is repeated and the order holds
        assert!(rest[0] >= 10.0);
        assert!(rest.windows(2).all(|w| w[1] == w[0] + 1.0));
        assert_eq!(*rest.last().unwrap(), 6199.0);
    }

    fn new_errors() -> (OutputErrors, Arc<AtomicBool>, Arc<AtomicU64>) {
        let lost = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(1));
        (
            OutputErrors::new(lost.clone(), generation.clone(), 1),
            lost,
            generation,
        )
    }

    fn backend(msg: &str) -> StreamError {
        StreamError::BackendSpecific {
            err: rodio::cpal::BackendSpecificError {
                description: msg.to_string(),
            },
        }
    }

    #[test]
    fn an_unplugged_device_is_lost() {
        let (mut errors, lost, _) = new_errors();
        errors.report(StreamError::BufferUnderrun);
        errors.report(backend("glitch"));
        assert!(
            !lost.load(Ordering::SeqCst),
            "one-off errors are not a loss"
        );
        errors.report(StreamError::DeviceNotAvailable);
        assert!(lost.load(Ordering::SeqCst));
    }

    #[test]
    fn a_burst_of_backend_errors_is_a_lost_device() {
        let (mut errors, _, _) = new_errors();
        let start = Instant::now();
        let gone: Vec<bool> = (0..BACKEND_ERROR_BURST)
            .map(|_| errors.is_gone(&backend("POLLERR"), start))
            .collect();
        assert!(!gone[..gone.len() - 1].iter().any(|g| *g));
        assert!(gone[gone.len() - 1]);

        // Spread out, the same errors are not
        let (mut errors, _, _) = new_errors();
        let gone = (0..BACKEND_ERROR_BURST * 2).any(|i| {
            let at = start + Duration::from_millis(600) * i;
            errors.is_gone(&backend("glitch"), at)
        });
        assert!(!gone);
    }

    #[test]
    fn a_replaced_device_can_no_longer_raise_the_flag() {
        let (mut errors, lost, generation) = new_errors();
        generation.store(2, Ordering::SeqCst);
        errors.report(StreamError::DeviceNotAvailable);
        assert!(!lost.load(Ordering::SeqCst));
    }
}
