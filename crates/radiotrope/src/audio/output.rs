//! Audio output device handling
//!
//! Opens the output device with an error callback that notices when the
//! device goes away (unplugged USB or Bluetooth headphones, a disabled
//! device). The engine then opens another and gives it a new output of the
//! playing station's queue ([`super::pcm::PcmFeed::output`]), so the
//! station carries on without restarting. On Linux only the default
//! device, the sound server's or the one it was on will do
//! ([`Devices::Reopen`]).
//!
//! On Windows the engine also moves playback when the default device
//! changes ([`DefaultWatch`]).
//!
//! Without a device, a [`SilentOutput`] takes the audio at a steady pace
//! and drops it, so the engine plays the same way (tests, headless use).

use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rodio::cpal::traits::HostTrait;
use rodio::cpal::{BufferSize, DeviceId};
use rodio::mixer::{self, Mixer};
use rodio::{
    CpalError, CpalErrorKind, DeviceSinkBuilder, DeviceSinkError, DeviceTrait, MixerDeviceSink,
};

/// How often a [`SilentOutput`] takes its next buffer of audio
const SILENT_PULL_INTERVAL: Duration = Duration::from_millis(10);

/// What the engine plays on
pub(crate) enum Output {
    Device(MixerDeviceSink),
    Silent(SilentOutput),
}

impl Output {
    /// Where players go to be heard
    pub(crate) fn mixer(&self) -> &Mixer {
        match self {
            Output::Device(sink) => sink.mixer(),
            Output::Silent(silent) => &silent.mixer,
        }
    }

    /// The device takes 16-bit samples, so what plays gets dither
    pub(crate) fn takes_16_bit(&self) -> bool {
        match self {
            Output::Device(sink) => matches!(
                sink.config().sample_format(),
                rodio::cpal::SampleFormat::I16 | rodio::cpal::SampleFormat::U16
            ),
            Output::Silent(_) => false,
        }
    }

    /// How much the output takes at a time, and its sample rate
    pub(crate) fn buffer(&self) -> (BufferSize, NonZero<u32>) {
        match self {
            Output::Device(sink) => (*sink.config().buffer_size(), sink.config().sample_rate()),
            Output::Silent(silent) => (BufferSize::Fixed(silent.frames), silent.sample_rate),
        }
    }
}

/// The loudest sample a [`SilentOutput`] took since it was last asked: what
/// a device would have played, for tests of what is heard
#[derive(Debug, Default)]
pub(crate) struct OutputPeak(AtomicU32);

impl OutputPeak {
    fn heard(&self, peak: f32) {
        // Bits of non-negative floats are in the floats' order
        self.0.fetch_max(peak.to_bits(), Ordering::Relaxed);
    }

    /// The loudest sample since the last call, from 0
    pub(crate) fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }
}

/// An output with no device: a thread takes the mixed audio every
/// `SILENT_PULL_INTERVAL`, `speed` times as fast as it would play, and
/// drops it
pub(crate) struct SilentOutput {
    mixer: Mixer,
    sample_rate: NonZero<u32>,
    /// Frames taken each time
    frames: u32,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SilentOutput {
    /// `peak` is told how loud the audio it takes is
    pub(crate) fn open(speed: f32, peak: Arc<OutputPeak>) -> Result<Self, String> {
        let channels = NonZero::new(2).expect("non-zero");
        let sample_rate = NonZero::new(44_100).expect("non-zero");
        let (mixer, mut source) = mixer::mixer(channels, sample_rate);
        let frames = (sample_rate.get() as f32 * SILENT_PULL_INTERVAL.as_secs_f32() * speed)
            .round()
            .max(1.0) as u32;
        let samples = frames as usize * usize::from(channels.get());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = thread::Builder::new()
            .name("silent-output".to_string())
            .spawn(move || {
                let mut pace = Pace::new(Instant::now());
                while !stopped.load(Ordering::Relaxed) {
                    // An empty mixer returns None until a player joins it
                    let mut loudest = 0.0f32;
                    for _ in 0..samples {
                        if let Some(sample) = source.next() {
                            loudest = loudest.max(sample.abs());
                        }
                    }
                    if loudest > 0.0 {
                        peak.heard(loudest);
                    }
                    if let Some(wait) = pace.pulled(Instant::now()) {
                        thread::sleep(wait);
                    }
                }
            })
            .map_err(|e| format!("Failed to start the silent output: {e}"))?;
        Ok(Self {
            mixer,
            sample_rate,
            frames,
            stop,
            thread: Some(thread),
        })
    }
}

/// When a [`SilentOutput`] pulls: each pull on a schedule of its own, so
/// the time spent pulling doesn't add up into lag behind real time
struct Pace {
    next: Instant,
}

impl Pace {
    fn new(now: Instant) -> Self {
        Self { next: now }
    }

