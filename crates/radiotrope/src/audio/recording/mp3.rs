//! MP3 recordings: LAME at a constant bitrate, with an ID3v2.4 tag
//!
//! The file keeps the rate and channel count of the first audio; later
//! changes are resampled and remixed to match, as a file with more than
//! one MPEG version confuses players. When the file can seek, LAME's tag
//! frame (length, encoder delay and padding, for gapless playback) is
//! filled in when the recording ends.

use mp3lame_encoder::{Bitrate, Builder, Encoder, FlushGap, InterleavedPcm, Mode, MonoPcm};

use super::resample::Resampler;
use super::{AudioEncoder, RecordingTags};

/// Bitrates LAME can write, in kbps.
const BITRATES: [(u32, Bitrate); 16] = [
    (8, Bitrate::Kbps8),
    (16, Bitrate::Kbps16),
    (24, Bitrate::Kbps24),
    (32, Bitrate::Kbps32),
    (40, Bitrate::Kbps40),
    (48, Bitrate::Kbps48),
    (64, Bitrate::Kbps64),
    (80, Bitrate::Kbps80),
    (96, Bitrate::Kbps96),
    (112, Bitrate::Kbps112),
    (128, Bitrate::Kbps128),
    (160, Bitrate::Kbps160),
    (192, Bitrate::Kbps192),
    (224, Bitrate::Kbps224),
    (256, Bitrate::Kbps256),
    (320, Bitrate::Kbps320),
];

/// The MP3 bitrate closest to `kbps` (the higher one on a tie).
fn nearest_bitrate(kbps: u32) -> Bitrate {
    BITRATES
        .iter()
        .min_by_key(|(k, _)| (k.abs_diff(kbps), u32::MAX - k))
        .map(|(_, b)| *b)
        .unwrap_or(Bitrate::Kbps192)
}

pub(super) struct Mp3Encoder {
    tags: Option<RecordingTags>,
    bitrate: Bitrate,
    /// The file can seek, so the LAME tag can be filled in at the end
    seekable: bool,
    lame: Option<Lame>,
    /// Resampler for the current input rate, if it isn't the file's
    resampler: Option<(u32, Resampler)>,
    resampled: Vec<f32>,
    /// Where the MP3 frames start, after the ID3 tag
    audio_start: u64,
    /// The finished LAME tag frame, to write over the first frame
    lame_tag: Vec<u8>,
}

impl Mp3Encoder {
    pub(super) fn new(tags: RecordingTags, kbps: u32, seekable: bool) -> Self {
        Self {
            tags: Some(tags),
            bitrate: nearest_bitrate(kbps),
            seekable,
            lame: None,
            resampler: None,
            resampled: Vec::new(),
            audio_start: 0,
            lame_tag: Vec::new(),
        }
    }

    fn write_tag(&mut self, out: &mut Vec<u8>) {
        if let Some(tags) = self.tags.take() {
            let tag = id3v2_tag(&tags);
            self.audio_start = tag.len() as u64;
            out.extend_from_slice(&tag);
        }
    }
}

impl AudioEncoder for Mp3Encoder {
    fn encode(
        &mut self,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        self.write_tag(out);
        if self.lame.is_none() {
            let lame = Lame::new(sample_rate, channels, self.bitrate, self.seekable)?;
            self.lame = Some(lame);
        }
        let Some(lame) = self.lame.as_mut() else {
            return Ok(());
        };

        let converted;
        let samples = if channels == lame.channels {
            samples
        } else {
            converted = super::convert_channels(samples, channels, lame.channels);
            &converted[..]
        };

        // A new rate: first encode what the old resampler still holds.
        if let Some((_, mut old)) = self.resampler.take_if(|(rate, _)| *rate != sample_rate) {
            self.resampled.clear();
            old.finish(&mut self.resampled)?;
            lame.encode(&self.resampled, out)?;
        }
        if sample_rate == lame.sample_rate {
            return lame.encode(samples, out);
        }
        if self.resampler.is_none() {
            self.resampler = Some((
                sample_rate,
                Resampler::new(sample_rate, lame.sample_rate, lame.channels)?,
            ));
        }
        if let Some((_, r)) = self.resampler.as_mut() {
            self.resampled.clear();
            r.process(samples, &mut self.resampled)?;
            lame.encode(&self.resampled, out)?;
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        self.write_tag(out);
        let Some(mut lame) = self.lame.take() else {
            return Ok(());
        };
        if let Some((_, mut r)) = self.resampler.take() {
            self.resampled.clear();
            r.finish(&mut self.resampled)?;
            lame.encode(&self.resampled, out)?;
        }
        lame.finish(out);
        self.lame_tag = lame.lame_tag();
        Ok(())
    }

    fn header_patches(&self) -> Vec<(u64, Vec<u8>)> {
        if self.lame_tag.is_empty() {
            return Vec::new();
        }
        vec![(self.audio_start, self.lame_tag.clone())]
    }
}

/// A LAME encoder set up for one input format.
struct Lame {
    encoder: Encoder,
    sample_rate: u32,
    channels: u16,
}

impl Lame {
    /// With `lame_tag`, LAME starts with a placeholder frame that
    /// [`Lame::lame_tag`] fills in at the end.
    fn new(
        sample_rate: u32,
        channels: u16,
        bitrate: Bitrate,
        lame_tag: bool,
    ) -> Result<Self, String> {
        let mono = channels == 1;
        let mut builder = Builder::new().ok_or("Could not create the MP3 encoder")?;
        builder
            .set_sample_rate(sample_rate)
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_num_channels(if mono { 1 } else { 2 })
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_brate(bitrate)
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_mode(if mono { Mode::Mono } else { Mode::JointStereo })
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_quality(mp3lame_encoder::Quality::NearBest)
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        builder
            .set_to_write_vbr_tag(lame_tag)
            .map_err(|e| format!("MP3 encoder: {e}"))?;
        let encoder = builder.build().map_err(|e| format!("MP3 encoder: {e}"))?;
        Ok(Self {
            encoder,
            sample_rate,
            channels,
        })
    }

