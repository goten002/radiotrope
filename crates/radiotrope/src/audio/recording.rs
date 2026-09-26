//! Station recording
//!
//! Records the decoded audio of the playing station to an MP3 file.
//!
//! Two [`RecordingTap`] sources sit in the playback chain, one before and one
//! after the equalizer. Both pass audio through untouched; while a recording
//! is running, the tap chosen by [`TapPoint`] also copies the samples to a
//! writer thread, which encodes them with LAME and writes the file. Volume is
//! applied later by the output sink, so it never affects the recording.
//!
//! The taps never block playback: batches are handed over with `try_send`,
//! and if the writer falls behind, batches are dropped and counted.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::mem;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use mp3lame_encoder::{Bitrate, Builder, Encoder, FlushNoGap, InterleavedPcm, Mode, MonoPcm};
use rodio::Source;

use crate::error::RadioError;

/// Samples per batch sent to the writer (about 46 ms of 44.1 kHz stereo).
const BATCH_SAMPLES: usize = 4096;

/// Batches the writer may fall behind by before taps drop audio (~12 s).
const QUEUE_BATCHES: usize = 256;

/// How many samples a tap passes between checks of the recording state.
const STATE_CHECK_INTERVAL: u32 = 1024;

/// How often the writer flushes the file while recording.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// How long `stop()` waits for the writer to finish the file.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// MP3 bitrate for stereo recordings.
const STEREO_BITRATE: Bitrate = Bitrate::Kbps192;

/// MP3 bitrate for mono recordings.
const MONO_BITRATE: Bitrate = Bitrate::Kbps96;

/// Where in the playback chain a recording takes its audio from
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapPoint {
    /// The station's sound as decoded, before the equalizer
    BeforeEq,
    /// The sound after the equalizer (still before volume)
    AfterEq,
}

impl TapPoint {
    fn id(self) -> u8 {
        match self {
            TapPoint::BeforeEq => 1,
            TapPoint::AfterEq => 2,
        }
    }
}

/// Tag fields written at the start of the MP3 file
#[derive(Debug, Clone, Default)]
pub struct RecordingTags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub comment: String,
    /// Cover art as PNG or JPEG bytes
    pub cover: Option<Vec<u8>>,
}

/// What to record and where
#[derive(Debug, Clone)]
pub struct RecordingOptions {
    /// File to create. It must not exist yet.
    pub path: PathBuf,
    pub tap: TapPoint,
    pub tags: RecordingTags,
}

/// Progress of the current (or just finished) recording
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingStatus {
    pub path: PathBuf,
    /// Bytes written to the file so far
    pub bytes_written: u64,
    /// Length of audio recorded so far
    pub duration: Duration,
    /// Batches lost because the writer fell behind
    pub dropped_batches: u64,
    /// Set when writing failed; the recording has stopped
    pub error: Option<String>,
}

enum WriterMsg {
    Pcm {
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    },
    Finish,
}

#[derive(Default)]
struct WriterStats {
    bytes_written: AtomicU64,
    duration_micros: AtomicU64,
    dropped_batches: AtomicU64,
    failed: AtomicBool,
    error: Mutex<Option<String>>,
}

struct ActiveRecording {
    generation: u64,
    path: PathBuf,
    tx: Sender<WriterMsg>,
    handle: JoinHandle<()>,
    stats: Arc<WriterStats>,
}

#[derive(Default)]
struct RecorderShared {
    active: Mutex<Option<ActiveRecording>>,
    /// Generation of the running recording, 0 when idle
    generation: AtomicU64,
    /// `TapPoint::id()` of the running recording, 0 when idle
    tap: AtomicU8,
    next_generation: AtomicU64,
}

/// Starts and stops recordings. Cheap to clone; all clones share state.
#[derive(Clone, Default)]
pub struct Recorder {
    shared: Arc<RecorderShared>,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start recording to `options.path`.
    ///
    /// Fails if a recording is already running or the file cannot be
    /// created (it must not exist yet).
    pub fn start(&self, options: RecordingOptions) -> Result<(), RadioError> {
        let mut active = self.lock_active();
        if active.is_some() {
            return Err(RadioError::Audio("Already recording".to_string()));
        }

        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&options.path)
            .map_err(|e| {
                RadioError::Audio(format!("Cannot create {}: {e}", options.path.display()))
            })?;