    /// A pull is done at `now`: how long to wait before the next one
    fn pulled(&mut self, now: Instant) -> Option<Duration> {
        self.next += SILENT_PULL_INTERVAL;
        match self.next.checked_duration_since(now) {
            Some(wait) => Some(wait),
            // Far behind (the machine was busy, or asleep): carry on from
            // now rather than race to catch up
            None if now - self.next > SILENT_PULL_INTERVAL * 10 => {
                self.next = now;
                None
            }
            None => None,
        }
    }
}

impl Drop for SilentOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Errors that aren't a loss on their own, within one second, that mean the
/// device is gone: a backend can report a device that went away as a stream
/// of errors it has no better kind for rather than `DeviceNotAvailable`
const BACKEND_ERROR_BURST: u32 = 20;

/// Tells from an output stream's errors whether its device is gone, and
/// raises `lost` if this is still the engine's current device.
#[derive(Clone)]
pub(crate) struct OutputErrors {
    lost: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    /// The device generation this stream belongs to
    mine: u64,
    /// Raised when this stream finds its device gone, current or not yet:
    /// a loss in the first moments of an open, before it becomes the
    /// current device, still counts (see [`OutputErrors::opened`])
    gone: Arc<AtomicBool>,
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
            gone: Arc::new(AtomicBool::new(false)),
            burst_start: Instant::now(),
            burst: 0,
            last_logged: None,
        }
    }

    /// A copy for one stream: its own `gone`, the rest shared
    fn for_stream(&self) -> Self {
        Self {
            gone: Arc::new(AtomicBool::new(false)),
            ..self.clone()
        }
    }

    /// The stream opened: it is the current device from now on. `lost`
    /// starts as whether it has already found its device gone, so a loss
    /// reported before now isn't wiped.
    fn opened(&self) {
        self.generation.store(self.mine, Ordering::SeqCst);
        self.lost
            .store(self.gone.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Called by cpal on the audio thread
    pub(crate) fn report(&mut self, err: CpalError) {
        let gone = self.is_gone(&err, Instant::now());
        if err.kind() != CpalErrorKind::Xrun
            && self
                .last_logged
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            log::warn!("Audio output error: {err}");
            self.last_logged = Some(Instant::now());
        }
        if gone {
            self.gone.store(true, Ordering::SeqCst);
            // An old device's stream can still report after it was
            // replaced; a new one's, before the open has finished, is
            // taken in by `opened`
            if self.generation.load(Ordering::SeqCst) == self.mine {
                self.lost.store(true, Ordering::SeqCst);
            }
        }
    }

    fn is_gone(&mut self, err: &CpalError, now: Instant) -> bool {
        match err.kind() {
            CpalErrorKind::DeviceNotAvailable | CpalErrorKind::StreamInvalidated => true,
            // A glitch; or the stream carries on, rerouted to the new default
            // (macOS) or without real-time priority
            CpalErrorKind::Xrun | CpalErrorKind::DeviceChanged | CpalErrorKind::RealtimeDenied => {
                false
            }
            // What cpal 0.17 reported as `BackendSpecific`, now split into
            // kinds (`BackendError`, `DeviceBusy`, `Other`, ...)
            _ => {
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

/// Which devices an open may try when the default device doesn't open
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Devices {
    /// Any other real one (as rodio's `open_default_sink` does)
    Any,
    /// The output went away, or was closed while nothing played. On Linux
    /// only the sound server's own devices and the one played on last:
    /// ALSA's others include raw hardware (`hw:`), which a stream opens
    /// exclusively, taking the sound from the rest of the desktop while
    /// PipeWire or PulseAudio restarts. Elsewhere like `Any`.
    Reopen,
    /// None: the default device changed, and only it will do
    DefaultOnly,
}

/// ALSA's devices that play through a sound server, sharing the card
const SOUND_SERVER_PCMS: [&str; 2] = ["pipewire", "pulse"];

/// Whether an open of `devices` may try `device` after the default, given
/// the device played on `last`
fn worth_trying(devices: Devices, device: &DeviceId, last: Option<&DeviceId>) -> bool {
    match devices {
        Devices::Any => true,
        Devices::Reopen if cfg!(target_os = "linux") => {
            SOUND_SERVER_PCMS.contains(&device.id()) || last == Some(device)
        }
        Devices::Reopen => true,
        Devices::DefaultOnly => false,
    }
}

/// Opens the engine's output devices
pub(crate) struct DeviceOutputs {
    /// Raised by each device's errors only while it is the current one
    generation: Arc<AtomicU64>,
    /// The device opened last
    last: Option<DeviceId>,
}

impl DeviceOutputs {
    pub(crate) fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            last: None,
        }
    }

    /// Open the default output device, or failing that another of
    /// `devices`, reporting its loss through `lost`. Returns the device's
    /// id too, as [`DefaultWatch`] tells devices apart. Each device opened
    /// starts a new generation: errors from earlier devices no longer
    /// count. If none opens, the current device still reports.
    pub(crate) fn open(
        &mut self,
        lost: &Arc<AtomicBool>,
        devices: Devices,
    ) -> Result<(MixerDeviceSink, Option<String>), String> {
        let mine = self.generation.load(Ordering::SeqCst) + 1;
        let errors = OutputErrors::new(lost.clone(), self.generation.clone(), mine);
        let open = |device: rodio::cpal::Device| {
            let id = device.id().ok();
            let errors = errors.for_stream();
            let mut reporter = errors.clone();
            DeviceSinkBuilder::from_device(device)
                .and_then(|builder| {
                    builder
                        .with_error_callback(move |err| reporter.report(err))
                        .open_sink_or_fallback()
                })
                .map(|sink| (sink, id, errors))
        };

        let host = rodio::cpal::default_host();
        let (mut sink, id, errors) = host
            .default_output_device()
            .ok_or(DeviceSinkError::NoDevice)
            .and_then(open)
            .or_else(|original| {
                if devices == Devices::DefaultOnly {
                    return Err(original);
                }
                let Ok(others) = host.output_devices() else {
                    return Err(original);
                };
                others
                    .filter(|device| {
                        device
                            .description()
                            .is_ok_and(|d| d.driver().is_some_and(|driver| driver != "null"))
                            && device
                                .id()
                                .is_ok_and(|id| worth_trying(devices, &id, self.last.as_ref()))
                    })
                    .find_map(|device| open(device).ok())
                    .ok_or(original)
            })
            .map_err(|e| e.to_string())?;
        errors.opened();
        sink.log_on_drop(false);
        let device = id.as_ref().map(ToString::to_string);
        self.last = id;
        Ok((sink, device))
    }
}

/// How often [`DefaultWatch::system`] asks which device is the default
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The system's default output device, as an id that tells devices apart
fn system_default_id() -> Option<String> {
    let device = rodio::cpal::default_host().default_output_device()?;
    device.id().ok().map(|id| id.to_string())
}

/// A new default that didn't open is asked for again after this many
/// checks, then twice as many each time it fails again, up to
/// `RETRY_CHECKS_MAX`
const RETRY_CHECKS_FIRST: u32 = 2;
const RETRY_CHECKS_MAX: u32 = 32;

/// Notices when the system's default output device changes.
///
/// On Windows an output stays on the device it was opened on: plug in
/// headphones and Windows makes them the default, but playback carries on
/// through the speakers until they fail. Elsewhere this isn't needed: on
/// Linux the engine plays through ALSA's "default", which PipeWire and
/// PulseAudio move themselves, and cpal opens the macOS default device as
/// one that follows the system's choice.
pub(crate) struct DefaultWatch {
    /// Reads which device is the default now
    current: Box<dyn Fn() -> Option<String> + Send>,
    /// The device the output is on
    followed: Option<String>,
    every: Duration,
    next_check: Instant,
    /// The default the last move was for
    seen: Option<String>,
    /// A default that didn't open, and when to try it again
    failed: Option<FailedDefault>,
}

/// A new default device that didn't open (a Bluetooth headset still
/// connecting)
struct FailedDefault {
    device: String,
    retry_at: Instant,
    /// How long it waited this time
    wait: Duration,
}

impl DefaultWatch {
    pub(crate) fn new(
        current: impl Fn() -> Option<String> + Send + 'static,
        every: Duration,
    ) -> Self {
        Self {
            current: Box::new(current),
            followed: None,
            every,
            next_check: Instant::now(),
            seen: None,
            failed: None,
        }
    }

    /// Watch the system's default output device
    pub(crate) fn system() -> Self {
        Self::new(system_default_id, DEFAULT_CHECK_INTERVAL)
    }

    /// An output was opened on `device`. It may not be the default: one
    /// that doesn't open gives way to another device, and the default is
    /// then asked for again.
    pub(crate) fn follow(&mut self, device: Option<String>) {
        self.followed = device;
        self.failed = None;
    }

    /// The default [`moved`](Self::moved) found didn't open: it is tried
    /// again later, waiting longer each time, unless another device
    /// becomes the default first
    pub(crate) fn not_opened(&mut self, now: Instant) {
        let Some(device) = self.seen.take() else {
            return;
        };
        let wait = match &self.failed {
            Some(failed) if failed.device == device => {
                (failed.wait * 2).min(self.every * RETRY_CHECKS_MAX)
            }
            _ => self.every * RETRY_CHECKS_FIRST,
        };
        self.failed = Some(FailedDefault {
            device,
            retry_at: now + wait,
            wait,
        });
    }

    /// Whether the default is a device other than the output's, and worth
    /// opening now. Asks at most once every `every`. With no default at all
    /// the output isn't moved: if its device is gone, the loss is noticed.
    pub(crate) fn moved(&mut self, now: Instant) -> bool {
        if now < self.next_check {
            return false;
        }
        self.next_check = now + self.every;
        let Some(default) = (self.current)() else {
            return false;
        };
        if self.followed.as_ref() == Some(&default) {
            return false;
        }
        let waiting = self
            .failed
            .as_ref()
            .is_some_and(|failed| failed.device == default && now < failed.retry_at);
        if waiting {
            return false;
        }
        self.seen = Some(default);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_errors() -> (OutputErrors, Arc<AtomicBool>, Arc<AtomicU64>) {
        let lost = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(1));
        (
            OutputErrors::new(lost.clone(), generation.clone(), 1),
            lost,
            generation,
        )
    }

    fn backend(msg: &str) -> CpalError {
        CpalError::with_message(CpalErrorKind::BackendError, msg.to_string())
    }

    fn error(kind: CpalErrorKind) -> CpalError {
        CpalError::new(kind)
    }

    #[test]
    fn an_unplugged_device_is_lost() {
        let (mut errors, lost, _) = new_errors();
        errors.report(error(CpalErrorKind::Xrun));
        errors.report(backend("glitch"));
        assert!(
            !lost.load(Ordering::SeqCst),
            "one-off errors are not a loss"
        );
        errors.report(error(CpalErrorKind::DeviceNotAvailable));
        assert!(lost.load(Ordering::SeqCst));
    }

    #[test]
    fn errors_the_stream_plays_on_after_are_never_a_loss() {
        let (mut errors, _, _) = new_errors();
        let start = Instant::now();
        for kind in [
            CpalErrorKind::Xrun,
            CpalErrorKind::DeviceChanged,
            CpalErrorKind::RealtimeDenied,
        ] {
            let gone = (0..BACKEND_ERROR_BURST * 2).any(|_| errors.is_gone(&error(kind), start));
            assert!(!gone, "{kind:?} counted as a loss");
        }
        // Kinds cpal 0.17 reported as backend-specific still count towards
        // a burst
        let gone: Vec<bool> = (0..BACKEND_ERROR_BURST)
            .map(|_| errors.is_gone(&error(CpalErrorKind::DeviceBusy), start))
            .collect();
        assert!(gone[gone.len() - 1]);
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
    fn a_loss_in_the_first_moments_of_an_open_is_kept() {
        // The old device is generation 1, the stream being opened 2
        let lost = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicU64::new(1));
        let errors = OutputErrors::new(lost.clone(), generation.clone(), 2).for_stream();
        let mut reporter = errors.clone();
        // The new stream reports its device gone before the open returns
        reporter.report(error(CpalErrorKind::DeviceNotAvailable));
        assert_eq!(generation.load(Ordering::SeqCst), 1);
        errors.opened();
        assert_eq!(generation.load(Ordering::SeqCst), 2);
        assert!(lost.load(Ordering::SeqCst), "the early loss was wiped");

        // A stream that opened fine starts with no loss, and an old
        // stream's errors no longer count
        let errors = OutputErrors::new(lost.clone(), generation.clone(), 3).for_stream();
        let mut old = OutputErrors::new(lost.clone(), generation.clone(), 2).for_stream();
        errors.opened();
        assert!(!lost.load(Ordering::SeqCst));
        old.report(error(CpalErrorKind::DeviceNotAvailable));
        assert!(!lost.load(Ordering::SeqCst));
    }

    #[test]
    fn the_silent_output_keeps_to_real_time() {
        /// Endless stereo silence, counting the samples taken
        struct Counted(Arc<AtomicU64>, NonZero<u32>);
        impl Iterator for Counted {
            type Item = f32;
            fn next(&mut self) -> Option<f32> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Some(0.0)
            }
        }
        impl rodio::Source for Counted {
            fn current_span_len(&self) -> Option<usize> {
                None
            }
            fn channels(&self) -> NonZero<u16> {
                NonZero::new(2).unwrap()
            }
            fn sample_rate(&self) -> NonZero<u32> {
                self.1
            }
            fn total_duration(&self) -> Option<Duration> {
                None
            }
        }

        let output = SilentOutput::open(1.0, Arc::default()).unwrap();
        // One pull's worth at a time, 100 times a second
        assert_eq!(output.frames, 441);
        // Count what it takes over half a second. How close to real time
        // it stays depends on how busy the machine is: `the_silent_outputs
        // _pace_*` check the schedule exactly. Here, only that it isn't
        // ahead of real time by more than the pull under way, nor far behind.
        let pulled = Arc::new(AtomicU64::new(0));
        let rate = output.sample_rate;
        output.mixer.add(Counted(pulled.clone(), rate));
        let start = Instant::now();
        thread::sleep(Duration::from_millis(500));
        let frames = pulled.load(Ordering::Relaxed) as f64 / 2.0;
        let secs = frames / f64::from(rate.get());
        let elapsed = start.elapsed().as_secs_f64();
        assert!(
            secs <= elapsed + 2.0 * SILENT_PULL_INTERVAL.as_secs_f64() && secs > elapsed / 2.0,
            "took {secs:.3} s of audio in {elapsed:.3} s"
        );
    }

    #[test]
    fn the_silent_outputs_pace_doesnt_drift_with_the_time_spent_pulling() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut pace = Pace::new(start);
        // Each pull takes 3 ms: the waits make up the rest of the 10 ms
        let mut now = start;
        for pull in 1..=100u64 {
            now += Duration::from_millis(3);
            let wait = pace.pulled(now).expect("on time");
            assert_eq!(wait, Duration::from_millis(7));
            now += wait;
            assert_eq!(now, at(pull * 10));
        }
        // A pull that ran 5 ms over is made up by the next waits, not lost
        assert_eq!(pace.pulled(at(1015)), None);
        assert_eq!(pace.pulled(at(1016)), Some(Duration::from_millis(4)));
    }

    #[test]
    fn the_silent_outputs_pace_starts_again_after_a_long_pause() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut pace = Pace::new(start);
        // 100 ms behind is caught up: no waits until back on schedule
        assert_eq!(pace.pulled(at(105)), None);
        assert_eq!(pace.pulled(at(106)), None);
        // More than that (asleep): carries on from now, a pull every 10 ms
        let mut pace = Pace::new(start);
        assert_eq!(pace.pulled(at(500)), None);
        assert_eq!(pace.pulled(at(501)), Some(Duration::from_millis(9)));
    }

    /// A watch on a default the test sets
    fn watch() -> (DefaultWatch, Arc<std::sync::Mutex<Option<String>>>) {
        let default = Arc::new(std::sync::Mutex::new(Some("speakers".to_string())));
        let current = default.clone();
        let watch = DefaultWatch::new(
            move || current.lock().unwrap().clone(),
            Duration::from_secs(1),
        );
        (watch, default)
    }

    fn follow(watch: &mut DefaultWatch, device: &str) {
        watch.follow(Some(device.to_string()));
    }

    #[test]
    fn a_new_default_device_is_noticed_once() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        follow(&mut watch, "speakers");
        assert!(!watch.moved(start));

        *default.lock().unwrap() = Some("headphones".to_string());
        assert!(watch.moved(start + Duration::from_secs(1)));
        // Once opened, the new default is the one followed
        follow(&mut watch, "headphones");
        assert!(!watch.moved(start + Duration::from_secs(2)));
    }

    #[test]
    fn the_default_is_asked_at_most_once_a_second() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        follow(&mut watch, "speakers");
        assert!(!watch.moved(start));
        *default.lock().unwrap() = Some("headphones".to_string());
        assert!(!watch.moved(start + Duration::from_millis(500)));
        assert!(watch.moved(start + Duration::from_millis(1000)));
    }

    #[test]
    fn no_default_device_is_not_a_move() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        follow(&mut watch, "speakers");
        *default.lock().unwrap() = None;
        assert!(!watch.moved(start));

        // A device that appears after the output was opened without one is
        watch.follow(None);
        *default.lock().unwrap() = Some("headphones".to_string());
        assert!(watch.moved(start + Duration::from_secs(1)));
    }

    #[test]
    fn an_output_opened_on_another_device_moves_to_the_default() {
        // The default didn't open at start, and another device did
        let (mut watch, _) = watch();
        follow(&mut watch, "hdmi");
        assert!(watch.moved(Instant::now()));
    }

    #[test]
    fn a_default_that_did_not_open_is_tried_again_later() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        follow(&mut watch, "speakers");
        *default.lock().unwrap() = Some("headphones".to_string());

        // Waiting 2, then 4 seconds after each failure, not trying it at
        // every check
        assert!(watch.moved(at(0)));
        watch.not_opened(at(0));
        assert!(!watch.moved(at(1)));
        assert!(watch.moved(at(2)));
        watch.not_opened(at(2));
        assert!(!watch.moved(at(3)));
        assert!(!watch.moved(at(5)));
        assert!(watch.moved(at(6)));

        // Until it opens
        follow(&mut watch, "headphones");
        assert!(!watch.moved(at(7)));
    }

    #[test]
    fn the_wait_for_a_default_that_does_not_open_is_capped() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        follow(&mut watch, "speakers");
        *default.lock().unwrap() = Some("broken".to_string());
        let mut now = start;
        let mut tries = Vec::new();
        while now < start + Duration::from_secs(200) {
            if watch.moved(now) {
                tries.push(now.duration_since(start).as_secs());
                watch.not_opened(now);
            }
            now += Duration::from_secs(1);
        }
        assert_eq!(tries, [0, 2, 6, 14, 30, 62, 94, 126, 158, 190]);
    }

    #[test]
    fn another_new_default_is_tried_at_once() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        follow(&mut watch, "speakers");
        *default.lock().unwrap() = Some("headphones".to_string());
        assert!(watch.moved(start));
        watch.not_opened(start);

        *default.lock().unwrap() = Some("usb headset".to_string());
        assert!(watch.moved(start + Duration::from_secs(1)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reopens_only_shared_devices_or_the_one_it_was_on() {
        let alsa = |pcm: &str| DeviceId::new(rodio::cpal::HostId::Alsa, pcm);
        let hw = alsa("hw:CARD=0,DEV=0");
        let usb = alsa("front:CARD=USB,DEV=0");
        for pcm in SOUND_SERVER_PCMS {
            assert!(worth_trying(Devices::Reopen, &alsa(pcm), None));
        }
        // Raw hardware would take the card from the rest of the desktop
        assert!(!worth_trying(Devices::Reopen, &hw, None));
        assert!(!worth_trying(Devices::Reopen, &hw, Some(&usb)));
        assert!(worth_trying(Devices::Reopen, &usb, Some(&usb)));
        // At start, any device will do
        assert!(worth_trying(Devices::Any, &hw, None));
        assert!(!worth_trying(Devices::DefaultOnly, &alsa("pipewire"), None));
    }

    #[test]
    fn a_replaced_device_can_no_longer_raise_the_flag() {
        let (mut errors, lost, generation) = new_errors();
        generation.store(2, Ordering::SeqCst);
        errors.report(error(CpalErrorKind::DeviceNotAvailable));
        assert!(!lost.load(Ordering::SeqCst));
    }
}
