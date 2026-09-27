//! Decoding ahead of the audio output
//!
//! A station's decoder, EQ, analysis and recording taps run on their own
//! thread and keep a little decoded audio (PCM) queued ahead of the output.
//! The audio callback only takes from that queue: it never waits on the
//! network or the decoder, and when the queue runs dry (the station stalls)
//! it plays silence until audio comes back.
//!
//! The queue holds a few of the output device's buffers
//! ([`decode_ahead`]), so everything decoded is heard that much later: EQ
//! changes, the analysis the visualizer shows and song changes are ahead of
//! the sound by up to that. Volume stays on the output and is instant.

use std::io;
use std::num::NonZero;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use rodio::cpal::BufferSize;
use rodio::{MixerDeviceSink, Source};

use crate::config::audio::{
    DECODE_AHEAD_BUFFERS, DECODE_AHEAD_DEFAULT_MS, DECODE_AHEAD_MAX_MS, DECODE_AHEAD_MIN_MS,
    DECODE_CHUNK_MS, UNDERRUN_SILENCE_MS,
};
use crate::stream::StreamCancel;

/// Chunks the queue can hold: enough for the longest decode-ahead in
/// chunks of the shortest usual length
const QUEUE_CHUNKS: usize = 64;

/// How often a decoder that is far enough ahead checks for room
const ROOM_POLL: Duration = Duration::from_millis(5);

/// How far ahead to decode for `output`: a few of its buffers, since the
/// audio callback takes one buffer's worth at a time
pub(crate) fn decode_ahead(output: Option<&MixerDeviceSink>) -> Duration {
    ahead_of_buffer(output.map(|o| (*o.config().buffer_size(), o.config().sample_rate())))
}

/// [`decode_ahead`] for a device buffer of this size at this rate
fn ahead_of_buffer(buffer: Option<(BufferSize, NonZero<u32>)>) -> Duration {
    let min = Duration::from_millis(DECODE_AHEAD_MIN_MS);
    let max = Duration::from_millis(DECODE_AHEAD_MAX_MS);
    match buffer {
        Some((BufferSize::Fixed(frames), rate)) => {
            let buffer = Duration::from_secs_f64(f64::from(frames) / f64::from(rate.get()));
            (buffer * DECODE_AHEAD_BUFFERS).clamp(min, max)
        }
        // The backend picks the buffer size: allow for a large one
        _ => Duration::from_millis(DECODE_AHEAD_DEFAULT_MS),
    }
}

/// Decoded audio in one format
struct Chunk {
    channels: NonZero<u16>,
    sample_rate: NonZero<u32>,
    samples: Vec<f32>,
}

impl Chunk {
    fn micros(&self) -> u64 {
        let frames = (self.samples.len() / usize::from(self.channels.get())) as u64;
        frames * 1_000_000 / u64::from(self.sample_rate.get())
    }
}

/// State shared by the decode thread, the outputs and the engine
struct Shared {
    /// Audio in the queue, in microseconds
    queued_us: AtomicU64,
    /// How far ahead to decode, in microseconds
    ahead_us: AtomicU64,
    /// Format of the audio the output last took, for the silence it plays
    /// and for a new output to start with
    format: AtomicU64,
    /// Times the output ran dry after playing audio
    underruns: AtomicU64,
    /// Buffers the decoder allocated; the rest are played ones handed back
    new_buffers: AtomicU64,
}

fn pack(channels: NonZero<u16>, sample_rate: NonZero<u32>) -> u64 {
    (u64::from(channels.get()) << 32) | u64::from(sample_rate.get())
}

fn unpack(format: u64) -> (NonZero<u16>, NonZero<u32>) {
    let channels = NonZero::new((format >> 32) as u16).unwrap_or(NonZero::<u16>::MIN);
    let sample_rate = NonZero::new(format as u32).unwrap_or(NonZero::<u32>::MIN);
    (channels, sample_rate)
}

