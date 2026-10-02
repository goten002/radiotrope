//! One station check: resolve, open, listen, and judge
//!
//! The work runs on a thread of its own and reports each step on a channel;
//! the caller's thread keeps the clock. When the time is up it cancels the
//! stream, which ends every wait in the engine, so a check always returns
//! within its timeout.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use radiotrope::audio::decoder::start_open;
use radiotrope::audio::{CodecInfo, DecoderStats};
use radiotrope::error::RadioError;
use radiotrope::stream::buffer::PlaybackPositionReader;
use radiotrope::stream::{
    MetadataSource, StreamCancel, StreamInfo, StreamMetadata, StreamResolver, StreamType,
};

use crate::classify::classify;
use crate::level::{Levels, Player};
use crate::report::{
    Audio, ErrorCode, ErrorInfo, Metadata, Problem, ProblemCode, Report, Station, StreamDetails,
};
use crate::time::rfc3339;

/// How a check runs
#[derive(Debug, Clone)]
pub struct Options {
    /// How long to listen once the audio starts
    pub listen: Duration,
    /// The most the whole check may take
    pub timeout: Duration,
    /// Blocks quieter than this (dBFS) are silence
    pub silence_db: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            listen: Duration::from_secs(10),
            timeout: Duration::from_secs(30),
            silence_db: -50.0,
        }
    }
}

/// How often the listen looks at the stream
const TICK: Duration = Duration::from_millis(100);

/// Decode errors above this share of frames make a station degraded
const MAX_DECODE_ERROR_SHARE: f64 = 0.05;

/// Less audio than this share of the time listened is slow
const MIN_REALTIME_RATIO: f64 = 0.9;

/// Samples taken from the decoder at a time
const CHUNK: usize = 4096;

/// What the checking thread reports, in order
enum Step {
    Resolved {
        info: StreamInfo,
        metadata: Option<Receiver<StreamMetadata>>,
        bytes: Option<Arc<AtomicU64>>,
    },
    Opened {
        codec: CodecInfo,
        label: Arc<Mutex<String>>,
        stats: Arc<DecoderStats>,
    },
    /// The decoder ran out of audio: the reason, if it failed
    Ended(Option<String>),
    Failed(RadioError),
}

/// What the checking thread and the listen share
#[derive(Default)]
struct Shared {
    /// Microseconds of audio decoded
    decoded: Arc<AtomicU64>,
    levels: Mutex<Levels>,
    /// Set when the listen is over
    stop: AtomicBool,
}

/// Check the station at `url`
pub fn check(url: &str, options: &Options) -> Report {
    let started = Instant::now();
    let mut report = Report::new(url, rfc3339(SystemTime::now()));
    match valid_url(url) {
        Ok(()) => run(url, options, started, &mut report),
        Err(message) => {
            report.error = Some(ErrorInfo {
                code: ErrorCode::InvalidUrl,
                http_status: None,
                message,
            })
        }
    }
    report.timing_ms.total = millis(started.elapsed());
    report.conclude();
    report
}

/// Only http and https addresses are stations
fn valid_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("Not a valid address: {e}"))?;
    match parsed.scheme() {
        "http" | "https" if parsed.host_str().is_some() => Ok(()),
        "http" | "https" => Err("The address has no host".to_string()),
        scheme => Err(format!(
            "Only http and https addresses can be checked, not {scheme}"
        )),
    }
}

fn run(url: &str, options: &Options, started: Instant, report: &mut Report) {
    let deadline = started + options.timeout;
    let cancel = StreamCancel::new();
    let shared = Arc::new(Shared::default());
    let (steps_tx, steps) = crossbeam_channel::unbounded();
    let spawned = {
        let (url, cancel, shared) = (url.to_string(), cancel.clone(), shared.clone());
        thread::Builder::new()
            .name("probe-check".to_string())
            .spawn(move || work(&url, &cancel, &shared, &steps_tx))
    };
    if let Err(e) = spawned {
        report.error = Some(ErrorInfo {
            code: ErrorCode::StreamFailed,
            http_status: None,
            message: format!("Could not start the check: {e}"),
        });
        return;
    }

    let outcome = follow(options, started, deadline, &steps, &shared, report);
    shared.stop.store(true, Ordering::Relaxed);
    cancel.cancel();
    if let Err(error) = outcome {
        report.error = Some(error);
    }
}

