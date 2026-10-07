//! Listening on a phone: the playing station as one endless MP3 stream
//!
//! The same taps that feed recordings (see [`super::RecordingTap`]) copy
//! the decoded audio here while at least one listener is connected. One
//! encoder thread turns it into MP3 at a fixed rate and bitrate and hands
//! each piece to every listener, so the format never changes, whatever the
//! station plays or how often it switches. When no audio comes (stopped,
//! buffering, switching station) the encoder fills in silence at the pace
//! of the clock, so a phone's player never runs dry.
//!
//! Like recording, listening never holds up playback: audio goes to the
//! encoder with `try_send`, and a listener that falls too far behind is
//! dropped (its stream ends, and the phone can connect again).

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};

use super::mp3::Mp3Encoder;
use super::{TapPoint, QUEUE_BATCHES};

/// Sample rate of the stream
pub const LISTEN_SAMPLE_RATE: u32 = 44_100;

/// Channels of the stream
pub const LISTEN_CHANNELS: u16 = 2;

/// Bitrate of the stream, in kbps (constant)
pub const LISTEN_BITRATE_KBPS: u32 = 192;

/// Most listeners at once
pub const MAX_LISTENERS: usize = 4;

/// How long without audio before silence fills in
const SILENCE_AFTER: Duration = Duration::from_millis(500);

/// How often the encoder looks at the clock while no audio comes
const IDLE_TICK: Duration = Duration::from_millis(100);

/// Pieces a listener may fall behind by before it is dropped (each piece is
/// one batch of audio, about 46 ms, so about 12 s)
const LISTENER_QUEUE: usize = 256;

/// Why a listener couldn't join
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenError {
    /// [`MAX_LISTENERS`] are listening already
    TooMany,
    /// The encoder couldn't start
    Failed(String),
}

impl std::fmt::Display for ListenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenError::TooMany => write!(f, "{MAX_LISTENERS} phones are listening already"),
            ListenError::Failed(e) => f.write_str(e),
        }
    }
}

enum EncoderMsg {
    Pcm {
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    },
    Finish,
}

struct Listener {
    id: u64,
    name: String,
    tx: Sender<Arc<[u8]>>,
    /// Has had its first whole MP3 frame: a listener that joins mid-stream
    /// starts at a frame header
    synced: bool,
}

struct Encoder {
    generation: u64,
    tx: Sender<EncoderMsg>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct ListenShared {
    listeners: Mutex<Vec<Listener>>,
    encoder: Mutex<Option<Encoder>>,
    /// Generation of the running encoder, 0 when nobody listens
    generation: AtomicU64,
    next_generation: AtomicU64,
    next_listener: AtomicU64,
    /// `TapPoint::id()` listeners hear; 0 is the default (before the EQ)
    tap: AtomicU8,
    /// Pieces the taps couldn't hand to a busy encoder
    dropped_batches: AtomicU64,
}

/// Lets phones listen to the playing station. Cheap to clone; all clones
/// share state.
#[derive(Clone, Default)]
pub struct Listen {
    shared: Arc<ListenShared>,
}

impl std::fmt::Debug for Listen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listen")
            .field("listeners", &self.listener_names())
            .finish()
    }
}

impl Listen {
    pub fn new() -> Self {
        Self::default()
    }

    /// Join as a listener called `name` (shown to the user).
    ///
    /// The stream starts with whole MP3 frames and goes on until the
    /// returned [`ListenStream`] is dropped, or the listener falls behind.
    pub fn subscribe(&self, name: &str) -> Result<ListenStream, ListenError> {
        let mut listeners = self.lock_listeners();
        if listeners.len() >= MAX_LISTENERS {
            return Err(ListenError::TooMany);
        }
        self.ensure_encoder()?;
        let id = self.shared.next_listener.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = bounded(LISTENER_QUEUE);
        listeners.push(Listener {
            id,
            name: name.to_string(),
            tx,
            synced: false,
        });
        Ok(ListenStream {
            id,
            rx,
            listen: self.clone(),
        })
    }

    /// Names of the listeners, in the order they joined
    pub fn listener_names(&self) -> Vec<String> {
        self.lock_listeners()
            .iter()
            .map(|l| l.name.clone())
            .collect()
    }

