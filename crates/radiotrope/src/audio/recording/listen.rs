//! Listening on a phone: the playing station as one endless stream
//!
//! The same taps that feed recordings (see [`super::RecordingTap`]) copy
//! the decoded audio here while at least one listener is connected. One
//! encoder thread turns it into Opus packets for WebRTC at a fixed rate, and
//! hands each packet to every listener, so the format never changes,
//! whatever the station plays or how often it switches. Only the bitrate
//! follows the station (see [`listen_opus_kbps`]); Opus packets carry their
//! own size, so a phone needs no notice. When no
//! audio comes (stopped, buffering, switching station) the encoder fills in
//! silence at the pace of the clock, so a phone's player never runs dry.
//!
//! Like recording, listening never holds up playback: audio goes to the
//! encoder with `try_send`, and a listener that falls too far behind is
//! dropped (its stream ends, and the phone can connect again).

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};

use super::opus::OpusPackets;
use super::{TapPoint, QUEUE_BATCHES};

/// Sample rate silence is filled in at
const SILENCE_RATE: u32 = 44_100;

/// Channels of the stream
pub const LISTEN_CHANNELS: u16 = 2;

/// Bitrate of the Opus packets, in kbps (48 kHz stereo), for a station
/// sending `station_kbps`. Opus needs fewer bits than MP3 or AAC for the
/// same sound, and the phone can't hear more than the station sends, so
/// 160 kbps covers the best stations, and one we know nothing about.
pub fn listen_opus_kbps(station_kbps: Option<u32>) -> u32 {
    match station_kbps {
        Some(1..=64) => 96,
        Some(65..=128) => 128,
        Some(129..) | Some(0) | None => 160,
    }
}

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
    /// Advertised bitrate of the playing station in kbps, 0 when unknown
    station_kbps: AtomicU32,
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
    /// The stream is one Opus packet (20 ms, 48 kHz stereo) per piece and
    /// goes on until the returned [`ListenStream`] is dropped, or the
    /// listener falls behind.
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

    /// Tell the encoder the playing station's bitrate (`None` when it
    /// isn't known); the packets follow it from the next one on
    pub fn set_station_kbps(&self, kbps: Option<u32>) {
        self.shared
            .station_kbps
            .store(kbps.unwrap_or(0), Ordering::Relaxed);
    }

    /// Bitrate the Opus packets have now, in kbps
    pub fn opus_kbps(&self) -> u32 {
        listen_opus_kbps(self.station_kbps())
    }

    fn station_kbps(&self) -> Option<u32> {
        Some(self.shared.station_kbps.load(Ordering::Relaxed)).filter(|&k| k > 0)
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

/// One listener's stream of Opus packets. Dropping it leaves.
pub struct ListenStream {
    id: u64,
    rx: Receiver<Arc<[u8]>>,
    listen: Listen,
}

impl ListenStream {
    /// The next Opus packet, waiting at most `timeout`.
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

/// Give `packet` to every listener; drop the ones that fell behind or left
fn broadcast(shared: &ListenShared, packet: &[u8]) {
    let mut listeners = shared.listeners.lock().unwrap_or_else(|e| e.into_inner());
    let packet: Arc<[u8]> = packet.into();
    listeners.retain(|listener| match listener.tx.try_send(packet.clone()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
    });
}

/// The Opus encoder; it starts with the first listener and goes with the
/// last
#[derive(Default)]
struct Encoders {
    opus: Option<OpusPackets>,
    /// Bitrate `opus` was last set to
    kbps: u32,
}

impl Encoders {
    /// Encode `samples` for the listeners. False once nobody can listen
    /// any more (the [`Listen`] is gone).
    fn encode(
        &mut self,
        shared: &Weak<ListenShared>,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
    ) -> bool {
        let Some(shared) = shared.upgrade() else {
            return false;
        };
        let listening = !shared
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if !listening {
            self.opus = None;
            return true;
        }
        let kbps =
            listen_opus_kbps(Some(shared.station_kbps.load(Ordering::Relaxed)).filter(|&k| k > 0));
        if self.opus.is_none() {
            match OpusPackets::new(LISTEN_CHANNELS, kbps, OPUS_LOSS_PERCENT) {
                Ok(encoder) => {
                    self.opus = Some(encoder);
                    self.kbps = kbps;
                }
                Err(e) => fail(&shared, &e),
            }
        }
        if let Some(encoder) = self.opus.as_mut() {
            if self.kbps != kbps {
                encoder.set_bitrate(kbps);
                self.kbps = kbps;
            }
            let sent = encoder.encode(sample_rate, channels, samples, |packet| {
                broadcast(&shared, packet)
            });
            if let Err(e) = sent {
                self.opus = None;
                fail(&shared, &e);
            }
        }
        true
    }
}

/// The encoder broke: end the listeners' streams (they may connect again)
fn fail(shared: &ListenShared, error: &str) {
    eprintln!("Listening: {error}");
    shared
        .listeners
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
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
        let frames = (now.duration_since(from).as_secs_f64() * f64::from(SILENCE_RATE)) as u64;
        if frames == 0 {
            continue;
        }
        let silence = vec![0.0f32; frames as usize * usize::from(LISTEN_CHANNELS)];
        if !encoders.encode(&shared, SILENCE_RATE, LISTEN_CHANNELS, &silence) {
            return;
        }
        silence_until =
            Some(from + Duration::from_secs_f64(frames as f64 / f64::from(SILENCE_RATE)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read packets until `count` have come or `timeout` passes
    fn read(stream: &ListenStream, count: usize, timeout: Duration) -> Vec<Arc<[u8]>> {
        let deadline = Instant::now() + timeout;
        let mut got = Vec::new();
        while got.len() < count && Instant::now() < deadline {
            if let Ok(packet) = stream.recv_timeout(Duration::from_millis(50)) {
                got.push(packet);
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

    /// Each piece is one Opus packet: a CELT (music) 20 ms stereo frame
    fn assert_packets(packets: &[Arc<[u8]>]) {
        for p in packets {
            assert!(!p.is_empty() && p.len() < 1275, "{} bytes", p.len());
            assert_eq!(p[0] >> 3 & 0b11, 3, "20 ms frames");
            assert_eq!(p[0] & 0b100, 0b100, "stereo");
        }
    }

    #[test]
    fn silence_fills_in_while_nothing_plays() {
        let listen = Listen::new();
        let stream = listen.subscribe("Pixel").unwrap();
        // Half a second: 25 packets of 20 ms
        let got = read(&stream, 25, Duration::from_secs(5));
        assert!(got.len() >= 25, "only {} packets of silence", got.len());
        assert_packets(&got);
    }

    #[test]
    fn audio_at_any_rate_comes_out_as_one_format() {
        let listen = Listen::new();
        let stream = listen.subscribe("Pixel").unwrap();
        let generation = listen.generation_for(TapPoint::BeforeEq);
        assert_ne!(generation, 0);
        assert_eq!(listen.generation_for(TapPoint::AfterEq), 0);
        // A 48 kHz mono station, then a 44.1 kHz stereo one
        for _ in 0..10 {
            listen.submit(generation, 48_000, 1, sine(48_000, 1, 4800));
        }
        for _ in 0..10 {
            listen.submit(generation, 44_100, 2, sine(44_100, 2, 4410));
        }
        // About two seconds: 100 packets of 20 ms
        let got = read(&stream, 90, Duration::from_secs(5));
        assert!(got.len() >= 90, "only {} packets", got.len());
        assert_packets(&got);
    }

    #[test]
    fn the_bitrate_follows_the_station() {
        assert_eq!(listen_opus_kbps(Some(32)), 96);
        assert_eq!(listen_opus_kbps(Some(64)), 96);
        assert_eq!(listen_opus_kbps(Some(96)), 128);
        assert_eq!(listen_opus_kbps(Some(128)), 128);
        assert_eq!(listen_opus_kbps(Some(192)), 160);
        assert_eq!(listen_opus_kbps(Some(320)), 160);
        assert_eq!(listen_opus_kbps(Some(0)), 160);
        assert_eq!(listen_opus_kbps(None), 160);
    }

    /// White noise, which takes every bit the encoder may spend
    fn noise(seed: &mut u32, frames: usize) -> Vec<f32> {
        (0..frames * 2)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 17;
                *seed ^= *seed << 5;
                (*seed as f32 / u32::MAX as f32 - 0.5) * 0.8
            })
            .collect()
    }

    #[test]
    fn a_station_change_changes_the_packet_size() {
        let listen = Listen::new();
        let stream = listen.subscribe("Pixel").unwrap();
        let generation = listen.generation_for(TapPoint::BeforeEq);
        let mut seed = 1;
        // Average bytes per packet of a second of noise from a station
        // of `kbps`, leaving out the first packets after the change
        let mut sizes = |kbps: Option<u32>| {
            listen.set_station_kbps(kbps);
            assert_eq!(listen.opus_kbps(), listen_opus_kbps(kbps));
            for _ in 0..10 {
                listen.submit(generation, 48_000, 2, noise(&mut seed, 4800));
            }
            let got = read(&stream, 50, Duration::from_secs(5));
            assert_eq!(got.len(), 50);
            assert_packets(&got);
            got[10..].iter().map(|p| p.len()).sum::<usize>() / 40
        };
        let low = sizes(Some(64));
        let mid = sizes(Some(128));
        let high = sizes(Some(320));
        // 20 ms packets: 96 kbps is 240 bytes, 128 is 320, 160 is 400
        assert!((180..=290).contains(&low), "96 kbps: {low} bytes");
        assert!((260..=370).contains(&mid), "128 kbps: {mid} bytes");
        assert!((340..=480).contains(&high), "160 kbps: {high} bytes");
        assert!(low < mid && mid < high, "{low} {mid} {high}");
    }

    #[test]
    fn every_listener_gets_every_packet() {
        let listen = Listen::new();
        let a = listen.subscribe("Pixel").unwrap();
        let b = listen.subscribe("Xiaomi").unwrap();
        let generation = listen.generation_for(TapPoint::BeforeEq);
        for _ in 0..10 {
            listen.submit(generation, 44_100, 2, sine(44_100, 2, 4410));
        }
        let got_a = read(&a, 40, Duration::from_secs(5));
        let got_b = read(&b, 40, Duration::from_secs(5));
        assert_eq!(got_a.len(), 40);
        assert_eq!(got_a, got_b);
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
        assert!(!read(&c, 5, Duration::from_secs(5)).is_empty());
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
}
