//! MP3 recordings: LAME at 192 kbps, with an ID3v2.4 tag

use mp3lame_encoder::{Bitrate, Builder, Encoder, FlushNoGap, InterleavedPcm, Mode, MonoPcm};

use super::{AudioEncoder, RecordingTags};

/// MP3 bitrate for stereo recordings.
const STEREO_BITRATE: Bitrate = Bitrate::Kbps192;

/// MP3 bitrate for mono recordings.
const MONO_BITRATE: Bitrate = Bitrate::Kbps96;

pub(super) struct Mp3Encoder {
    tags: Option<RecordingTags>,
    lame: Option<Lame>,
}

impl Mp3Encoder {
    pub(super) fn new(tags: RecordingTags) -> Self {
        Self {
            tags: Some(tags),
            lame: None,
        }
    }

    fn write_tag(&mut self, out: &mut Vec<u8>) {
        if let Some(tags) = self.tags.take() {
            out.extend_from_slice(&id3v2_tag(&tags));
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
        let format_changed = self
            .lame
            .as_ref()
            .is_some_and(|l| l.sample_rate != sample_rate || l.channels != channels);
        if format_changed {
            // E.g. HE-AAC switching rate: finish these frames, go on in the
            // new format in the same file. MP3 frames stand alone, so
            // players follow the change.
            if let Some(mut old) = self.lame.take() {
                old.finish(out);
            }
        }
        if self.lame.is_none() {
            self.lame = Some(Lame::new(sample_rate, channels)?);
        }
        match self.lame.as_mut() {
            Some(lame) => lame.encode(samples, out),
            None => Ok(()),
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        self.write_tag(out);
        if let Some(mut lame) = self.lame.take() {
            lame.finish(out);
        }
        Ok(())
    }
}

/// A LAME encoder set up for one input format.
struct Lame {
    encoder: Encoder,
    sample_rate: u32,
    channels: u16,
}

impl Lame {
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

    /// Flush the encoder's last frames into `out`.
    fn finish(&mut self, out: &mut Vec<u8>) {
        out.reserve(7200);
        let _ = self.encoder.flush_to_vec::<FlushNoGap>(out);
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
