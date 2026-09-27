//! Station recording
//!
//! Records the decoded audio of the playing station to an MP3, Opus or WAV
//! file.
//!
//! Two [`RecordingTap`] sources sit in the playback chain, one before and one
//! after the equalizer. Both pass audio through untouched; while a recording
//! is running, the tap chosen by [`TapPoint`] also copies the samples to a
//! writer thread, which encodes them and writes the file. Volume is
//! applied later by the output sink, so it never affects the recording.
//!
//! The taps never block playback: batches are handed over with `try_send`,
//! and if the writer falls behind, batches are dropped and counted.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::mem;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use rodio::Source;

use crate::error::RadioError;

mod mp3;
mod opus;
mod resample;
mod wav;

#[cfg(test)]
pub(crate) use opus::encode_ogg_opus;

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

/// File format of a recording
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordingFormat {
    /// MP3, 192 kbps: plays everywhere
    #[default]
    Mp3,
    /// Opus in Ogg, 128 kbps: better sound for the size
    Opus,
    /// 16-bit PCM WAV: uncompressed, large
    Wav,
}

impl RecordingFormat {
    /// File name extension, without the dot
    pub fn extension(self) -> &'static str {
        match self {
            RecordingFormat::Mp3 => "mp3",
            RecordingFormat::Opus => "opus",
            RecordingFormat::Wav => "wav",
        }
    }
}

/// Tag fields stored in the file (ID3, Opus comments or WAV INFO)
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
    pub format: RecordingFormat,
    /// Bitrate for MP3 and Opus, in kbps (clamped to what each supports);
    /// WAV ignores it
    pub bitrate_kbps: u32,
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
    handle: Option<JoinHandle<()>>,
    stats: Arc<WriterStats>,
}

/// How the writer thread ended
#[derive(Debug, PartialEq)]
enum Finished {
    Done,
    /// It panicked (debug builds; release builds abort)
    Crashed,
    /// Still writing at the deadline: left to finish on its own
    Stuck,
}

impl Finished {
    fn problem(&self) -> Option<String> {
        match self {
            Finished::Done => None,
            Finished::Crashed => {
                Some("the recording stopped unexpectedly, the file may be incomplete".into())
            }
            Finished::Stuck => {
                Some("the drive stopped responding, the file may be incomplete".into())
            }
        }
    }
}

