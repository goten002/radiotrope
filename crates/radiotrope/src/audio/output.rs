//! Audio output device handling
//!
//! Opens the output device with an error callback that notices when the
//! device goes away (unplugged USB or Bluetooth headphones, a disabled
//! device). The engine then opens another and gives it a new output of the
//! playing station's queue ([`super::pcm::PcmFeed::output`]), so the
//! station carries on without restarting.
//!
//! On Windows the engine also moves playback when the default device
//! changes ([`DefaultWatch`]).
//!
//! Without a device, a [`SilentOutput`] takes the audio at a steady pace
//! and drops it, so the engine plays the same way (tests, headless use).

use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rodio::cpal::traits::HostTrait;
use rodio::cpal::{BufferSize, StreamError};
use rodio::mixer::{self, Mixer};
use rodio::{DeviceSinkBuilder, DeviceTrait, MixerDeviceSink};

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

    /// How much the output takes at a time, and its sample rate
    pub(crate) fn buffer(&self) -> (BufferSize, NonZero<u32>) {
        match self {
            Output::Device(sink) => (*sink.config().buffer_size(), sink.config().sample_rate()),
            Output::Silent(silent) => (BufferSize::Fixed(silent.frames), silent.sample_rate),
        }
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
    pub(crate) fn open(speed: f32) -> Result<Self, String> {
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
                while !stopped.load(Ordering::Relaxed) {
                    // An empty mixer returns None until a player joins it
                    for _ in 0..samples {
                        source.next();
                    }
                    thread::sleep(SILENT_PULL_INTERVAL);
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

impl Drop for SilentOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Errors that aren't a loss on their own, within one second, that mean the
/// device is gone: ALSA reports an unplugged device as a stream of
/// backend errors rather than `DeviceNotAvailable`
const BACKEND_ERROR_BURST: u32 = 20;

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
/// Each device opened starts a new generation: errors from earlier devices
/// no longer count. If none opens, the current device still reports.
pub(crate) fn open_output(
    lost: &Arc<AtomicBool>,
    generation: &Arc<AtomicU64>,
) -> Result<MixerDeviceSink, String> {
    let mine = generation.load(Ordering::SeqCst) + 1;
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
    generation.store(mine, Ordering::SeqCst);
    lost.store(false, Ordering::SeqCst);
    sink.log_on_drop(false);
    Ok(sink)
}

/// How often [`DefaultWatch::system`] asks which device is the default
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The system's default output device, as an id that tells devices apart
fn system_default_id() -> Option<String> {
    let device = rodio::cpal::default_host().default_output_device()?;
    device.id().ok().map(|id| id.to_string())
}

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
    /// The default when the output was last opened
    followed: Option<String>,
    every: Duration,
    next_check: Instant,
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
        }
    }

    /// Watch the system's default output device
    pub(crate) fn system() -> Self {
        Self::new(system_default_id, DEFAULT_CHECK_INTERVAL)
    }

    /// The default device is about to be opened: remember which one it is
    pub(crate) fn follow(&mut self) {
        self.followed = (self.current)();
    }

    /// Whether another device became the default since the output was
    /// opened. Asks at most once every `every`. With no default at all the
    /// output isn't moved: if its device is gone, the loss is noticed.
    pub(crate) fn moved(&mut self, now: Instant) -> bool {
        if now < self.next_check {
            return false;
        }
        self.next_check = now + self.every;
        (self.current)().is_some_and(|id| self.followed.as_ref() != Some(&id))
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

    #[test]
    fn a_new_default_device_is_noticed_once() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        watch.follow();
        assert!(!watch.moved(start));

        *default.lock().unwrap() = Some("headphones".to_string());
        assert!(watch.moved(start + Duration::from_secs(1)));
        // Once opened, the new default is the one followed
        watch.follow();
        assert!(!watch.moved(start + Duration::from_secs(2)));
    }

    #[test]
    fn the_default_is_asked_at_most_once_a_second() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        watch.follow();
        assert!(!watch.moved(start));
        *default.lock().unwrap() = Some("headphones".to_string());
        assert!(!watch.moved(start + Duration::from_millis(500)));
        assert!(watch.moved(start + Duration::from_millis(1000)));
    }

    #[test]
    fn no_default_device_is_not_a_move() {
        let (mut watch, default) = watch();
        let start = Instant::now();
        watch.follow();
        *default.lock().unwrap() = None;
        assert!(!watch.moved(start));

        // A device that appears after the output was opened without one is
        watch.follow();
        *default.lock().unwrap() = Some("headphones".to_string());
        assert!(watch.moved(start + Duration::from_secs(1)));
    }

    #[test]
    fn a_replaced_device_can_no_longer_raise_the_flag() {
        let (mut errors, lost, generation) = new_errors();
        generation.store(2, Ordering::SeqCst);
        errors.report(StreamError::DeviceNotAvailable);
        assert!(!lost.load(Ordering::SeqCst));
    }
}
