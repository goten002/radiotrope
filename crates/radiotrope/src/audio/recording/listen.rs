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
//! When the player fades a station out (Smooth Switching), the stream
//! fades it out too. The next station starts on the speakers while the
//! last one fades, but a stream can't play two things at once: the next
//! station's audio waits until the fade is over, so the two don't take
//! turns in the stream (which trembles).
//!
//! Packets go out at the pace they play, one every 20 ms. A station that
//! starts hands over a burst (the audio decoded ahead for the speakers),
//! and a burst makes a phone's jitter buffer speed the sound up to catch
//! up, which warbles; paced, the phone hears an even stream.
//!
//! Like recording, listening never holds up playback: audio goes to the
//! encoder with `try_send`, and a listener that falls too far behind is
//! dropped (its stream ends, and the phone can connect again).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};

use super::opus::OpusPackets;
use super::{TapPoint, QUEUE_BATCHES};
use crate::config::sound::FADE_OUT_MS;

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

/// How long one Opus packet plays
const PACKET_TIME: Duration = Duration::from_millis(20);

/// Most packets held back for pacing (1 s); more go out at once, so the
/// stream never lags far behind (the sound card's clock and the system's
/// drift apart a little)
const MOST_PACED: usize = 50;

/// How long the next station's audio waits for the last one's fade at
/// most, should the fading station stop sending
const FADE_WAIT: Duration = Duration::from_millis(FADE_OUT_MS + 200);

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

/// A piece of audio from a tap
struct Pcm {
    /// The station it is from (see [`Listen::next_station`])
    station: u64,
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
}

enum EncoderMsg {
    Pcm(Pcm),
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
    /// `TapPoint::id()` listeners hear; 0 is the default (after the EQ, so
    /// phones hear what the speakers play, before volume)
    tap: AtomicU8,
    /// Pieces the taps couldn't hand to a busy encoder
    dropped_batches: AtomicU64,
    /// Advertised bitrate of the playing station in kbps, 0 when unknown
    station_kbps: AtomicU32,
    /// The station playing (see [`Listen::next_station`])
    station: AtomicU64,
    /// The station fading out, 0 for none
    fading: AtomicU64,
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