        let (tx, rx) = bounded(QUEUE_BATCHES);
        let stats = Arc::new(WriterStats::default());
        let header = id3v2_tag(&options.tags);
        let writer_stats = stats.clone();
        let handle = thread::Builder::new()
            .name("recording-writer".to_string())
            .spawn(move || writer_loop(file, header, rx, writer_stats))
            .map_err(|e| RadioError::Audio(format!("Failed to spawn recording thread: {e}")))?;

        let generation = self.shared.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        *active = Some(ActiveRecording {
            generation,
            path: options.path,
            tx,
            handle,
            stats,
        });
        self.shared.tap.store(options.tap.id(), Ordering::SeqCst);
        self.shared.generation.store(generation, Ordering::SeqCst);
        Ok(())
    }

    /// Stop recording, finish the file, and return its final status.
    ///
    /// Returns `None` if nothing was being recorded.
    pub fn stop(&self) -> Option<RecordingStatus> {
        let active = {
            let mut guard = self.lock_active();
            self.shared.generation.store(0, Ordering::SeqCst);
            self.shared.tap.store(0, Ordering::SeqCst);
            guard.take()?
        };

        // The writer may have exited on an error, so don't wait forever.
        let _ = active.tx.send_timeout(WriterMsg::Finish, STOP_TIMEOUT);
        drop(active.tx);
        let _ = active.handle.join();
        Some(status_of(&active.path, &active.stats))
    }

    /// Whether a recording is running
    pub fn is_recording(&self) -> bool {
        self.shared.generation.load(Ordering::SeqCst) != 0
    }

    /// Progress of the running recording, `None` when idle.
    ///
    /// If writing failed, `error` is set; call [`Recorder::stop`] to clean up.
    pub fn status(&self) -> Option<RecordingStatus> {
        let active = self.lock_active();
        active.as_ref().map(|a| status_of(&a.path, &a.stats))
    }

    /// Generation of the running recording if it records from `tap`, else 0.
    fn generation_for(&self, tap: TapPoint) -> u64 {
        if self.shared.tap.load(Ordering::SeqCst) == tap.id() {
            self.shared.generation.load(Ordering::SeqCst)
        } else {
            0
        }
    }

    /// Hand a batch to the writer if recording `generation` is still running.
    fn submit(&self, generation: u64, sample_rate: u32, channels: u16, samples: Vec<f32>) {
        let active = self.lock_active();
        let Some(active) = active.as_ref().filter(|a| a.generation == generation) else {
            return;
        };
        let msg = WriterMsg::Pcm {
            sample_rate,
            channels,
            samples,
        };
        match active.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                active.stats.dropped_batches.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn lock_active(&self) -> std::sync::MutexGuard<'_, Option<ActiveRecording>> {
        self.shared.active.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for RecorderShared {
    fn drop(&mut self) {
        // Last handle gone (engine shut down): finish the file.
        let active = self
            .active
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(active) = active {
            let _ = active.tx.send_timeout(WriterMsg::Finish, STOP_TIMEOUT);
            drop(active.tx);
            let _ = active.handle.join();
        }
    }
}

fn status_of(path: &Path, stats: &WriterStats) -> RecordingStatus {
    let error = if stats.failed.load(Ordering::SeqCst) {
        stats
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    } else {
        None
    };
    RecordingStatus {
        path: path.to_path_buf(),
        bytes_written: stats.bytes_written.load(Ordering::Relaxed),
        duration: Duration::from_micros(stats.duration_micros.load(Ordering::Relaxed)),
        dropped_batches: stats.dropped_batches.load(Ordering::Relaxed),
        error,
    }
}

// ---------------------------------------------------------------------------
// Tap source
// ---------------------------------------------------------------------------

/// Passes audio through and copies it to the [`Recorder`] while a recording
/// that uses this tap point is running.
pub struct RecordingTap<S> {
    inner: S,
    recorder: Recorder,
    point: TapPoint,
    generation: u64,
    until_check: u32,
    batch: Vec<f32>,
    batch_rate: u32,
    batch_channels: u16,
}

impl<S> RecordingTap<S>
where
    S: Source<Item = f32>,
{
    pub fn new(inner: S, recorder: Recorder, point: TapPoint) -> Self {
        Self {
            inner,
            recorder,
            point,
            generation: 0,
            until_check: 0,
            batch: Vec::new(),
            batch_rate: 0,
            batch_channels: 0,
        }
    }

    fn send_batch(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let samples = mem::replace(&mut self.batch, Vec::with_capacity(BATCH_SAMPLES));
        self.recorder.submit(
            self.generation,
            self.batch_rate,
            self.batch_channels,
            samples,
        );
    }
}

impl<S> Iterator for RecordingTap<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next()?;

        if self.until_check == 0 {
            self.until_check = STATE_CHECK_INTERVAL;
            let generation = self.recorder.generation_for(self.point);
            if generation != self.generation {
                // Started, stopped or restarted: don't carry audio across.
                self.batch.clear();
                self.generation = generation;
            }
        }
        self.until_check -= 1;

        if self.generation != 0 {
            // Read after next(): the format of the sample just returned.
            let rate = self.inner.sample_rate().get();
            let channels = self.inner.channels().get();
            if rate != self.batch_rate || channels != self.batch_channels {
                self.send_batch();
                self.batch_rate = rate;
                self.batch_channels = channels;
            }
            self.batch.push(sample);
            // Whole frames only, so the writer can split channels.
            if self.batch.len() >= BATCH_SAMPLES
                && self.batch.len().is_multiple_of(channels as usize)
            {
                self.send_batch();
            }
        }

        Some(sample)
    }
}