impl ActiveRecording {
    /// Ask the writer to finish the file, and wait for it until `deadline`
    fn finish(&mut self, deadline: Instant) -> Finished {
        // The writer may have exited on an error, or be stuck writing
        let _ = self.tx.send_deadline(WriterMsg::Finish, deadline);
        let Some(handle) = self.handle.take() else {
            return Finished::Done;
        };
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                // Dropping the handle detaches the thread
                return Finished::Stuck;
            }
            thread::sleep(Duration::from_millis(10));
        }
        match handle.join() {
            Ok(()) => Finished::Done,
            Err(_) => Finished::Crashed,
        }
    }
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
        if self.lock_active().is_some() {
            return Err(RadioError::Audio("Already recording".to_string()));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&options.path)
            .map_err(|e| {
                RadioError::Audio(format!("Cannot create {}: {e}", options.path.display()))
            })?;
        self.start_writing(options, file)
    }

    /// Start recording into `file` (created for `options.path`)
    fn start_writing(
        &self,
        options: RecordingOptions,
        file: impl RecordingFile + Send + 'static,
    ) -> Result<(), RadioError> {
        let mut active = self.lock_active();
        if active.is_some() {
            return Err(RadioError::Audio("Already recording".to_string()));
        }

        let (tx, rx) = bounded(QUEUE_BATCHES);
        let stats = Arc::new(WriterStats::default());
        let (format, tags, kbps) = (options.format, options.tags, options.bitrate_kbps);
        let writer_stats = stats.clone();
        let handle = thread::Builder::new()
            .name("recording-writer".to_string())
            .spawn(move || writer_loop(file, new_encoder(format, tags, kbps), rx, writer_stats))
            .map_err(|e| RadioError::Audio(format!("Failed to spawn recording thread: {e}")))?;

        let generation = self.shared.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        *active = Some(ActiveRecording {
            generation,
            path: options.path,
            tx,
            handle: Some(handle),
            stats,
        });
        self.shared.tap.store(options.tap.id(), Ordering::SeqCst);
        self.shared.generation.store(generation, Ordering::SeqCst);
        Ok(())
    }

    /// Stop recording, finish the file, and return its final status.
    ///
    /// Returns `None` if nothing was being recorded. Waits at most
    /// [`STOP_TIMEOUT`] for the file to be finished: a drive that stopped
    /// answering (an unplugged USB drive, a dropped network share) can't
    /// hold up the caller. The status then says the file may be incomplete.
    pub fn stop(&self) -> Option<RecordingStatus> {
        self.stop_within(STOP_TIMEOUT)
    }

    fn stop_within(&self, timeout: Duration) -> Option<RecordingStatus> {
        let mut active = {
            let mut guard = self.lock_active();
            self.shared.generation.store(0, Ordering::SeqCst);
            self.shared.tap.store(0, Ordering::SeqCst);
            guard.take()?
        };
        let finished = active.finish(Instant::now() + timeout);
        let mut status = status_of(&active.path, &active.stats);
        if status.error.is_none() {
            status.error = finished.problem();
        }
        Some(status)
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
        if let Some(mut active) = active {
            active.finish(Instant::now() + STOP_TIMEOUT);
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
    /// Position of the next sample within its frame (0 = frame start)
    frame_pos: u16,
    /// Channel count `frame_pos` counts in
    frame_channels: u16,
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
            frame_pos: 0,
            frame_channels: 0,
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
        // Read after next(): the format of the sample just returned.
        let channels = self.inner.channels().get();
        if channels != self.frame_channels {
            // Formats change between packets, so this sample starts a frame
            self.frame_channels = channels;
            self.frame_pos = 0;
        }
        let frame_start = self.frame_pos == 0;
        self.frame_pos = (self.frame_pos + 1) % channels;

        self.until_check = self.until_check.saturating_sub(1);
        // Start or stop only at a frame start: a recording that began
        // mid-frame would take the wrong channels as left and right
        if self.until_check == 0 && frame_start {
            self.until_check = STATE_CHECK_INTERVAL;
            let generation = self.recorder.generation_for(self.point);
            if generation != self.generation {
                // Started, stopped or restarted: don't carry audio across.
                self.batch.clear();
                self.generation = generation;
            }
        }

        if self.generation != 0 {
            let rate = self.inner.sample_rate().get();
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
        // rodio can stop a source mid-frame: keep whole frames only
        let channels = usize::from(self.batch_channels.max(1));
        self.batch
            .truncate(self.batch.len() - self.batch.len() % channels);
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

/// Turns interleaved audio into the bytes of one file format.
///
/// Encoders get mono or stereo samples within ±1.0. The rate and channel
/// count can change between calls (a station switching streams); each
/// format deals with that in its own way.
trait AudioEncoder {
    /// Encode `samples`, appending file bytes to `out`.
    fn encode(
        &mut self,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
        out: &mut Vec<u8>,
    ) -> Result<(), String>;

    /// Write whatever is still buffered; called once at the end.
    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), String>;

    /// Bytes to overwrite at file offsets once everything is written,
    /// e.g. sizes in a header.
    fn header_patches(&self) -> Vec<(u64, Vec<u8>)> {
        Vec::new()
    }
}

fn new_encoder(format: RecordingFormat, tags: RecordingTags, kbps: u32) -> Box<dyn AudioEncoder> {
    match format {
        RecordingFormat::Mp3 => Box::new(mp3::Mp3Encoder::new(tags, kbps)),
        RecordingFormat::Opus => Box::new(opus::OpusEncoder::new(tags, kbps)),
        RecordingFormat::Wav => Box::new(wav::WavEncoder::new(tags)),
    }
}

/// Mix interleaved audio from `from` channels to `to` (1 or 2).
fn convert_channels(samples: &[f32], from: u16, to: u16) -> Vec<f32> {
    let from = from.max(1) as usize;
    let frames = samples.chunks_exact(from);
    match (from, to) {
        (1, _) => frames.flat_map(|f| [f[0], f[0]]).collect(),
        (_, 1) => frames.map(|f| (f[0] + f[1]) * 0.5).collect(),
        // Keep front left/right of surround streams
        _ => frames.flat_map(|f| [f[0], f[1]]).collect(),
    }
}

fn writer_loop<F: RecordingFile>(
    file: F,
    encoder: Box<dyn AudioEncoder>,
    rx: Receiver<WriterMsg>,
    stats: Arc<WriterStats>,
) {
    if let Err(e) = write_recording(file, encoder, &rx, &stats) {
        *stats.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
        stats.failed.store(true, Ordering::SeqCst);
    }
}

/// The file a recording is written to. A trait so tests can make writes fail.
trait RecordingFile: Write + Seek {
    fn sync_all(&self) -> std::io::Result<()>;
}

impl RecordingFile for File {
    fn sync_all(&self) -> std::io::Result<()> {
        File::sync_all(self)
    }
}

fn write_recording<F: RecordingFile>(
    file: F,
    mut encoder: Box<dyn AudioEncoder>,
    rx: &Receiver<WriterMsg>,
    stats: &WriterStats,
) -> Result<(), String> {
    let io_err = |e: std::io::Error| format!("Writing the recording failed: {e}");
    let mut out = BufWriter::with_capacity(64 * 1024, file);
    let mut bytes = Vec::new();
    let write = |out: &mut BufWriter<F>, bytes: &mut Vec<u8>| -> Result<(), String> {
        out.write_all(bytes).map_err(io_err)?;
        stats
            .bytes_written
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        bytes.clear();
        Ok(())
    };

    // Run until stopped or an error; either way, finish the file after so
    // what was recorded stays playable.
    let mut last_flush = Instant::now();
    let mut result = (|| loop {
        // Flush at least every FLUSH_INTERVAL, also while audio keeps
        // arriving, so a crash loses little
        if last_flush.elapsed() >= FLUSH_INTERVAL {
            out.flush().map_err(io_err)?;
            last_flush = Instant::now();
        }
        match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok(WriterMsg::Pcm {
                sample_rate,
                channels,
                mut samples,
            }) => {
                if sample_rate == 0 || channels == 0 {
                    continue;
                }
                // Whole frames only: a stray sample would swap left and
                // right for the rest of the file
                samples.truncate(samples.len() - samples.len() % channels as usize);
                for s in samples.iter_mut() {
                    // The EQ has no limiter; clip like the sound card would.
                    *s = if s.is_finite() {
                        s.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                }
                let frames = (samples.len() / channels as usize) as u64;
                let (samples, channels) = if channels > 2 {
                    (convert_channels(&samples, channels, 2), 2)
                } else {
                    (samples, channels)
                };
                let encoded = encoder.encode(sample_rate, channels, &samples, &mut bytes);
                write(&mut out, &mut bytes)?;
                encoded?;
                stats
                    .duration_micros
                    .fetch_add(frames * 1_000_000 / sample_rate as u64, Ordering::Relaxed);
            }
            Ok(WriterMsg::Finish) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
        }
    })();

    let finished = encoder.finish(&mut bytes);
    let written = write(&mut out, &mut bytes);
    result = result.and(finished).and(written);

    // Patch the header even when the last flush fails (a full disk): the
    // sizes are a few bytes at the start of the file, and without them a
    // WAV reads as empty
    let mut file = match out.into_inner() {
        Ok(file) => file,
        Err(e) => {
            let (error, out) = e.into_parts();
            result = result.and(Err(io_err(error)));
            out.into_parts().0
        }
    };
    let patched = (|| {
        for (offset, patch) in encoder.header_patches() {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&patch)?;
        }
        file.sync_all()
    })()
    .map_err(io_err);
    result.and(patched)
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
            format: RecordingFormat::Mp3,
            bitrate_kbps: 192,
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
        decode(path, "mp3")
    }

    /// Decode a recording with the engine's decoder, as the app would play
    /// it; returns (sample_rate, channels, frames).
    fn decode(path: &Path, hint: &str) -> (u32, usize, usize) {
        let data = std::fs::read(path).unwrap();
        let mut source = SymphoniaSource::new_with_hint(Cursor::new(data), Some(hint))
            .expect("recording should probe");
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

    /// Record `seconds` of a sine in `format` and return the file and status.
    fn record(
        name: &str,
        format: RecordingFormat,
        parts: &[(u32, u16, f32)],
    ) -> (PathBuf, RecordingStatus) {
        let path = temp_path(name).with_extension(format.extension());
        let recorder = Recorder::new();
        let mut opts = options(&path, TapPoint::BeforeEq);
        opts.format = format;
        opts.tags.cover = Some(b"\x89PNG fake".to_vec());
        recorder.start(opts).unwrap();
        for &(rate, channels, seconds) in parts {
            let source = SamplesBuffer::new(
                NonZero::new(channels).unwrap(),
                NonZero::new(rate).unwrap(),
                sine(rate, channels, seconds),
            );
            play_through(source, &recorder, TapPoint::BeforeEq);
        }
        let status = recorder.stop().unwrap();
        assert_eq!(status.error, None);
        assert_eq!(
            status.bytes_written,
            std::fs::metadata(&path).unwrap().len()
        );
        (path, status)
    }

    #[test]
    fn records_opus_resampled_to_48k() {
        let (path, _) = record("opus", RecordingFormat::Opus, &[(44_100, 2, 2.0)]);
        let data = std::fs::read(&path).unwrap();
        assert!(data.starts_with(b"OggS"));
        assert!(data.windows(8).any(|w| w == b"OpusHead"));
        assert!(data.windows(6).any(|w| w == b"TITLE="));
        assert!(data.windows(23).any(|w| w == b"METADATA_BLOCK_PICTURE="));

        let (rate, channels, frames) = decode(&path, "opus");
        assert_eq!(rate, 48_000);
        assert_eq!(channels, 2);
        // Two seconds at 48 kHz; the decoder may keep the pre-skip.
        assert!(
            (96_000 - 960..96_000 + 2 * 960).contains(&frames),
            "{frames}"
        );
    }

    #[test]
    fn opus_follows_rate_and_channel_changes() {
        let (path, status) = record(
            "opus-change",
            RecordingFormat::Opus,
            &[(48_000, 1, 1.0), (22_050, 2, 1.0)],
        );
        assert!(status.duration.abs_diff(Duration::from_secs(2)) < Duration::from_millis(50));
        let (rate, channels, frames) = decode(&path, "opus");
        assert_eq!(
            (rate, channels),
            (48_000, 1),
            "keeps the first channel count"
        );
        assert!(
            (96_000 - 960..96_000 + 2 * 960).contains(&frames),
            "{frames}"
        );
    }

    #[test]
    fn records_wav_with_exact_length() {
        let (path, _) = record("wav", RecordingFormat::Wav, &[(44_100, 2, 1.5)]);
        let (rate, channels, frames) = decode(&path, "wav");
        assert_eq!((rate, channels, frames), (44_100, 2, 66_150));
        let data = std::fs::read(&path).unwrap();
        let riff = u32::from_le_bytes(data[4..8].try_into().unwrap());
        assert_eq!(riff as usize, data.len() - 8);
    }

    #[test]
    fn wav_resamples_a_rate_change_into_the_first_rate() {
        let (path, _) = record(
            "wav-change",
            RecordingFormat::Wav,
            &[(44_100, 2, 1.0), (48_000, 6, 1.0)],
        );
        let (rate, channels, frames) = decode(&path, "wav");
        assert_eq!((rate, channels), (44_100, 2));
        assert_eq!(frames, 88_200);
    }

    #[test]
    fn empty_recordings_are_still_valid_files() {
        for format in [
            RecordingFormat::Mp3,
            RecordingFormat::Opus,
            RecordingFormat::Wav,
        ] {
            let path = temp_path(&format!("empty-{format:?}")).with_extension(format.extension());
            let recorder = Recorder::new();
            let mut opts = options(&path, TapPoint::BeforeEq);
            opts.format = format;
            recorder.start(opts).unwrap();
            let status = recorder.stop().unwrap();
            assert_eq!(status.error, None, "{format:?}");
        }
    }

    #[test]
    fn bitrate_setting_changes_file_size() {
        for format in [RecordingFormat::Mp3, RecordingFormat::Opus] {
            let size = |kbps: u32| {
                let path = temp_path(&format!("kbps-{format:?}-{kbps}"))
                    .with_extension(format.extension());
                let recorder = Recorder::new();
                let mut opts = options(&path, TapPoint::BeforeEq);
                opts.format = format;
                opts.bitrate_kbps = kbps;
                recorder.start(opts).unwrap();
                let source = SamplesBuffer::new(
                    NonZero::new(2).unwrap(),
                    NonZero::new(48_000).unwrap(),
                    sine(48_000, 2, 3.0),
                );
                play_through(source, &recorder, TapPoint::BeforeEq);
                recorder.stop().unwrap().bytes_written as f64
            };
            let (low, high) = (size(96), size(256));
            // CBR MP3 is exact; Opus VBR spends less on a plain sine.
            assert!(high > low * 1.5, "{format:?}: {low} vs {high}");
        }
    }

    #[test]
    fn channel_conversion() {
        assert_eq!(convert_channels(&[0.5, 0.25], 1, 2), [0.5, 0.5, 0.25, 0.25]);
        assert_eq!(convert_channels(&[0.5, 0.25], 2, 1), [0.375]);
        assert_eq!(convert_channels(&[1., 2., 3., 4., 5., 6.], 6, 2), [1., 2.]);
    }

    // --- Robustness ---

    /// In-memory file that fails writes past `limit` bytes, like a full
    /// disk; writes below it (header patches) still work
    #[derive(Clone)]
    struct FullDisk {
        data: Arc<Mutex<Vec<u8>>>,
        pos: u64,
        limit: usize,
    }

    impl Write for FullDisk {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut data = self.data.lock().unwrap();
            let pos = self.pos as usize;
            if pos + buf.len() > self.limit {
                return Err(std::io::Error::other("No space left on device"));
            }
            if data.len() < pos + buf.len() {
                data.resize(pos + buf.len(), 0);
            }
            data[pos..pos + buf.len()].copy_from_slice(buf);
            self.pos += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for FullDisk {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            if let SeekFrom::Start(p) = pos {
                self.pos = p;
            }
            Ok(self.pos)
        }
    }

    impl RecordingFile for FullDisk {
        fn sync_all(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn full_disk(limit: usize) -> FullDisk {
        FullDisk {
            data: Arc::default(),
            pos: 0,
            limit,
        }
    }

    fn pcm(samples: Vec<f32>) -> WriterMsg {
        WriterMsg::Pcm {
            sample_rate: 44_100,
            channels: 2,
            samples,
        }
    }

    #[test]
    fn full_disk_still_gets_a_wav_header() {
        let disk = full_disk(100_000);
        let (tx, rx) = bounded(QUEUE_BATCHES);
        // About 1 s of stereo 16-bit: more than fits
        for chunk in sine(44_100, 2, 1.0).chunks(4096) {
            tx.send(pcm(chunk.to_vec())).unwrap();
        }
        tx.send(WriterMsg::Finish).unwrap();
        let encoder = new_encoder(RecordingFormat::Wav, RecordingTags::default(), 0);
        let stats = WriterStats::default();
        let result = write_recording(disk.clone(), encoder, &rx, &stats);
        assert!(result.is_err(), "the full disk is reported");

        let data = disk.data.lock().unwrap();
        assert_eq!(&data[..4], b"RIFF");
        let riff_size = u32::from_le_bytes(data[4..8].try_into().unwrap());
        assert!(riff_size > 0, "header sizes never written: reads as empty");
    }

    #[test]
    fn flushes_while_audio_keeps_arriving() {
        let disk = full_disk(usize::MAX);
        let (tx, rx) = bounded(QUEUE_BATCHES);
        let writer = {
            let disk = disk.clone();
            thread::spawn(move || {
                let encoder = new_encoder(RecordingFormat::Wav, RecordingTags::default(), 0);
                write_recording(disk, encoder, &rx, &WriterStats::default())
            })
        };
        // Small batches, faster than the flush interval: never a timeout,
        // and never enough to fill the 64 KiB write buffer
        for _ in 0..8 {
            tx.send(pcm(vec![0.1; 1000])).unwrap();
            thread::sleep(Duration::from_millis(200));
        }
        let on_disk = disk.data.lock().unwrap().len();
        tx.send(WriterMsg::Finish).unwrap();
        writer.join().unwrap().unwrap();
        assert!(on_disk > 0, "nothing reached the file after 1.6 s of audio");
    }

    /// Decode a WAV recording to interleaved samples
    fn decode_samples(path: &Path) -> (usize, Vec<f32>) {
        let data = std::fs::read(path).unwrap();
        let source = SymphoniaSource::new_with_hint(Cursor::new(data), Some("wav")).unwrap();
        let channels = source.channels().get() as usize;
        (channels, source.collect())
    }

    fn wav_options(path: &Path) -> RecordingOptions {
        RecordingOptions {
            path: path.with_extension("wav"),
            format: RecordingFormat::Wav,
            ..options(path, TapPoint::BeforeEq)
        }
    }

    #[test]
    fn surround_recording_started_mid_stream_keeps_the_front_pair() {
        let path = temp_path("surround-late").with_extension("wav");
        let recorder = Recorder::new();
        // 5.1: front left carries the tone, every other channel is silent
        let frames = 44_100;
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * 0.05).sin() * 0.5;
                [v, 0.0, 0.0, 0.0, 0.0, 0.0]
            })
            .collect();
        let source = SamplesBuffer::new(
            NonZero::new(6).unwrap(),
            NonZero::new(44_100).unwrap(),
            samples,
        );
        let mut tap = RecordingTap::new(source, recorder.clone(), TapPoint::BeforeEq);
        // Play a while before recording starts; the tap looks at the
        // recorder every 1024 samples, which is not a multiple of 6
        for _ in 0..2000 {
            tap.next();
        }
        recorder.start(wav_options(&path)).unwrap();
        for _ in tap {}
        recorder.stop().unwrap();

        let (channels, out) = decode_samples(&path);
        assert_eq!(channels, 2);
        let peak = |ch: usize| {
            out.iter()
                .skip(ch)
                .step_by(2)
                .fold(0.0f32, |m, v| m.max(v.abs()))
        };
        assert!(peak(0) > 0.4, "left lost the front-left channel");
        assert!(peak(1) < 0.01, "right picked up another channel");
    }

    #[test]
    fn source_stopped_mid_frame_does_not_swap_channels() {
        let path = temp_path("mid-frame").with_extension("wav");
        let recorder = Recorder::new();
        recorder.start(wav_options(&path)).unwrap();
        // Tone on the left, silence on the right
        let stereo = |frames: usize| -> Vec<f32> {
            (0..frames)
                .flat_map(|i| [(i as f32 * 0.05).sin() * 0.5, 0.0])
                .collect()
        };
        let source = |samples| {
            SamplesBuffer::new(
                NonZero::new(2).unwrap(),
                NonZero::new(44_100).unwrap(),
                samples,
            )
        };
        // rodio stops a source after an odd number of samples
        let mut first =
            RecordingTap::new(source(stereo(10_000)), recorder.clone(), TapPoint::BeforeEq);
        for _ in 0..5_001 {
            first.next();
        }
        drop(first);
        // The next source goes on in the same recording
        play_through(source(stereo(10_000)), &recorder, TapPoint::BeforeEq);
        recorder.stop().unwrap();

        let (channels, out) = decode_samples(&path);
        assert_eq!(channels, 2);
        let right_peak = out
            .iter()
            .skip(1)
            .step_by(2)
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(
            right_peak < 0.01,
            "left and right swapped: right peaks at {right_peak}"
        );
    }

    /// A file on a drive that stopped answering (an unplugged USB drive, a
    /// dropped network share): every write blocks until `released` closes
    struct StuckDisk {
        released: Receiver<()>,
    }

    impl Write for StuckDisk {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.released.recv();
            Err(std::io::Error::other("the drive is gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            let _ = self.released.recv();
            Ok(())
        }
    }

    impl Seek for StuckDisk {
        fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    impl RecordingFile for StuckDisk {
        fn sync_all(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A file whose writes panic, like a bug in an encoder
    struct PanickingDisk;

    impl Write for PanickingDisk {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            panic!("a bug while writing");
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for PanickingDisk {
        fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    impl RecordingFile for PanickingDisk {
        fn sync_all(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn half_a_second() -> SamplesBuffer {
        SamplesBuffer::new(
            NonZero::new(2).unwrap(),
            NonZero::new(44_100).unwrap(),
            sine(44_100, 2, 0.5),
        )
    }

    #[test]
    fn stop_gives_up_on_a_drive_that_stopped_answering() {
        let (release, released) = bounded::<()>(0);
        let recorder = Recorder::new();
        recorder
            .start_writing(wav_options(Path::new("stuck")), StuckDisk { released })
            .unwrap();
        play_through(half_a_second(), &recorder, TapPoint::BeforeEq);

        let start = Instant::now();
        let status = recorder
            .stop_within(Duration::from_millis(200))
            .expect("a recording was running");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "stop waited {:?} for a stuck drive",
            start.elapsed()
        );
        let error = status.error.expect("the unfinished file is reported");
        assert!(error.contains("stopped responding"), "{error}");
        assert!(!recorder.is_recording());

        // The stuck writer doesn't block the next recording
        recorder
            .start_writing(wav_options(Path::new("next")), full_disk(usize::MAX))
            .unwrap();
        play_through(half_a_second(), &recorder, TapPoint::BeforeEq);
        let status = recorder.stop().unwrap();
        assert_eq!(status.error, None);
        assert!(status.bytes_written > 0);
        drop(release);
    }

    #[test]
    fn a_crashed_writer_is_not_reported_as_saved() {
        let recorder = Recorder::new();
        recorder
            .start_writing(wav_options(Path::new("crash")), PanickingDisk)
            .unwrap();
        play_through(half_a_second(), &recorder, TapPoint::BeforeEq);

        let status = recorder.stop().expect("a recording was running");
        let error = status.error.expect("the crash is reported");
        assert!(error.contains("stopped unexpectedly"), "{error}");
    }
}