    /// How many listen now
    pub fn listeners(&self) -> usize {
        self.lock_listeners().len()
    }

    /// Where in the playback chain listeners take the audio from (before
    /// the equalizer unless set)
    pub fn set_tap(&self, tap: TapPoint) {
        self.shared.tap.store(tap.id(), Ordering::SeqCst);
    }

    /// Pieces of audio lost because the encoder was busy
    pub fn dropped_batches(&self) -> u64 {
        self.shared.dropped_batches.load(Ordering::Relaxed)
    }

    fn tap(&self) -> TapPoint {
        match self.shared.tap.load(Ordering::SeqCst) {
            2 => TapPoint::AfterEq,
            _ => TapPoint::BeforeEq,
        }
    }

    /// Generation of the running encoder if it listens at `tap`, else 0
    pub(super) fn generation_for(&self, tap: TapPoint) -> u64 {
        if self.tap() == tap {
            self.shared.generation.load(Ordering::SeqCst)
        } else {
            0
        }
    }

    /// Hand audio to the encoder if `generation` is still running
    pub(super) fn submit(
        &self,
        generation: u64,
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    ) {
        let encoder = self.lock_encoder();
        let Some(encoder) = encoder.as_ref().filter(|e| e.generation == generation) else {
            return;
        };
        let msg = EncoderMsg::Pcm {
            sample_rate,
            channels,
            samples,
        };
        if encoder.tx.try_send(msg).is_err() {
            self.shared.dropped_batches.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Start the encoder thread unless it runs. Called with the listeners
    /// locked, so it can't race the last listener leaving.
    fn ensure_encoder(&self) -> Result<(), ListenError> {
        let mut encoder = self.lock_encoder();
        if encoder.is_some() {
            return Ok(());
        }
        let mp3 = Mp3Encoder::stream(LISTEN_BITRATE_KBPS, LISTEN_SAMPLE_RATE, LISTEN_CHANNELS)
            .map_err(ListenError::Failed)?;
        let (tx, rx) = bounded(QUEUE_BATCHES);
        let shared = Arc::downgrade(&self.shared);
        let handle = thread::Builder::new()
            .name("listen-encoder".to_string())
            .spawn(move || {
                encoder_loop(mp3, rx, move |piece| {
                    // The listeners went with the last handle: nothing to feed
                    if let Some(shared) = shared.upgrade() {
                        broadcast(&shared, piece);
                    }
                })
            })
            .map_err(|e| ListenError::Failed(format!("Failed to start listening: {e}")))?;
        let generation = self.shared.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        *encoder = Some(Encoder {
            generation,
            tx,
            handle: Some(handle),
        });
        self.shared.generation.store(generation, Ordering::SeqCst);
        Ok(())
    }

    fn leave(&self, id: u64) {
        let mut listeners = self.lock_listeners();
        listeners.retain(|l| l.id != id);
        if listeners.is_empty() {
            self.stop_encoder();
        }
    }

    /// Stop the encoder; the listeners must be locked (or gone)
    fn stop_encoder(&self) {
        self.shared.generation.store(0, Ordering::SeqCst);
        let encoder = self.lock_encoder().take();
        if let Some(mut encoder) = encoder {
            let _ = encoder.tx.try_send(EncoderMsg::Finish);
            // Not joined: it ends on its own once it reads Finish or sees
            // the sender gone, and the caller may be an async task
            drop(encoder.tx);
            drop(encoder.handle.take());
        }
    }

    fn lock_listeners(&self) -> MutexGuard<'_, Vec<Listener>> {
        self.shared
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn lock_encoder(&self) -> MutexGuard<'_, Option<Encoder>> {
        self.shared
            .encoder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

/// One listener's stream of MP3 bytes. Dropping it leaves.
pub struct ListenStream {
    id: u64,
    rx: Receiver<Arc<[u8]>>,
    listen: Listen,
}

impl ListenStream {
    /// The next piece of MP3, waiting at most `timeout`.
    ///
    /// `Err(Disconnected)` means the stream has ended: the listener fell
    /// behind, or listening stopped.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Arc<[u8]>, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}

impl Drop for ListenStream {
    fn drop(&mut self) {
        self.listen.leave(self.id);
    }
}

/// Give `piece` to every listener; drop the ones that fell behind or left
fn broadcast(shared: &ListenShared, piece: &[u8]) {
    let mut listeners = shared.listeners.lock().unwrap_or_else(|e| e.into_inner());
    let mut shared_piece: Option<Arc<[u8]>> = None;
    listeners.retain_mut(|listener| {
        let data: Arc<[u8]> = if listener.synced {
            shared_piece.get_or_insert_with(|| piece.into()).clone()
        } else {
            let Some(start) = frame_start(piece) else {
                return true;
            };
            listener.synced = true;
            piece[start..].into()
        };
        match listener.tx.try_send(data) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    });
}

/// Where the first MPEG-1 Layer III frame header starts in `data`
fn frame_start(data: &[u8]) -> Option<usize> {
    data.windows(2)
        .position(|w| w[0] == 0xff && (w[1] & 0xfe) == 0xfa)
}

/// Encode what comes in, and silence while nothing does, handing each
/// piece of MP3 to `deliver`
fn encoder_loop(mut mp3: Mp3Encoder, rx: Receiver<EncoderMsg>, deliver: impl Fn(&[u8])) {
    use super::AudioEncoder as _;

    let mut out = Vec::new();
    let mut last_audio = Instant::now();
    // How far silence has been filled in since the last audio
    let mut silence_until: Option<Instant> = None;
    loop {
        match rx.recv_timeout(IDLE_TICK) {
            Ok(EncoderMsg::Pcm {
                sample_rate,
                channels,
                samples,
            }) => {
                out.clear();
                let clamped: Vec<f32> = samples.iter().map(|s| s.clamp(-1.0, 1.0)).collect();
                if let Err(e) = mp3.encode(sample_rate, channels, &clamped, &mut out) {
                    eprintln!("Listening: {e}");
                    return;
                }
                last_audio = Instant::now();
                silence_until = None;
                if !out.is_empty() {
                    deliver(&out);
                }
            }
            Ok(EncoderMsg::Finish) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        if now.duration_since(last_audio) < SILENCE_AFTER {
            continue;
        }
        let from = silence_until.unwrap_or(last_audio);
        let frames =
            (now.duration_since(from).as_secs_f64() * f64::from(LISTEN_SAMPLE_RATE)) as u64;
        if frames == 0 {
            continue;
        }
        let silence = vec![0.0f32; frames as usize * usize::from(LISTEN_CHANNELS)];
        out.clear();
        if let Err(e) = mp3.encode(LISTEN_SAMPLE_RATE, LISTEN_CHANNELS, &silence, &mut out) {
            eprintln!("Listening: {e}");
            return;
        }
        silence_until =
            Some(from + Duration::from_secs_f64(frames as f64 / f64::from(LISTEN_SAMPLE_RATE)));
        if !out.is_empty() {
            deliver(&out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read pieces until `bytes` have come or `timeout` passes
    fn read(stream: &ListenStream, bytes: usize, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut got = Vec::new();
        while got.len() < bytes && Instant::now() < deadline {
            if let Ok(piece) = stream.recv_timeout(Duration::from_millis(50)) {
                got.extend_from_slice(&piece);
            }
        }
        got
    }

    fn sine(rate: u32, channels: u16, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5;
                std::iter::repeat_n(v, channels as usize)
            })
            .collect()
    }

    /// Count MP3 frames by walking their headers from the start
    fn count_frames(data: &[u8]) -> usize {
        // MPEG-1 Layer III bitrates (kbps) and rates (Hz)
        const KBPS: [u32; 16] = [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
        ];
        const RATES: [u32; 4] = [44_100, 48_000, 32_000, 0];
        let mut at = 0;
        let mut frames = 0;
        while at + 4 <= data.len() {
            let h = &data[at..at + 4];
            assert_eq!(h[0], 0xff, "frame {frames} at {at} is not a header");
            assert_eq!(h[1] & 0xfe, 0xfa, "frame {frames} is not MPEG-1 Layer III");
            let kbps = KBPS[usize::from(h[2] >> 4)];
            let rate = RATES[usize::from((h[2] >> 2) & 3)];
            assert_eq!(rate, LISTEN_SAMPLE_RATE);
            assert_eq!(kbps, LISTEN_BITRATE_KBPS);
            let padding = u32::from((h[2] >> 1) & 1);
            at += (144 * kbps * 1000 / rate + padding) as usize;
            frames += 1;
        }
        frames
    }

    #[test]
    fn silence_fills_in_while_nothing_plays() {
        let listen = Listen::new();
        let stream = listen.subscribe("Pixel").unwrap();
        // About a second of silence at 192 kbps is 24 kB
        let got = read(&stream, 12_000, Duration::from_secs(5));
        assert!(got.len() >= 12_000, "only {} bytes of silence", got.len());
        assert!(count_frames(&got) > 0);
    }

    #[test]
    fn audio_at_any_rate_comes_out_as_one_format() {
        let listen = Listen::new();
        let stream = listen.subscribe("Pixel").unwrap();
        let generation = listen.generation_for(TapPoint::BeforeEq);
        assert_ne!(generation, 0);
        assert_eq!(listen.generation_for(TapPoint::AfterEq), 0);
        // A 48 kHz mono station, then a 44.1 kHz stereo one
        for _ in 0..20 {
            listen.submit(generation, 48_000, 1, sine(48_000, 1, 4800));
        }
        for _ in 0..20 {
            listen.submit(generation, 44_100, 2, sine(44_100, 2, 4410));
        }
        let got = read(&stream, 40_000, Duration::from_secs(5));
        assert!(got.len() >= 40_000, "only {} bytes", got.len());
        // Every frame is 44.1 kHz at 192 kbps (checked inside)
        assert!(count_frames(&got) > 10);
    }

    #[test]
    fn the_last_listener_leaving_stops_the_encoder() {
        let listen = Listen::new();
        let a = listen.subscribe("Pixel").unwrap();
        let b = listen.subscribe("Xiaomi").unwrap();
        assert_eq!(listen.listener_names(), ["Pixel", "Xiaomi"]);
        drop(a);
        assert_eq!(listen.listeners(), 1);
        assert_ne!(listen.generation_for(TapPoint::BeforeEq), 0);
        drop(b);
        assert_eq!(listen.listeners(), 0);
        assert_eq!(listen.generation_for(TapPoint::BeforeEq), 0);
        // And starts again for the next one
        let c = listen.subscribe("Pixel").unwrap();
        assert!(!read(&c, 1000, Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn listeners_are_limited() {
        let listen = Listen::new();
        let streams: Vec<_> = (0..MAX_LISTENERS)
            .map(|i| listen.subscribe(&format!("Phone {i}")).unwrap())
            .collect();
        assert_eq!(
            listen.subscribe("One more").err(),
            Some(ListenError::TooMany)
        );
        drop(streams);
        assert!(listen.subscribe("One more").is_ok());
    }

    #[test]
    fn a_listener_that_falls_behind_is_dropped() {
        let listen = Listen::new();
        let slow = listen.subscribe("Slow").unwrap();
        let generation = listen.generation_for(TapPoint::BeforeEq);
        // Far more audio than the listener's queue holds, never read
        for _ in 0..(LISTENER_QUEUE * 3) {
            listen.submit(generation, 44_100, 2, sine(44_100, 2, 2048));
            thread::sleep(Duration::from_millis(1));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while listen.listeners() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(listen.listeners(), 0);
        // Its stream ends once read out
        let ended = loop {
            match slow.recv_timeout(Duration::from_secs(1)) {
                Ok(_) => continue,
                Err(e) => break e,
            }
        };
        assert_eq!(ended, RecvTimeoutError::Disconnected);
    }

    #[test]
    fn a_late_listener_starts_on_a_frame() {
        let listen = Listen::new();
        let first = listen.subscribe("First").unwrap();
        let _ = read(&first, 5_000, Duration::from_secs(5));
        let late = listen.subscribe("Late").unwrap();
        let got = read(&late, 5_000, Duration::from_secs(5));
        assert_eq!(&got[..1], &[0xff]);
        assert_eq!(got[1] & 0xfe, 0xfa);
    }
}