    /// Where in the playback chain listeners take the audio from (after
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
            1 => TapPoint::BeforeEq,
            _ => TapPoint::AfterEq,
        }
    }

    /// A new station starts: the audio its taps hand over from now on is
    /// told apart from the last station's, which may still be fading out
    pub fn next_station(&self) -> u64 {
        self.shared.station.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The station playing, as its taps hand it over
    pub(super) fn station(&self) -> u64 {
        self.shared.station.load(Ordering::SeqCst)
    }

    /// The station playing fades out: listeners hear it go quiet over
    /// [`FADE_OUT_MS`], then silence until the next one
    pub fn fade_out(&self) {
        self.shared
            .fading
            .store(self.shared.station.load(Ordering::SeqCst), Ordering::SeqCst);
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
        station: u64,
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    ) {
        let encoder = self.lock_encoder();
        let Some(encoder) = encoder.as_ref().filter(|e| e.generation == generation) else {
            return;
        };
        let msg = EncoderMsg::Pcm(Pcm {
            station,
            sample_rate,
            channels,
            samples,
        });
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
        pacer: &mut Pacer,
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
            pacer.clear();
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
            let now = Instant::now();
            let sent = encoder.encode(sample_rate, channels, samples, |packet| {
                pacer.push(packet.into(), now)
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
    let mut switch = Switch::default();
    let mut pacer = Pacer::default();
    let mut ready = Vec::new();
    let mut last_audio = Instant::now();
    // How far silence has been filled in since the last audio
    let mut silence_until: Option<Instant> = None;
    loop {
        let wait = pacer
            .wait(Instant::now())
            .map_or(IDLE_TICK, |w| w.min(IDLE_TICK));
        let fading = || {
            shared
                .upgrade()
                .map_or(0, |s| s.fading.load(Ordering::SeqCst))
        };
        match rx.recv_timeout(wait) {
            Ok(EncoderMsg::Pcm(mut pcm)) => {
                for x in &mut pcm.samples {
                    *x = x.clamp(-1.0, 1.0);
                }
                switch.take(pcm, fading(), Instant::now(), &mut ready);
            }
            Ok(EncoderMsg::Finish) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        switch.expire(fading(), Instant::now(), &mut ready);
        for pcm in ready.drain(..) {
            if !encoders.encode(
                &shared,
                &mut pacer,
                pcm.sample_rate,
                pcm.channels,
                &pcm.samples,
            ) {
                return;
            }
            last_audio = Instant::now();
            silence_until = None;
        }

        // Silence is due once the phone would run out: the audio has
        // stopped coming, and what is paced has gone out
        let now = Instant::now();
        let quiet_since = pacer.runs_out().map_or(last_audio, |t| t.max(last_audio));
        if pacer.is_empty()
            && now.saturating_duration_since(quiet_since) >= SILENCE_AFTER
            && !switch.holding()
        {
            let from = silence_until.unwrap_or(quiet_since);
            let frames = (now.duration_since(from).as_secs_f64() * f64::from(SILENCE_RATE)) as u64;
            if frames > 0 {
                let silence = vec![0.0f32; frames as usize * usize::from(LISTEN_CHANNELS)];
                if !encoders.encode(&shared, &mut pacer, SILENCE_RATE, LISTEN_CHANNELS, &silence) {
                    return;
                }
                silence_until =
                    Some(from + Duration::from_secs_f64(frames as f64 / f64::from(SILENCE_RATE)));
            }
        }

        let Some(listening) = shared.upgrade() else {
            return;
        };
        pacer.send_due(Instant::now(), |packet| broadcast(&listening, packet));
    }
}

/// Puts a switch in order. While the last station fades out, the next
/// one's audio waits; once the fade is over, what the fading station still
/// sends is dropped (it is silent, and would hold the next one back).
#[derive(Default)]
struct Switch {
    fade: Fade,
    /// The next station's audio, waiting for the fade
    waiting: VecDeque<Pcm>,
    /// When the first of it came
    waiting_since: Option<Instant>,
}

impl Switch {
    /// Take a piece of audio while station `fading` fades out (0 for
    /// none); what may be encoded now goes to `ready`, in order
    fn take(&mut self, mut pcm: Pcm, fading: u64, now: Instant, ready: &mut Vec<Pcm>) {
        if fading != 0 && pcm.station == fading {
            if self.fade.finished(fading) {
                return;
            }
            self.fade
                .apply(fading, pcm.sample_rate, pcm.channels, &mut pcm.samples);
            ready.push(pcm);
            if self.fade.finished(fading) {
                self.release(ready);
            }
        } else if fading != 0 && !self.fade.finished(fading) {
            self.waiting_since.get_or_insert(now);
            self.waiting.push_back(pcm);
        } else {
            self.release(ready);
            ready.push(pcm);
        }
    }

    /// Let the next station through if the fading one stopped sending
    fn expire(&mut self, fading: u64, now: Instant, ready: &mut Vec<Pcm>) {
        let stale = self
            .waiting_since
            .is_some_and(|since| now.duration_since(since) >= FADE_WAIT);
        if stale || (fading != 0 && self.fade.finished(fading)) || fading == 0 {
            if stale {
                self.fade.finish(fading);
            }
            self.release(ready);
        }
    }

    fn release(&mut self, ready: &mut Vec<Pcm>) {
        ready.extend(self.waiting.drain(..));
        self.waiting_since = None;
    }

    /// Whether audio is waiting (so no silence is due)
    fn holding(&self) -> bool {
        !self.waiting.is_empty()
    }
}

/// Sends packets at the pace they play, so a burst of audio reaches a
/// phone as an even stream
#[derive(Default)]
struct Pacer {
    queue: VecDeque<Arc<[u8]>>,
    /// When the next packet is due
    next: Option<Instant>,
}

impl Pacer {
    fn push(&mut self, packet: Arc<[u8]>, now: Instant) {
        // Nothing waiting and the clock past due: start over from now,
        // rather than catch up on the time nothing came
        if self.queue.is_empty() && self.next.is_none_or(|next| next < now) {
            self.next = Some(now);
        }
        self.queue.push_back(packet);
    }

    /// Send what is due by `now`, and what is over [`MOST_PACED`]
    fn send_due(&mut self, now: Instant, mut send: impl FnMut(&[u8])) {
        while let Some(packet) = self.queue.front() {
            let due = self.next.unwrap_or(now);
            if due > now && self.queue.len() <= MOST_PACED {
                break;
            }
            send(packet);
            self.queue.pop_front();
            self.next = Some(due + PACKET_TIME);
        }
    }

    /// How long until the next packet is due, if one waits
    fn wait(&self, now: Instant) -> Option<Duration> {
        (!self.queue.is_empty()).then(|| {
            self.next
                .map_or(Duration::ZERO, |n| n.saturating_duration_since(now))
        })
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.next = None;
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// When what has been sent finishes playing
    fn runs_out(&self) -> Option<Instant> {
        self.next
    }
}

/// A station fading out, as the speakers hear it (see `OutputStage`)
#[derive(Default)]
struct Fade {
    station: u64,
    /// Frames of it faded so far, of `total`
    frames: u64,
    total: u64,
}

impl Fade {
    /// Fade `samples` of `station`, which fades out; silence once it has
    fn apply(&mut self, station: u64, sample_rate: u32, channels: u16, samples: &mut [f32]) {
        if station != self.station {
            self.station = station;
            self.frames = 0;
        }
        let total = (u64::from(sample_rate) * FADE_OUT_MS / 1000).max(1);
        self.total = total;
        for frame in samples.chunks_mut(usize::from(channels.max(1))) {
            let t = 1.0 - (self.frames.min(total) as f32 / total as f32);
            // Smoothstep down: no corner at either end
            let gain = t * t * (3.0 - 2.0 * t);
            for x in frame {
                *x *= gain;
            }
            self.frames += 1;
        }
    }

    /// Whether `station` has faded all the way out
    fn finished(&self, station: u64) -> bool {
        self.station == station && self.total > 0 && self.frames >= self.total
    }

    /// Count `station` as faded out
    fn finish(&mut self, station: u64) {
        self.station = station;
        self.total = 1;
        self.frames = 1;
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
        let generation = listen.generation_for(TapPoint::AfterEq);
        assert_ne!(generation, 0);
        assert_eq!(listen.generation_for(TapPoint::BeforeEq), 0);
        // A 48 kHz mono station, then a 44.1 kHz stereo one
        for _ in 0..10 {
            listen.submit(generation, 0, 48_000, 1, sine(48_000, 1, 4800));
        }
        for _ in 0..10 {
            listen.submit(generation, 0, 44_100, 2, sine(44_100, 2, 4410));
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
        let generation = listen.generation_for(TapPoint::AfterEq);
        let mut seed = 1;
        // Average bytes per packet of a second of noise from a station
        // of `kbps`, leaving out the first packets after the change
        let mut sizes = |kbps: Option<u32>| {
            listen.set_station_kbps(kbps);
            assert_eq!(listen.opus_kbps(), listen_opus_kbps(kbps));
            for _ in 0..10 {
                listen.submit(generation, 0, 48_000, 2, noise(&mut seed, 4800));
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
        let generation = listen.generation_for(TapPoint::AfterEq);
        for _ in 0..10 {
            listen.submit(generation, 0, 44_100, 2, sine(44_100, 2, 4410));
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
        assert_ne!(listen.generation_for(TapPoint::AfterEq), 0);
        drop(b);
        assert_eq!(listen.listeners(), 0);
        assert_eq!(listen.generation_for(TapPoint::AfterEq), 0);
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
        let generation = listen.generation_for(TapPoint::AfterEq);
        // Far more audio than the listener's queue holds, never read
        for _ in 0..(LISTENER_QUEUE * 3) {
            listen.submit(generation, 0, 44_100, 2, sine(44_100, 2, 2048));
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
    fn a_fading_station_goes_quiet_then_silent() {
        let mut fade = Fade::default();
        // 400 ms at 1 kHz is 400 frames: hand over 600 stereo frames of 1.0
        let mut samples = vec![1.0f32; 1_200];
        fade.apply(7, 1_000, 2, &mut samples);
        assert_eq!(samples[0], 1.0, "starts where it was");
        assert!(samples.windows(2).all(|w| w[1] <= w[0]), "only goes down");
        assert!((samples[400] - 0.5).abs() < 0.01, "halfway at half time");
        assert!(samples[800..].iter().all(|&x| x == 0.0), "silent after");
        // It carries on across batches, and starts over for the next fade
        let mut more = vec![1.0f32; 10];
        fade.apply(7, 1_000, 2, &mut more);
        assert!(more.iter().all(|&x| x == 0.0));
        fade.apply(8, 1_000, 2, &mut more);
        let mut next = vec![1.0f32; 2];
        fade.apply(9, 1_000, 2, &mut next);
        assert_eq!(next[0], 1.0);
    }

    #[test]
    fn only_the_station_playing_fades() {
        let listen = Listen::new();
        let first = listen.next_station();
        listen.fade_out();
        let second = listen.next_station();
        assert_eq!(listen.shared.fading.load(Ordering::SeqCst), first);
        assert_ne!(first, second);
        assert_eq!(listen.station(), second);
    }

    fn pcm(station: u64, frames: usize, value: f32) -> Pcm {
        Pcm {
            station,
            sample_rate: 1_000,
            channels: 1,
            samples: vec![value; frames],
        }
    }

    /// The next station starts while the last fades (as on the speakers):
    /// its audio waits, so the two don't take turns in the stream
    #[test]
    fn the_next_station_waits_for_the_fade() {
        let mut switch = Switch::default();
        let mut ready = Vec::new();
        let now = Instant::now();
        // Station 1 fades (400 frames at 1 kHz); station 2 comes in between
        switch.take(pcm(1, 100, 1.0), 1, now, &mut ready);
        switch.take(pcm(2, 100, 0.5), 1, now, &mut ready);
        switch.take(pcm(1, 100, 1.0), 1, now, &mut ready);
        switch.take(pcm(2, 100, 0.5), 1, now, &mut ready);
        switch.expire(1, now, &mut ready);
        assert_eq!(
            ready.iter().map(|p| p.station).collect::<Vec<_>>(),
            [1, 1],
            "station 2 waits"
        );
        assert!(switch.holding());
        // The fade ends: station 2 follows, in order
        switch.take(pcm(1, 300, 1.0), 1, now, &mut ready);
        assert_eq!(
            ready.iter().map(|p| p.station).collect::<Vec<_>>(),
            [1, 1, 1, 2, 2]
        );
        assert!(!switch.holding());
        // What station 1 still sends is silent and dropped; 2 plays on
        ready.clear();
        switch.take(pcm(1, 100, 1.0), 1, now, &mut ready);
        switch.take(pcm(2, 100, 0.5), 1, now, &mut ready);
        assert_eq!(ready.iter().map(|p| p.station).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn the_next_station_goes_on_if_the_fading_one_stops() {
        let mut switch = Switch::default();
        let mut ready = Vec::new();
        let now = Instant::now();
        switch.take(pcm(1, 100, 1.0), 1, now, &mut ready);
        switch.take(pcm(2, 100, 0.5), 1, now, &mut ready);
        switch.expire(1, now + FADE_WAIT / 2, &mut ready);
        assert_eq!(ready.len(), 1);
        switch.expire(1, now + FADE_WAIT, &mut ready);
        assert_eq!(ready.iter().map(|p| p.station).collect::<Vec<_>>(), [1, 2]);
        // And station 1 counts as gone
        ready.clear();
        switch.take(pcm(1, 100, 1.0), 1, now, &mut ready);
        assert!(ready.is_empty());
    }

    #[test]
    fn without_a_fade_audio_passes_straight_through() {
        let mut switch = Switch::default();
        let mut ready = Vec::new();
        let now = Instant::now();
        switch.take(pcm(1, 100, 1.0), 0, now, &mut ready);
        switch.take(pcm(2, 100, 1.0), 0, now, &mut ready);
        assert_eq!(ready.len(), 2);
        assert!(ready.iter().all(|p| p.samples.iter().all(|&x| x == 1.0)));
    }

    #[test]
    fn packets_go_out_at_the_pace_they_play() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        // A burst of 10 packets
        for i in 0..10u8 {
            pacer.push(Arc::from(&[i][..]), start);
        }
        let mut sent = Vec::new();
        pacer.send_due(start, |p| sent.push(p[0]));
        assert_eq!(sent, [0], "one now");
        pacer.send_due(start + Duration::from_millis(19), |p| sent.push(p[0]));
        assert_eq!(sent.len(), 1, "none early");
        assert_eq!(
            pacer.wait(start + Duration::from_millis(19)),
            Some(Duration::from_millis(1))
        );
        pacer.send_due(start + Duration::from_millis(60), |p| sent.push(p[0]));
        assert_eq!(sent, [0, 1, 2, 3], "one per 20 ms, in order");
        // After a pause it starts over, rather than rushing to catch up
        pacer.send_due(start + Duration::from_secs(10), |p| sent.push(p[0]));
        assert_eq!(sent.len(), 10);
        let later = start + Duration::from_secs(20);
        pacer.push(Arc::from(&[10u8][..]), later);
        pacer.push(Arc::from(&[11u8][..]), later);
        pacer.send_due(later, |p| sent.push(p[0]));
        assert_eq!(sent.len(), 11);
        assert_eq!(pacer.wait(later), Some(PACKET_TIME));
    }

    #[test]
    fn the_pacer_never_holds_back_more_than_a_second() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        for _ in 0..(MOST_PACED + 20) {
            pacer.push(Arc::from(&[0u8][..]), start);
        }
        let mut count = 0;
        pacer.send_due(start, |_| count += 1);
        assert_eq!(count, 20);
        assert_eq!(pacer.queue.len(), MOST_PACED);
    }
}
