//! Audio output device handling
//!
//! Opens the output device with an error callback that notices when the
//! device goes away (unplugged USB or Bluetooth headphones, a disabled
//! device). The engine then opens another and gives it a new output of the
//! playing station's queue ([`super::pcm::PcmFeed::output`]), so the
//! station carries on without restarting.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rodio::cpal::traits::HostTrait;
use rodio::cpal::StreamError;
use rodio::{DeviceSinkBuilder, DeviceTrait, MixerDeviceSink};

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