/// The checking thread: resolve, open, then decode until told to stop
fn work(url: &str, cancel: &StreamCancel, shared: &Shared, steps: &Sender<Step>) {
    let stream = match StreamResolver::resolve_cancellable(url, cancel) {
        Ok(stream) => stream,
        Err(e) => {
            let _ = steps.send(Step::Failed(e));
            return;
        }
    };
    let format_hint = stream.info.format_hint.clone();
    let _ = steps.send(Step::Resolved {
        info: stream.info,
        metadata: stream.metadata_rx,
        bytes: stream.bytes_received,
    });

    // The readers hold song changes until the decoder reaches them, so the
    // decoder must say how far it has read
    let reader = PlaybackPositionReader::new(
        Patient(stream.reader),
        stream.playback_position.unwrap_or_default(),
    );
    let opened = start_open(reader, format_hint).and_then(|rx| {
        rx.recv()
            .unwrap_or_else(|_| Err(RadioError::Decode("The decoder stopped".to_string())))
    });
    let mut source = match opened {
        Ok(source) => source,
        Err(e) => {
            let _ = steps.send(Step::Failed(e));
            return;
        }
    };
    source.count_decoded_time(shared.decoded.clone());
    let codec = source.codec_info();
    lock(&shared.levels).set_format(codec.sample_rate, codec.channels);
    let errors = source.error_slot();
    let _ = steps.send(Step::Opened {
        codec,
        label: source.codec_label(),
        stats: source.decoder_stats(),
    });

    let mut chunk = Vec::with_capacity(CHUNK);
    loop {
        chunk.clear();
        chunk.extend(source.by_ref().take(CHUNK));
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let ran_out = chunk.len() < CHUNK;
        let mut levels = lock(&shared.levels);
        if levels.push(&chunk) {
            // AAC+ can change rate after the first frames
            let codec = source.codec_info();
            if (codec.sample_rate, codec.channels) != (levels.sample_rate, levels.channels) {
                levels.set_format(codec.sample_rate, codec.channels);
            }
        }
        drop(levels);
        if ran_out {
            let reason = lock(&errors).take();
            let _ = steps.send(Step::Ended(reason));
            return;
        }
    }
}

/// The caller's side: take each step as it comes, then listen
fn follow(
    options: &Options,
    started: Instant,
    deadline: Instant,
    steps: &Receiver<Step>,
    shared: &Shared,
    report: &mut Report,
) -> Result<(), ErrorInfo> {
    // The engine resolves once the first audio bytes are in
    let waited = || format!("No audio within {} s", options.timeout.as_secs());
    let (metadata, bytes) = match next_step(steps, deadline, waited)? {
        Step::Resolved {
            info,
            metadata,
            bytes,
        } => {
            report.timing_ms.resolve = Some(millis(started.elapsed()));
            report.stream = Some(StreamDetails {
                kind: match info.stream_type {
                    StreamType::Direct => "direct",
                    StreamType::Hls => "hls",
                },
                resolved_url: info.resolved_url,
                content_type: info.content_type,
            });
            report.station = Some(Station {
                name: info.station_name,
                advertised_kbps: info.bitrate,
            });
            (metadata, bytes)
        }
        Step::Failed(e) => return Err(classify(&e)),
        _ => return Err(out_of_order()),
    };

    let no_audio = || format!("No audio within {} s", options.timeout.as_secs());
    let (label, stats) = match next_step(steps, deadline, no_audio)? {
        Step::Opened {
            codec,
            label,
            stats,
        } => {
            report.timing_ms.first_audio = Some(millis(started.elapsed()));
            report.audio = Some(Audio {
                codec: codec.codec_name,
                sample_rate: codec.sample_rate,
                channels: codec.channels,
                bits_per_sample: codec.bits_per_sample,
                ..Audio::default()
            });
            (label, stats)
        }
        Step::Failed(e) => return Err(classify(&e)),
        _ => return Err(out_of_order()),
    };

    listen(options, deadline, steps, shared, bytes, metadata, report);
    if let Some(audio) = report.audio.as_mut() {
        audio.codec = lock(&label).clone();
        let (frames, errors) = stats.snapshot();
        audio.frames = frames;
        audio.decode_errors = errors;
        if errors > 0 && errors as f64 > (frames + errors) as f64 * MAX_DECODE_ERROR_SHARE {
            report.problems.push(Problem {
                code: ProblemCode::DecodeErrors,
                message: format!(
                    "{errors} of {} frames could not be decoded",
                    frames + errors
                ),
            });
        }
    }
    let heard = report.audio.as_ref().is_some_and(|a| a.decoded_s > 0.0);
    if !heard {
        // Only the problems that say why there's no audio stay
        report.problems.clear();
        return Err(ErrorInfo {
            code: ErrorCode::NoAudio,
            http_status: None,
            message: "The stream opened but no audio came".to_string(),
        });
    }
    Ok(())
}