/// A station being decoded ahead of its output.
///
/// The engine keeps this while the station plays and gives each player an
/// [`PcmFeed::output`]. When the output device is replaced, a new output
/// carries on from the queue. The decode thread ends when the stream ends,
/// when the stream is cancelled, or when the feed and its outputs are gone.
pub(crate) struct PcmFeed {
    chunks: Receiver<Chunk>,
    spent: Sender<Vec<f32>>,
    shared: Arc<Shared>,
}

impl PcmFeed {
    /// Start decoding `source` on its own thread, `ahead` of the output
    pub(crate) fn start(
        source: impl Source + Send + 'static,
        ahead: Duration,
        cancel: StreamCancel,
    ) -> io::Result<Self> {
        let (chunk_tx, chunks) = bounded(QUEUE_CHUNKS);
        // Every buffer in use fits: the queue's, and one each for the
        // decoder and the output
        let (spent, spent_rx) = bounded(QUEUE_CHUNKS + 2);
        let shared = Arc::new(Shared {
            queued_us: AtomicU64::new(0),
            ahead_us: AtomicU64::new(ahead.as_micros() as u64),
            format: AtomicU64::new(pack(source.channels(), source.sample_rate())),
            underruns: AtomicU64::new(0),
            new_buffers: AtomicU64::new(0),
        });
        let decoder = Decoder {
            chunker: Chunker::new(source),
            chunks: chunk_tx,
            spent: spent_rx,
            shared: shared.clone(),
            cancel,
        };
        thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || decoder.run())?;
        Ok(Self {
            chunks,
            spent,
            shared,
        })
    }

    /// A source for a player: plays the queue from where it is
    pub(crate) fn output(&self) -> PcmOutput {
        let (channels, sample_rate) = unpack(self.shared.format.load(Ordering::Relaxed));
        let mut output = PcmOutput {
            chunks: self.chunks.clone(),
            spent: self.spent.clone(),
            shared: self.shared.clone(),
            samples: Vec::new(),
            pos: 0,
            silence_left: 0,
            channels,
            sample_rate,
            played_audio: false,
            dry: false,
        };
        output.advance();
        output
    }

    /// Decode `ahead` of the output from now on (a new output device)
    pub(crate) fn set_ahead(&self, ahead: Duration) {
        self.shared
            .ahead_us
            .store(ahead.as_micros() as u64, Ordering::Relaxed);
    }

    /// Times the output ran out of decoded audio and played silence
    pub(crate) fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }
}

/// The decode thread
struct Decoder<S> {
    chunker: Chunker<S>,
    chunks: Sender<Chunk>,
    /// Buffers the output has played, for reuse
    spent: Receiver<Vec<f32>>,
    shared: Arc<Shared>,
    cancel: StreamCancel,
}

