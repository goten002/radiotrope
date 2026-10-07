//! Listening on a phone: the playing station as one endless stream
//!
//! The same taps that feed recordings (see [`super::RecordingTap`]) copy
//! the decoded audio here while at least one listener is connected. One
//! encoder thread turns it into each format a listener wants ([`ListenFormat`]:
//! an MP3 byte stream, or Opus packets for WebRTC) at a fixed rate and
//! bitrate, and hands each piece to the listeners of that format, so the
//! format never changes, whatever the station plays or how often it
//! switches. When no audio comes (stopped,
//! buffering, switching station) the encoder fills in silence at the pace
//! of the clock, so a phone's player never runs dry.
//!
//! Like recording, listening never holds up playback: audio goes to the
//! encoder with `try_send`, and a listener that falls too far behind is
//! dropped (its stream ends, and the phone can connect again).

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};

use super::mp3::Mp3Encoder;
use super::opus::OpusPackets;
use super::{TapPoint, QUEUE_BATCHES};

/// Sample rate of the stream
pub const LISTEN_SAMPLE_RATE: u32 = 44_100;

/// Channels of the stream
pub const LISTEN_CHANNELS: u16 = 2;

/// Bitrate of the stream, in kbps (constant)
pub const LISTEN_BITRATE_KBPS: u32 = 192;

/// Bitrate of the Opus packets, in kbps (48 kHz stereo)
pub const LISTEN_OPUS_KBPS: u32 = 128;

/// Sample rate of the Opus packets (Opus always runs at 48 kHz)
pub const LISTEN_OPUS_RATE: u32 = OpusPackets::RATE;

/// Samples per channel in each Opus packet (20 ms)
pub const LISTEN_OPUS_FRAME: usize = OpusPackets::FRAME;

/// Packet loss the Opus encoder prepares for (Wi-Fi drops a few)
const OPUS_LOSS_PERCENT: i32 = 5;

/// Most listeners at once
pub const MAX_LISTENERS: usize = 4;

/// How long without audio before silence fills in
const SILENCE_AFTER: Duration = Duration::from_millis(500);

/// How often the encoder looks at the clock while no audio comes
const IDLE_TICK: Duration = Duration::from_millis(100);

/// Pieces a listener may fall behind by before it is dropped (each piece is
/// one batch of audio, about 46 ms, so about 12 s)
const LISTENER_QUEUE: usize = 256;