impl<S> Source for RecordingTap<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> NonZero<u16> {
        self.inner.channels()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

impl<S> Drop for RecordingTap<S> {
    fn drop(&mut self) {
        if self.generation != 0 && !self.batch.is_empty() {
            let samples = mem::take(&mut self.batch);
            self.recorder.submit(
                self.generation,
                self.batch_rate,
                self.batch_channels,
                samples,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------

/// A LAME encoder set up for one input format.
struct Mp3Encoder {
    encoder: Encoder,
    sample_rate: u32,
    channels: u16,
    /// Reused buffer for the first two channels of multichannel audio
    stereo: Vec<f32>,
}

impl Mp3Encoder {
    fn new(sample_rate: u32, channels: u16) -> Result<Self, String> {
        let mono = channels == 1;
        let mut builder = Builder::new().ok_or("Could not create the MP3 encoder")?;
        builder
            .set_sample_rate(sample_rate)
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_num_channels(if mono { 1 } else { 2 })
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_brate(if mono { MONO_BITRATE } else { STEREO_BITRATE })
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_mode(if mono { Mode::Mono } else { Mode::JointStereo })
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_quality(mp3lame_encoder::Quality::NearBest)
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        let encoder = builder.build().map_err(|e| format!("MP3 encoder: {e}"))?;
        Ok(Self {
            encoder,
            sample_rate,
            channels,
            stereo: Vec::new(),
        })
    }

    /// Encode interleaved samples, appending MP3 data to `out`.
    fn encode(&mut self, samples: &mut [f32], out: &mut Vec<u8>) -> Result<(), String> {
        for s in samples.iter_mut() {
            // The EQ has no limiter; clip like the sound card would.
            *s = if s.is_finite() {
                s.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        let frames = samples.len() / self.channels as usize;
        // LAME's worst case: 1.25 * samples + 7200 bytes.
        out.reserve(frames * 5 / 4 + 7200);
        let result = match self.channels {
            1 => self.encoder.encode_to_vec(MonoPcm(samples), out),
            2 => self.encoder.encode_to_vec(InterleavedPcm(samples), out),
            n => {
                // Keep front left/right of surround streams.
                self.stereo.clear();
                for frame in samples.chunks_exact(n as usize) {
                    self.stereo.extend_from_slice(&frame[..2]);
                }
                self.encoder
                    .encode_to_vec(InterleavedPcm(&self.stereo), out)
            }
        };
        result
            .map(|_| ())
            .map_err(|e| format!("MP3 encoding failed: {e}"))
    }

    /// Flush the encoder's last frames into `out`.
    fn finish(&mut self, out: &mut Vec<u8>) {
        out.reserve(7200);
        let _ = self.encoder.flush_to_vec::<FlushNoGap>(out);
    }
}

fn writer_loop(file: File, header: Vec<u8>, rx: Receiver<WriterMsg>, stats: Arc<WriterStats>) {
    if let Err(e) = write_recording(file, header, &rx, &stats) {
        *stats.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
        stats.failed.store(true, Ordering::SeqCst);
    }
}

fn write_recording(
    file: File,
    header: Vec<u8>,
    rx: &Receiver<WriterMsg>,
    stats: &WriterStats,
) -> Result<(), String> {
    let io_err = |e: std::io::Error| format!("Writing the recording failed: {e}");
    let mut out = BufWriter::with_capacity(64 * 1024, file);
    let mut encoder: Option<Mp3Encoder> = None;
    let mut mp3 = Vec::new();

    out.write_all(&header).map_err(io_err)?;
    stats
        .bytes_written
        .fetch_add(header.len() as u64, Ordering::Relaxed);

    loop {
        match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok(WriterMsg::Pcm {
                sample_rate,
                channels,
                mut samples,
            }) => {
                if sample_rate == 0 || channels == 0 {
                    continue;
                }
                let format_changed = encoder
                    .as_ref()
                    .is_some_and(|e| e.sample_rate != sample_rate || e.channels != channels);
                if format_changed {
                    // E.g. HE-AAC switching rate: finish these frames, go on
                    // in the new format in the same file.
                    if let Some(mut old) = encoder.take() {
                        old.finish(&mut mp3);
                    }
                }
                if encoder.is_none() {
                    encoder = Some(Mp3Encoder::new(sample_rate, channels)?);
                }
                if let Some(enc) = encoder.as_mut() {
                    enc.encode(&mut samples, &mut mp3)?;
                }
                let frames = (samples.len() / channels as usize) as u64;
                stats
                    .duration_micros
                    .fetch_add(frames * 1_000_000 / sample_rate as u64, Ordering::Relaxed);
            }
            Ok(WriterMsg::Finish) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                out.flush().map_err(io_err)?;
                continue;
            }
        }
        if !mp3.is_empty() {
            out.write_all(&mp3).map_err(io_err)?;
            stats
                .bytes_written
                .fetch_add(mp3.len() as u64, Ordering::Relaxed);
            mp3.clear();
        }
    }

    if let Some(mut enc) = encoder.take() {
        enc.finish(&mut mp3);
    }
    out.write_all(&mp3).map_err(io_err)?;
    stats
        .bytes_written
        .fetch_add(mp3.len() as u64, Ordering::Relaxed);
    let file = out.into_inner().map_err(|e| io_err(e.into_error()))?;
    file.sync_all().map_err(io_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ID3v2 tag
// ---------------------------------------------------------------------------

/// Build an ID3v2.4 tag (UTF-8 text) for the start of the file.
///
/// Returns an empty vector when no field is set.
pub fn id3v2_tag(tags: &RecordingTags) -> Vec<u8> {
    let mut frames = Vec::new();
    for (id, text) in [
        (b"TIT2", &tags.title),
        (b"TPE1", &tags.artist),
        (b"TALB", &tags.album),
    ] {
        if !text.is_empty() {
            let mut body = vec![3u8]; // UTF-8
            body.extend_from_slice(text.as_bytes());
            push_frame(&mut frames, id, &body);
        }
    }
    if !tags.comment.is_empty() {
        let mut body = vec![3u8];
        body.extend_from_slice(b"eng");
        body.push(0); // empty description
        body.extend_from_slice(tags.comment.as_bytes());
        push_frame(&mut frames, b"COMM", &body);
    }
    if let Some(cover) = tags.cover.as_deref().filter(|c| !c.is_empty()) {
        let mime: &[u8] = if cover.starts_with(b"\x89PNG") {
            b"image/png"
        } else {
            b"image/jpeg"
        };
        let mut body = vec![3u8];
        body.extend_from_slice(mime);
        body.push(0);
        body.push(3); // front cover
        body.push(0); // empty description
        body.extend_from_slice(cover);
        push_frame(&mut frames, b"APIC", &body);
    }
    if frames.is_empty() {
        return frames;
    }

    let mut tag = Vec::with_capacity(10 + frames.len());
    tag.extend_from_slice(b"ID3\x04\x00\x00");
    tag.extend_from_slice(&synchsafe(frames.len() as u32));
    tag.extend_from_slice(&frames);
    tag
}

fn push_frame(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&synchsafe(body.len() as u32));
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(body);
}

/// 28-bit synchsafe integer (7 bits per byte) as used by ID3v2.4 sizes.
fn synchsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decoder::SymphoniaSource;
    use rodio::buffer::SamplesBuffer;
    use std::io::Cursor;

    fn temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("radiotrope-rec-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("test.mp3")
    }

    fn sine(sample_rate: u32, channels: u16, seconds: f32) -> Vec<f32> {
        let frames = (sample_rate as f32 * seconds) as usize;
        let mut out = Vec::with_capacity(frames * channels as usize);
        for i in 0..frames {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / sample_rate as f32).sin() * 0.5;
            for _ in 0..channels {
                out.push(v);
            }
        }
        out
    }

    fn options(path: &Path, tap: TapPoint) -> RecordingOptions {
        RecordingOptions {
            path: path.to_path_buf(),
            tap,
            tags: RecordingTags {
                title: "Test FM, 2026-09-26 20:15".to_string(),
                artist: "Test FM".to_string(),
                ..Default::default()
            },
        }
    }

    /// Pull every sample through a tap, like the output sink would.
    fn play_through(source: SamplesBuffer, recorder: &Recorder, point: TapPoint) {
        let tap = RecordingTap::new(source, recorder.clone(), point);
        for _ in tap {}
    }

    /// Decode an MP3 file with the engine's decoder; returns
    /// (sample_rate, channels, frames).
    fn decode_mp3(path: &Path) -> (u32, usize, usize) {
        let data = std::fs::read(path).unwrap();
        let mut source = SymphoniaSource::new_with_hint(Cursor::new(data), Some("mp3"))
            .expect("recording should probe as MP3");
        let mut samples = 0;
        let mut rate = 0;
        let mut channels = 0;
        while source.next().is_some() {
            samples += 1;
            rate = source.sample_rate().get();
            channels = source.channels().get() as usize;
        }
        (rate, channels, samples / channels.max(1))
    }

    #[test]
    fn records_stereo_to_playable_mp3() {
        let path = temp_path("stereo");
        let recorder = Recorder::new();
        recorder.start(options(&path, TapPoint::BeforeEq)).unwrap();
        assert!(recorder.is_recording());

        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44_100).unwrap(),
            sine(44_100, 2, 2.0),
        );
        play_through(source, &recorder, TapPoint::BeforeEq);

        let status = recorder.stop().unwrap();
        assert!(!recorder.is_recording());
        assert_eq!(status.error, None);
        assert_eq!(status.dropped_batches, 0);
        let expected = Duration::from_secs(2);
        assert!(
            status.duration.abs_diff(expected) < Duration::from_millis(50),
            "{:?}",
            status.duration
        );
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(status.bytes_written, len);

        let data = std::fs::read(&path).unwrap();
        assert!(data.starts_with(b"ID3\x04"), "file starts with the tag");

        let (rate, channels, frames) = decode_mp3(&path);
        assert_eq!(rate, 44_100);
        assert_eq!(channels, 2);
        // Encoder delay/padding add a little; nothing should be missing.
        assert!((88_200..88_200 + 4 * 1152).contains(&frames), "{frames}");
    }

    #[test]
    fn records_mono_as_mono() {
        let path = temp_path("mono");
        let recorder = Recorder::new();
        recorder.start(options(&path, TapPoint::BeforeEq)).unwrap();
        let source = SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(48_000).unwrap(),
            sine(48_000, 1, 1.0),
        );
        play_through(source, &recorder, TapPoint::BeforeEq);
        recorder.stop().unwrap();

        let (rate, channels, _) = decode_mp3(&path);
        assert_eq!(rate, 48_000);
        assert_eq!(channels, 1);
    }