impl<S: Source> Decoder<S> {
    /// Decode until the stream ends, is cancelled or nobody listens.
    /// Dropping the sender at the end ends the output once it has played
    /// the queue.
    fn run(mut self) {
        loop {
            // Far enough ahead: wait for the output to take some
            while self.shared.queued_us.load(Ordering::Relaxed)
                >= self.shared.ahead_us.load(Ordering::Relaxed)
            {
                if !self.cancel.sleep(ROOM_POLL) {
                    return;
                }
            }
            if self.cancel.is_cancelled() {
                return;
            }
            let buffer = self.spent.try_recv().unwrap_or_else(|_| {
                self.shared.new_buffers.fetch_add(1, Ordering::Relaxed);
                Vec::new()
            });
            let Some(chunk) = self.chunker.next_chunk(buffer) else {
                return;
            };
            self.shared
                .queued_us
                .fetch_add(chunk.micros(), Ordering::Relaxed);
            match self.chunks.try_send(chunk) {
                Ok(()) => {}
                // Many short chunks (format changes): wait for room
                Err(TrySendError::Full(chunk)) => {
                    if !self.cancel.send(&self.chunks, chunk) {
                        return;
                    }
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

/// Cuts a source into chunks of one format, [`DECODE_CHUNK_MS`] long or
/// shorter where the format changes or the source ends
struct Chunker<S> {
    source: S,
    /// Format of the span being read
    channels: NonZero<u16>,
    sample_rate: NonZero<u32>,
    /// Samples left in the span being read (None: the format holds to the
    /// end); at 0 the next sample starts a span
    span_left: Option<usize>,
    started: bool,
    ended: bool,
    /// The first sample of a new format, for the next chunk
    carry: Option<f32>,
}

impl<S: Source> Chunker<S> {
    fn new(source: S) -> Self {
        Self {
            channels: source.channels(),
            sample_rate: source.sample_rate(),
            source,
            span_left: None,
            started: false,
            ended: false,
            carry: None,
        }
    }

    /// The next sample, keeping track of the format it is in
    fn pull(&mut self) -> Option<f32> {
        if self.ended {
            return None;
        }
        let starts_span = !self.started || self.span_left == Some(0);
        let Some(sample) = self.source.next() else {
            self.ended = true;
            return None;
        };
        if starts_span {
            // The first sample of a span can change the format, so read it
            // after the sample
            self.started = true;
            self.channels = self.source.channels();
            self.sample_rate = self.source.sample_rate();
            self.span_left = self.source.current_span_len();
        } else if let Some(left) = self.span_left.as_mut() {
            *left = left.saturating_sub(1);
        }
        Some(sample)
    }

    /// The next chunk, in `samples` (a spent buffer, or a new one). None at
    /// the end of the source.
    fn next_chunk(&mut self, mut samples: Vec<f32>) -> Option<Chunk> {
        samples.clear();
        let first = match self.carry.take() {
            Some(sample) => sample,
            None => self.pull()?,
        };
        let (channels, sample_rate) = (self.channels, self.sample_rate);
        let frames = (u64::from(sample_rate.get()) * DECODE_CHUNK_MS / 1000).max(1) as usize;
        let want = frames * usize::from(channels.get());
        samples.reserve(want);
        samples.push(first);
        while samples.len() < want {
            match self.pull() {
                None => break,
                Some(sample) if (self.channels, self.sample_rate) != (channels, sample_rate) => {
                    self.carry = Some(sample);
                    break;
                }
                Some(sample) => samples.push(sample),
            }
        }
        // Whole frames only, so the silence the output may play next
        // doesn't swap the channels of what follows
        let partial = samples.len() % usize::from(channels.get());
        if partial != 0 {
            samples.resize(samples.len() + usize::from(channels.get()) - partial, 0.0);
        }
        Some(Chunk {
            channels,
            sample_rate,
            samples,
        })
    }
}

/// Plays a [`PcmFeed`]'s queue: the source a player gets.
///
/// Runs on the audio thread, so it never waits: when the queue is empty it
/// plays [`UNDERRUN_SILENCE_MS`] of silence in the current format and looks
/// again. It ends once the decoder has ended and the queue is played out.
/// Channels, sample rate and span length always describe the next sample.
pub(crate) struct PcmOutput {
    chunks: Receiver<Chunk>,
    spent: Sender<Vec<f32>>,
    shared: Arc<Shared>,
    /// The chunk playing
    samples: Vec<f32>,
    pos: usize,
    /// Samples of silence left to play
    silence_left: usize,
    channels: NonZero<u16>,
    sample_rate: NonZero<u32>,
    /// Some audio has played (running dry before that isn't an underrun)
    played_audio: bool,
    /// Ran dry and no audio since
    dry: bool,
}

impl PcmOutput {
    /// Line up what plays next: the next chunk, or silence, or the end
    fn advance(&mut self) {
        // Hand the played buffer back for reuse (no freeing on this thread)
        if self.samples.capacity() > 0 {
            let _ = self.spent.try_send(std::mem::take(&mut self.samples));
        }
        self.pos = 0;
        loop {
            match self.chunks.try_recv() {
                Ok(chunk) => {
                    self.shared
                        .queued_us
                        .fetch_sub(chunk.micros(), Ordering::Relaxed);
                    if chunk.samples.is_empty() {
                        continue;
                    }
                    self.channels = chunk.channels;
                    self.sample_rate = chunk.sample_rate;
                    self.shared
                        .format
                        .store(pack(chunk.channels, chunk.sample_rate), Ordering::Relaxed);
                    self.samples = chunk.samples;
                    self.played_audio = true;
                    self.dry = false;
                }
                Err(TryRecvError::Empty) => {
                    if self.played_audio && !self.dry {
                        self.shared.underruns.fetch_add(1, Ordering::Relaxed);
                    }
                    self.dry = true;
                    let frames = (u64::from(self.sample_rate.get()) * UNDERRUN_SILENCE_MS / 1000)
                        .max(1) as usize;
                    self.silence_left = frames * usize::from(self.channels.get());
                }
                // The decoder ended and the queue is played out
                Err(TryRecvError::Disconnected) => {}
            }
            return;
        }
    }
}

impl Iterator for PcmOutput {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = if self.pos < self.samples.len() {
            self.pos += 1;
            self.samples[self.pos - 1]
        } else if self.silence_left > 0 {
            self.silence_left -= 1;
            0.0
        } else {
            return None;
        };
        if self.pos >= self.samples.len() && self.silence_left == 0 {
            self.advance();
        }
        Some(sample)
    }
}

impl Source for PcmOutput {
    fn current_span_len(&self) -> Option<usize> {
        Some(if self.pos < self.samples.len() {
            self.samples.len() - self.pos
        } else {
            self.silence_left
        })
    }

    fn channels(&self) -> NonZero<u16> {
        self.channels
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    /// A source made of spans of the given (channels, rate, samples); sample
    /// values count up from 0
    struct Spans {
        spans: Vec<(u16, u32, usize)>,
        span: usize,
        left: usize,
        next_value: f32,
    }

    impl Spans {
        fn new(spans: &[(u16, u32, usize)]) -> Self {
            Self {
                spans: spans.to_vec(),
                span: 0,
                left: spans[0].2,
                next_value: 0.0,
            }
        }
    }

    impl Iterator for Spans {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            while self.left == 0 {
                self.span += 1;
                self.left = self.spans.get(self.span)?.2;
            }
            self.left -= 1;
            self.next_value += 1.0;
            Some(self.next_value - 1.0)
        }
    }

    impl Source for Spans {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.left)
        }
        fn channels(&self) -> NonZero<u16> {
            let i = self.span.min(self.spans.len() - 1);
            NonZero::new(self.spans[i].0).unwrap()
        }
        fn sample_rate(&self) -> NonZero<u32> {
            let i = self.span.min(self.spans.len() - 1);
            NonZero::new(self.spans[i].1).unwrap()
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    /// A mono source that plays `samples`, then waits until `release` is
    /// raised before it ends (a station that stops sending)
    struct Stalls {
        samples: std::vec::IntoIter<f32>,
        release: Arc<AtomicBool>,
    }

    impl Iterator for Stalls {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            if let Some(sample) = self.samples.next() {
                return Some(sample);
            }
            while !self.release.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            None
        }
    }

    impl Source for Stalls {
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn channels(&self) -> NonZero<u16> {
            NonZero::new(1).unwrap()
        }
        fn sample_rate(&self) -> NonZero<u32> {
            NonZero::new(1000).unwrap()
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    /// Every sample with the format and span length reported just before it
    fn trace(source: &mut dyn Source) -> Vec<(f32, u16, u32, Option<usize>)> {
        let mut out = Vec::new();
        loop {
            let (ch, rate, span) = (
                source.channels().get(),
                source.sample_rate().get(),
                source.current_span_len(),
            );
            match source.next() {
                Some(s) => out.push((s, ch, rate, span)),
                None => return out,
            }
        }
    }

    /// The audio (not the silence) of `output` until it ends, with each
    /// sample's format. Panics if it doesn't end within 5 s.
    fn audio_of(output: &mut PcmOutput) -> Vec<(f32, u16, u32)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut out = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "the output never ended");
            let in_audio = output.pos < output.samples.len();
            let (ch, rate) = (output.channels.get(), output.sample_rate.get());
            match output.next() {
                Some(s) if in_audio => out.push((s, ch, rate)),
                Some(_) => thread::yield_now(),
                None => return out,
            }
        }
    }

    const SPANS: &[(u16, u32, usize)] = &[(1, 44_100, 3000), (2, 48_000, 700), (2, 22_050, 2500)];

    fn feed(source: impl Source + Send + 'static) -> (PcmFeed, StreamCancel) {
        let cancel = StreamCancel::new();
        let feed = PcmFeed::start(source, Duration::from_millis(100), cancel.clone()).unwrap();
        (feed, cancel)
    }

    #[test]
    fn chunks_keep_one_format_and_whole_frames() {
        let mut chunker = Chunker::new(Spans::new(SPANS));
        let mut chunks = Vec::new();
        while let Some(chunk) = chunker.next_chunk(Vec::new()) {
            chunks.push(chunk);
        }
        // 20 ms chunks: 882 samples of 44.1 kHz mono, then the 700 samples
        // of 48 kHz stereo in one, then 882 of 22.05 kHz stereo
        let lens: Vec<_> = chunks
            .iter()
            .map(|c| (c.channels.get(), c.sample_rate.get(), c.samples.len()))
            .collect();
        assert_eq!(
            lens,
            [
                (1, 44_100, 882),
                (1, 44_100, 882),
                (1, 44_100, 882),
                (1, 44_100, 354),
                (2, 48_000, 700),
                (2, 22_050, 882),
                (2, 22_050, 882),
                (2, 22_050, 736),
            ]
        );
        let samples: Vec<f32> = chunks.iter().flat_map(|c| c.samples.clone()).collect();
        assert!(samples.iter().enumerate().all(|(i, s)| *s == i as f32));
    }

    #[test]
    fn a_source_ending_mid_frame_is_padded_to_a_whole_frame() {
        let mut chunker = Chunker::new(Spans::new(&[(2, 1000, 5)]));
        let chunk = chunker.next_chunk(Vec::new()).unwrap();
        assert_eq!(chunk.samples, [0.0, 1.0, 2.0, 3.0, 4.0, 0.0]);
        assert!(chunker.next_chunk(Vec::new()).is_none());
    }

    #[test]
    fn the_output_plays_the_source_as_it_was() {
        let (feed, _cancel) = feed(Spans::new(SPANS));
        let mut output = feed.output();
        drop(feed);
        let played = audio_of(&mut output);

        let expected: Vec<(f32, u16, u32)> = SPANS
            .iter()
            .flat_map(|&(ch, rate, len)| std::iter::repeat_n((ch, rate), len))
            .enumerate()
            .map(|(i, (ch, rate))| (i as f32, ch, rate))
            .collect();
        assert_eq!(played.len(), 6200);
        assert!(played == expected, "samples or formats changed");
    }

    #[test]
    fn spans_always_describe_what_plays_next() {
        let (feed, _cancel) = feed(Spans::new(SPANS));
        let mut output = feed.output();
        drop(feed);
        let traced = trace(&mut output);
        // Walk the spans: each sample is in the format its span announced,
        // and a span never claims more samples than it has
        let mut i = 0;
        while i < traced.len() {
            let (_, ch, rate, span) = traced[i];
            let len = span.expect("the output always knows its span");
            assert!(len > 0, "Some(0) would read as the end");
            assert_eq!(len % usize::from(ch), 0, "spans are whole frames");
            for (_, c, r, _) in &traced[i..(i + len).min(traced.len())] {
                assert_eq!((*c, *r), (ch, rate));
            }
            i += len;
        }
        assert_eq!(output.current_span_len(), Some(0));
    }

    #[test]
    fn a_stalled_station_plays_silence_without_waiting() {
        let release = Arc::new(AtomicBool::new(false));
        let source = Stalls {
            samples: vec![0.5; 100].into_iter(),
            release: release.clone(),
        };
        let (feed, _cancel) = feed(source);
        let mut output = feed.output();

        // The audio thread pulls a device buffer's worth at a time
        let start = Instant::now();
        let mut audio = 0;
        let mut silence = 0;
        for _ in 0..10_000 {
            match output.next() {
                Some(0.5) => audio += 1,
                Some(0.0) => silence += 1,
                other => panic!("unexpected {other:?}"),
            }
            if audio < 100 && silence > 0 {
                // The first audio may not be decoded yet: give it time
                thread::sleep(Duration::from_micros(50));
            }
        }
        assert_eq!(audio, 100);
        assert!(silence > 9000);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the output waited"
        );
        assert_eq!(feed.underruns(), 1, "one underrun, however long");

        // The station ends: so does the output, once it has noticed
        release.store(true, Ordering::SeqCst);
        drop(feed);
        assert!(audio_of(&mut output).is_empty());
    }

    #[test]
    fn a_new_output_carries_on_from_the_queue() {
        let (feed, _cancel) = feed(Spans::new(&[(1, 1000, 5000)]));
        let mut first = feed.output();
        let mut played = Vec::new();
        while played.len() < 10 {
            match first.next() {
                Some(s) if s != 0.0 || played.is_empty() => played.push(s),
                Some(_) => thread::yield_now(),
                None => panic!("ended early"),
            }
        }
        assert_eq!(played, (0..10).map(|i| i as f32).collect::<Vec<_>>());

        // The device goes away: a new output takes over the queue
        let mut second = feed.output();
        drop(first);
        drop(feed);
        let rest: Vec<f32> = audio_of(&mut second).into_iter().map(|a| a.0).collect();
        // What the first output had taken but not played is skipped;
        // nothing repeats and the order holds
        assert!(rest[0] >= 10.0);
        assert!(rest.windows(2).all(|w| w[1] == w[0] + 1.0));
        assert_eq!(*rest.last().unwrap(), 4999.0);
    }

    #[test]
    fn decoding_stays_ahead_by_the_set_amount() {
        let release = Arc::new(AtomicBool::new(false));
        let source = Stalls {
            samples: vec![0.5; 100_000].into_iter(),
            release: release.clone(),
        };
        let (feed, cancel) = feed(source);
        thread::sleep(Duration::from_millis(100));
        // 100 ms at 1 kHz, plus at most one chunk
        let queued = feed.shared.queued_us.load(Ordering::SeqCst);
        assert!((100_000..=120_000).contains(&queued), "queued {queued} us");
        cancel.cancel();
        release.store(true, Ordering::SeqCst);
    }

    #[test]
    fn a_cancel_ends_decoding_at_once() {
        let release = Arc::new(AtomicBool::new(false));
        let source = Stalls {
            samples: vec![0.5; 100_000].into_iter(),
            release: release.clone(),
        };
        let (feed, cancel) = feed(source);
        let mut output = feed.output();
        drop(feed);
        // Waiting for room in the queue
        thread::sleep(Duration::from_millis(50));
        cancel.cancel();
        // The decoder ends, and so the output after the queue
        let played = audio_of(&mut output);
        assert!(played.len() <= 120, "played {}", played.len());
        release.store(true, Ordering::SeqCst);
    }

    #[test]
    fn played_buffers_are_reused_rather_than_freed_on_the_audio_thread() {
        // Two seconds: 100 chunks
        let (feed, _cancel) = feed(Spans::new(&[(2, 48_000, 192_000)]));
        let mut output = feed.output();
        let shared = feed.shared.clone();
        drop(feed);
        let played = audio_of(&mut output);
        assert_eq!(played.len(), 192_000);
        // The queue's worth (100 ms, 5 chunks) and a few in hand
        let made = shared.new_buffers.load(Ordering::SeqCst);
        assert!(made <= 10, "{made} buffers for 100 chunks");
    }

    #[test]
    fn decode_ahead_follows_the_device_buffer() {
        let rate = NonZero::new(48_000).unwrap();
        let ahead = |frames| ahead_of_buffer(Some((BufferSize::Fixed(frames), rate)));
        // rodio's usual ~43 ms buffer: three of them
        assert_eq!(ahead(2048).as_micros(), 128_000);
        assert_eq!(ahead(256), Duration::from_millis(DECODE_AHEAD_MIN_MS));
        assert_eq!(ahead(48_000), Duration::from_millis(DECODE_AHEAD_MAX_MS));
        let unknown = Duration::from_millis(DECODE_AHEAD_DEFAULT_MS);
        assert_eq!(ahead_of_buffer(Some((BufferSize::Default, rate))), unknown);
        assert_eq!(decode_ahead(None), unknown);
    }
}