/// A step that can't come yet (never sent: each step follows the one before)
fn out_of_order() -> ErrorInfo {
    ErrorInfo {
        code: ErrorCode::StreamFailed,
        http_status: None,
        message: "The check went wrong".to_string(),
    }
}

/// The next step, or a timeout error with `message` at the deadline
fn next_step(
    steps: &Receiver<Step>,
    deadline: Instant,
    message: impl Fn() -> String,
) -> Result<Step, ErrorInfo> {
    let timeout = || ErrorInfo {
        code: ErrorCode::Timeout,
        http_status: None,
        message: message(),
    };
    match steps.recv_deadline(deadline) {
        Ok(step) => Ok(step),
        Err(RecvTimeoutError::Timeout) => Err(timeout()),
        Err(RecvTimeoutError::Disconnected) => Err(ErrorInfo {
            code: ErrorCode::StreamFailed,
            http_status: None,
            message: "The check stopped unexpectedly".to_string(),
        }),
    }
}

/// Listen for `options.listen`, or until the deadline or the stream's end
fn listen(
    options: &Options,
    deadline: Instant,
    steps: &Receiver<Step>,
    shared: &Shared,
    bytes: Option<Arc<AtomicU64>>,
    metadata: Option<Receiver<StreamMetadata>>,
    report: &mut Report,
) {
    let start = Instant::now();
    let end = (start + options.listen).min(deadline);
    let bytes_at_start = bytes.as_ref().map(|b| b.load(Ordering::Relaxed));
    let mut player = Player::default();
    let mut songs = Songs::default();
    let mut ended = None;
    let mut last = start;

    while ended.is_none() {
        let now = Instant::now();
        if now >= end {
            break;
        }
        match steps.recv_timeout(TICK.min(end - now)) {
            Ok(Step::Ended(reason)) => ended = Some(reason),
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => ended = Some(None),
        }
        let now = Instant::now();
        player.advance(decoded_secs(shared), (now - last).as_secs_f64());
        last = now;
        if let Some(rx) = &metadata {
            rx.try_iter().for_each(|m| songs.offer(m));
        }
    }
    if let Some(rx) = &metadata {
        rx.try_iter().for_each(|m| songs.offer(m));
    }

    let listened = start.elapsed().as_secs_f64();
    let decoded = decoded_secs(shared);
    let levels = lock(&shared.levels);
    let (silent_s, measured_s) = levels.silence(options.silence_db);
    let Some(audio) = report.audio.as_mut() else {
        return;
    };
    audio.sample_rate = levels.sample_rate;
    audio.channels = levels.channels;
    audio.listened_s = round2(listened);
    audio.decoded_s = round2(decoded);
    audio.realtime_ratio = if listened > 0.0 {
        round2(decoded / listened)
    } else {
        0.0
    };
    audio.measured_kbps = match (bytes, bytes_at_start) {
        (Some(bytes), Some(at_start)) if listened > 0.0 => {
            let got = bytes.load(Ordering::Relaxed).saturating_sub(at_start);
            Some(round1(got as f64 * 8.0 / 1000.0 / listened))
        }
        _ => None,
    };
    audio.stalls = player.stalls;
    audio.stalled_s = round2(player.stalled_secs);
    audio.rms_dbfs = levels.rms_dbfs().map(round1);
    audio.peak_dbfs = levels.peak_dbfs().map(round1);
    audio.silent_s = silent_s;
    drop(levels);

    if measured_s > 0.0 && silent_s >= measured_s {
        report.problems.push(Problem {
            code: ProblemCode::Silent,
            message: format!(
                "No sound above {} dBFS for the whole {} s",
                options.silence_db, measured_s
            ),
        });
    }
    if player.stalls > 0 {
        report.problems.push(Problem {
            code: ProblemCode::Stalled,
            message: format!(
                "Audio came slower than real time: a player would have stopped {} {} ({:.1} s in all)",
                player.stalls,
                if player.stalls == 1 { "time" } else { "times" },
                player.stalled_secs
            ),
        });
    }
    if listened > 0.0 && decoded / listened < MIN_REALTIME_RATIO && ended.is_none() {
        report.problems.push(Problem {
            code: ProblemCode::Slow,
            message: format!(
                "Only {decoded:.1} s of audio came in {listened:.1} s ({:.0} % of real time)",
                decoded / listened * 100.0
            ),
        });
    }
    if let Some(reason) = ended {
        report.problems.push(Problem {
            code: ProblemCode::Ended,
            message: match reason {
                Some(reason) => format!("The stream stopped after {decoded:.1} s: {reason}"),
                None => format!("The stream ended after {decoded:.1} s of audio"),
            },
        });
    }
    report.metadata = songs.into_metadata();
}