    #[test]
    fn surround_keeps_front_pair() {
        let path = temp_path("surround");
        let recorder = Recorder::new();
        recorder.start(options(&path, TapPoint::BeforeEq)).unwrap();
        let source = SamplesBuffer::new(
            NonZero::new(6).unwrap(),
            NonZero::new(48_000).unwrap(),
            sine(48_000, 6, 1.0),
        );
        play_through(source, &recorder, TapPoint::BeforeEq);
        let status = recorder.stop().unwrap();
        assert_eq!(status.error, None);

        let (_, channels, _) = decode_mp3(&path);
        assert_eq!(channels, 2);
    }

    #[test]
    fn only_the_chosen_tap_records() {
        let path = temp_path("tap-choice");
        let recorder = Recorder::new();
        recorder.start(options(&path, TapPoint::AfterEq)).unwrap();

        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44_100).unwrap(),
            sine(44_100, 2, 1.0),
        );
        play_through(source, &recorder, TapPoint::BeforeEq);
        let status = recorder.stop().unwrap();
        assert_eq!(status.duration, Duration::ZERO);
    }

    #[test]
    fn sample_rate_change_continues_in_same_file() {
        let path = temp_path("rate-change");
        let recorder = Recorder::new();
        recorder.start(options(&path, TapPoint::BeforeEq)).unwrap();
        for rate in [22_050, 44_100] {
            let source = SamplesBuffer::new(
                NonZero::new(2).unwrap(),
                NonZero::new(rate).unwrap(),
                sine(rate, 2, 1.0),
            );
            play_through(source, &recorder, TapPoint::BeforeEq);
        }
        let status = recorder.stop().unwrap();
        assert_eq!(status.error, None);
        assert!(status.duration.abs_diff(Duration::from_secs(2)) < Duration::from_millis(50));
    }

    #[test]
    fn tap_passes_audio_through_unchanged() {
        let recorder = Recorder::new();
        let samples = sine(8_000, 2, 0.5);
        let source = SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(8_000).unwrap(),
            samples.clone(),
        );
        let out: Vec<f32> = RecordingTap::new(source, recorder, TapPoint::BeforeEq).collect();
        assert_eq!(out, samples);
    }

    #[test]
    fn start_refuses_existing_file_and_second_recording() {
        let path = temp_path("exists");
        std::fs::write(&path, b"keep me").unwrap();
        let recorder = Recorder::new();
        assert!(recorder.start(options(&path, TapPoint::BeforeEq)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");

        let other = path.with_file_name("other.mp3");
        recorder.start(options(&other, TapPoint::BeforeEq)).unwrap();
        let third = path.with_file_name("third.mp3");
        assert!(recorder.start(options(&third, TapPoint::BeforeEq)).is_err());
        recorder.stop().unwrap();
        assert!(recorder.stop().is_none());
    }

    #[test]
    fn missing_folder_is_an_error() {
        let path = temp_path("missing").with_file_name("no-such-dir/test.mp3");
        let recorder = Recorder::new();
        assert!(recorder.start(options(&path, TapPoint::BeforeEq)).is_err());
        assert!(!recorder.is_recording());
    }

    #[test]
    fn id3_tag_layout() {
        let tag = id3v2_tag(&RecordingTags {
            title: "Ράδιο".to_string(),
            artist: "A".to_string(),
            album: String::new(),
            comment: "http://x".to_string(),
            cover: Some(b"\x89PNG....".to_vec()),
        });
        assert_eq!(&tag[..6], b"ID3\x04\x00\x00");
        let size = tag[6..10]
            .iter()
            .fold(0u32, |acc, b| (acc << 7) | *b as u32);
        assert_eq!(size as usize, tag.len() - 10);
        // First frame is the title, UTF-8 encoded.
        assert_eq!(&tag[10..14], b"TIT2");
        assert_eq!(tag[20], 3);
        assert_eq!(&tag[21..21 + "Ράδιο".len()], "Ράδιο".as_bytes());
        assert!(tag.windows(4).any(|w| w == b"COMM"));
        assert!(tag.windows(9).any(|w| w == b"image/png"));
        assert!(!tag.windows(4).any(|w| w == b"TALB"));
    }

    #[test]
    fn empty_tags_write_no_tag() {
        assert!(id3v2_tag(&RecordingTags::default()).is_empty());
    }

    #[test]
    fn synchsafe_encoding() {
        assert_eq!(synchsafe(0), [0, 0, 0, 0]);
        assert_eq!(synchsafe(127), [0, 0, 0, 127]);
        assert_eq!(synchsafe(128), [0, 0, 1, 0]);
        assert_eq!(synchsafe(0x0fff_ffff), [0x7f, 0x7f, 0x7f, 0x7f]);
    }
}