/// What a listener receives
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenFormat {
    /// An MP3 byte stream (pieces of any size; the first starts on a frame)
    Mp3,
    /// One Opus packet (20 ms, 48 kHz stereo) per piece
    Opus,
}

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
    format: ListenFormat,
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

    /// Join as an MP3 listener called `name` (shown to the user); see
    /// [`Listen::subscribe_as`]
    pub fn subscribe(&self, name: &str) -> Result<ListenStream, ListenError> {
        self.subscribe_as(name, ListenFormat::Mp3)
    }

    /// Join as a listener called `name` (shown to the user).
    ///
    /// The stream starts with whole MP3 frames (or whole Opus packets) and
    /// goes on until the returned [`ListenStream`] is dropped, or the
    /// listener falls behind.
    pub fn subscribe_as(
        &self,
        name: &str,
        format: ListenFormat,
    ) -> Result<ListenStream, ListenError> {
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
            format,
            tx,
            // Each Opus piece is a whole packet
            synced: format == ListenFormat::Opus,
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
        let (tx, rx) = bounded(QUEUE_BATCHES);
        let shared = Arc::downgrade(&self.shared);
        let handle = thread::Builder::new()
            .name("listen-encoder".to_string())
            .spawn(move || encoder_loop(shared, rx))
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

    /// The next piece if one is waiting
    pub fn try_recv(&self) -> Result<Arc<[u8]>, crossbeam_channel::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for ListenStream {
    fn drop(&mut self) {
        self.listen.leave(self.id);
    }
}

/// Give `piece` to every listener of `format`; drop the ones that fell
/// behind or left
fn broadcast(shared: &ListenShared, format: ListenFormat, piece: &[u8]) {
    let mut listeners = shared.listeners.lock().unwrap_or_else(|e| e.into_inner());
    let mut shared_piece: Option<Arc<[u8]>> = None;
    listeners.retain_mut(|listener| {
        if listener.format != format {
            return true;
        }
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

/// The encoders the listeners need now; each starts with its first
/// listener and goes with its last
#[derive(Default)]
struct Encoders {
    mp3: Option<Mp3Encoder>,
    opus: Option<OpusPackets>,
    out: Vec<u8>,
}

impl Encoders {
    /// Encode `samples` into each format someone listens in. False once
    /// nobody can listen any more (the [`Listen`] is gone).
    fn encode(
        &mut self,
        shared: &Weak<ListenShared>,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
    ) -> bool {
        use super::AudioEncoder as _;

        let Some(shared) = shared.upgrade() else {
            return false;
        };
        let (mp3, opus) = {
            let listeners = shared.listeners.lock().unwrap_or_else(|e| e.into_inner());
            (
                listeners.iter().any(|l| l.format == ListenFormat::Mp3),
                listeners.iter().any(|l| l.format == ListenFormat::Opus),
            )
        };

        if !mp3 {
            self.mp3 = None;
        } else {
            if self.mp3.is_none() {
                match Mp3Encoder::stream(LISTEN_BITRATE_KBPS, LISTEN_SAMPLE_RATE, LISTEN_CHANNELS) {
                    Ok(encoder) => self.mp3 = Some(encoder),
                    Err(e) => fail(&shared, ListenFormat::Mp3, &e),
                }
            }
            if let Some(encoder) = self.mp3.as_mut() {
                self.out.clear();
                match encoder.encode(sample_rate, channels, samples, &mut self.out) {
                    Ok(()) if !self.out.is_empty() => {
                        broadcast(&shared, ListenFormat::Mp3, &self.out)
                    }
                    Ok(()) => {}
                    Err(e) => {
                        self.mp3 = None;
                        fail(&shared, ListenFormat::Mp3, &e);
                    }
                }
            }
        }

        if !opus {
            self.opus = None;
        } else {
            if self.opus.is_none() {
                match OpusPackets::new(LISTEN_CHANNELS, LISTEN_OPUS_KBPS, OPUS_LOSS_PERCENT) {
                    Ok(encoder) => self.opus = Some(encoder),
                    Err(e) => fail(&shared, ListenFormat::Opus, &e),
                }
            }
            if let Some(encoder) = self.opus.as_mut() {
                let sent = encoder.encode(sample_rate, channels, samples, |packet| {
                    broadcast(&shared, ListenFormat::Opus, packet)
                });
                if let Err(e) = sent {
                    self.opus = None;
                    fail(&shared, ListenFormat::Opus, &e);
                }
            }
        }
        true
    }
}

/// An encoder broke: end its listeners' streams (they may connect again)
fn fail(shared: &ListenShared, format: ListenFormat, error: &str) {
    eprintln!("Listening: {error}");
    shared
        .listeners
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|l| l.format != format);
}

/// Encode what comes in, and silence while nothing does, for the listeners
/// of `shared`
fn encoder_loop(shared: Weak<ListenShared>, rx: Receiver<EncoderMsg>) {
    let mut encoders = Encoders::default();
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
                let clamped: Vec<f32> = samples.iter().map(|s| s.clamp(-1.0, 1.0)).collect();
                if !encoders.encode(&shared, sample_rate, channels, &clamped) {
                    return;
                }
                last_audio = Instant::now();
                silence_until = None;
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
        if !encoders.encode(&shared, LISTEN_SAMPLE_RATE, LISTEN_CHANNELS, &silence) {
            return;
        }
        silence_until =
            Some(from + Duration::from_secs_f64(frames as f64 / f64::from(LISTEN_SAMPLE_RATE)));
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
    fn opus_listeners_get_one_packet_per_piece() {
        let listen = Listen::new();
        let mp3 = listen.subscribe("Radio").unwrap();
        let opus = listen.subscribe_as("Pixel", ListenFormat::Opus).unwrap();
        let generation = listen.generation_for(TapPoint::BeforeEq);
        for _ in 0..10 {
            listen.submit(generation, 44_100, 2, sine(44_100, 2, 4410));
        }
        // About a second: 50 packets of 20 ms
        let mut packets = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while packets.len() < 40 && Instant::now() < deadline {
            if let Ok(p) = opus.recv_timeout(Duration::from_millis(50)) {
                packets.push(p);
            }
        }
        assert!(packets.len() >= 40, "only {} packets", packets.len());
        for p in &packets {
            // TOC byte: a CELT (music) 20 ms frame, stereo, one frame
            assert!(!p.is_empty() && p.len() < 1275, "{} bytes", p.len());
            assert_eq!(p[0] >> 3 & 0b11, 3, "20 ms frames");
            assert_eq!(p[0] & 0b100, 0b100, "stereo");
        }
        // The MP3 listener still gets MP3
        let got = read(&mp3, 5_000, Duration::from_secs(5));
        assert!(count_frames(&got) > 0);
        assert_eq!(listen.listener_names(), ["Radio", "Pixel"]);
    }

    #[test]
    fn opus_alone_fills_in_silence_too() {
        let listen = Listen::new();
        let opus = listen.subscribe_as("Pixel", ListenFormat::Opus).unwrap();
        let mut packets = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while packets < 25 && Instant::now() < deadline {
            if opus.recv_timeout(Duration::from_millis(50)).is_ok() {
                packets += 1;
            }
        }
        assert!(packets >= 25, "only {packets} packets of silence");
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