/// Song info seen during the listen
#[derive(Default)]
struct Songs {
    latest: Option<StreamMetadata>,
    changes: u32,
}

impl Songs {
    fn offer(&mut self, meta: StreamMetadata) {
        if meta.is_empty() {
            return;
        }
        let same = self
            .latest
            .as_ref()
            .is_some_and(|l| l.title == meta.title && l.artist == meta.artist);
        if !same {
            if self.latest.is_some() {
                self.changes += 1;
            }
            self.latest = Some(meta);
        }
    }

    fn into_metadata(self) -> Option<Metadata> {
        let meta = self.latest?;
        Some(Metadata {
            title: meta.title,
            artist: meta.artist,
            source: match meta.source {
                MetadataSource::Icy => "icy",
                MetadataSource::Id3v2 => "id3v2",
                MetadataSource::Id3v1 => "id3v1",
                MetadataSource::HlsPlaylist => "hls_playlist",
            },
            changes: self.changes,
        })
    }
}

/// The stream's readers return `Interrupted` while they wait for data, so
/// that a caller can look at a stop of its own; this one just waits on.
/// Cancelling the stream ends their waits with an error.
struct Patient<R>(R);

impl<R: Read> Read for Patient<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }
}

impl<R: Seek> Seek for Patient<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

fn decoded_secs(shared: &Shared) -> f64 {
    shared.decoded.load(Ordering::Relaxed) as f64 / 1_000_000.0
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn millis(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_addresses_are_checked() {
        assert!(valid_url("http://radio.example/live").is_ok());
        assert!(valid_url("https://radio.example:8443/a.pls").is_ok());
        assert!(valid_url("ftp://radio.example/live")
            .unwrap_err()
            .contains("ftp"));
        assert!(valid_url("radio.example/live").is_err());
        assert!(valid_url("").is_err());
    }

    #[test]
    fn a_bad_address_is_down_at_once() {
        let started = Instant::now();
        let report = check("not a url", &Options::default());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(report.status, crate::Status::Down);
        assert_eq!(report.error.unwrap().code, ErrorCode::InvalidUrl);
        assert!(report.stream.is_none());
    }

    fn meta(title: &str) -> StreamMetadata {
        StreamMetadata::new(Some(title.to_string()), None, MetadataSource::Icy)
    }

    #[test]
    fn song_changes_are_counted_once_each() {
        let mut songs = Songs::default();
        songs.offer(meta("One"));
        songs.offer(meta("One"));
        songs.offer(StreamMetadata::new(None, None, MetadataSource::Icy));
        songs.offer(meta("Two"));
        let m = songs.into_metadata().unwrap();
        assert_eq!(m.title.as_deref(), Some("Two"));
        assert_eq!(m.changes, 1);
        assert_eq!(m.source, "icy");
        assert!(Songs::default().into_metadata().is_none());
    }
}