    /// Encode interleaved mono or stereo samples, appending MP3 data to `out`.
    fn encode(&mut self, samples: &[f32], out: &mut Vec<u8>) -> Result<(), String> {
        let frames = samples.len() / self.channels as usize;
        // LAME's worst case: 1.25 * samples + 7200 bytes.
        out.reserve(frames * 5 / 4 + 7200);
        let result = if self.channels == 1 {
            self.encoder.encode_to_vec(MonoPcm(samples), out)
        } else {
            self.encoder.encode_to_vec(InterleavedPcm(samples), out)
        };
        result
            .map(|_| ())
            .map_err(|e| format!("MP3 encoding failed: {e}"))
    }

    /// Encode the audio LAME still holds, padded to whole frames, into `out`.
    fn finish(&mut self, out: &mut Vec<u8>) {
        out.reserve(7200);
        // Not the "no gap" flush: that one leaves the last samples unencoded.
        let _ = self.encoder.flush_to_vec::<FlushGap>(out);
    }

    /// The finished LAME tag frame, after [`Lame::finish`]; empty when the
    /// tag is off.
    fn lame_tag(&self) -> Vec<u8> {
        let mut frame = Vec::with_capacity(self.encoder.lame_tag_size());
        self.encoder.lame_tag_encode_to_vec(&mut frame);
        frame
    }
}

// ---------------------------------------------------------------------------
// ID3v2 tag
// ---------------------------------------------------------------------------

/// Build an ID3v2.4 tag (UTF-8 text) for the start of the file.
///
/// Returns an empty vector when no field is set.
pub(super) fn id3v2_tag(tags: &RecordingTags) -> Vec<u8> {
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

    #[test]
    fn bitrate_snaps_to_nearest_mp3_rate() {
        assert_eq!(nearest_bitrate(192) as u32, 192);
        assert_eq!(nearest_bitrate(150) as u32, 160);
        assert_eq!(nearest_bitrate(144) as u32, 160, "tie goes up");
        assert_eq!(nearest_bitrate(1000) as u32, 320);
        assert_eq!(nearest_bitrate(0) as u32, 8);
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
    fn lame_tag_is_filled_in_only_when_the_file_can_seek() {
        let tags = RecordingTags {
            title: "Test FM".to_string(),
            ..Default::default()
        };
        let sine: Vec<f32> = (0..44_100)
            .flat_map(|i| {
                let v = (i as f32 * 0.06).sin() * 0.5;
                [v, v]
            })
            .collect();
        let record = |seekable| {
            let mut enc = Mp3Encoder::new(tags.clone(), 128, seekable);
            let mut out = Vec::new();
            enc.encode(44_100, 2, &sine, &mut out).unwrap();
            enc.finish(&mut out).unwrap();
            (out, enc.header_patches())
        };

        let (with_tag, patches) = record(true);
        let [(at, frame)] = &patches[..] else {
            panic!("{} patches", patches.len());
        };
        // Written over LAME's placeholder, the first frame after the ID3 tag
        let at = *at as usize;
        assert_eq!(at, id3v2_tag(&tags).len());
        // Same version, bitrate, rate and padding, so the same size
        assert_eq!(&frame[..3], &with_tag[at..at + 3]);
        assert!(with_tag[at + 4..at + frame.len()].iter().all(|&b| b == 0));
        assert_eq!(with_tag[at + frame.len()], 0xff, "the audio frames follow");
        assert!(frame.windows(4).any(|w| w == b"Info"));
        assert!(frame.windows(4).any(|w| w == b"LAME"));

        // Without seeking there is no placeholder to fill in
        let (without, patches) = record(false);
        assert!(patches.is_empty());
        assert_eq!(without.len(), with_tag.len() - frame.len());
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
