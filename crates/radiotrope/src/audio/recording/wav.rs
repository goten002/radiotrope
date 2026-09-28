//! WAV recordings: uncompressed 16-bit PCM
//!
//! The file keeps the rate and channel count of the first audio; later
//! changes are resampled and remixed to match. The RIFF sizes are written
//! as zero at the start and filled in when the recording ends. WAV sizes
//! are 32-bit, so a file stops growing just under 4 GiB.

use super::resample::Resampler;
use super::{AudioEncoder, RecordingTags};

/// Byte offset of the RIFF chunk size.
const RIFF_SIZE_OFFSET: u64 = 4;

/// Largest audio data a WAV file can describe, in bytes, with room for
/// the header and tags.
const MAX_DATA_BYTES: u64 = u32::MAX as u64 - 1024 * 1024;

pub(super) struct WavEncoder {
    tags: RecordingTags,
    format: Option<Format>,
    /// Header bytes before the audio, and where the data size goes
    header_len: u64,
    data_size_offset: u64,
    data_bytes: u64,
    resampler: Option<(u32, Resampler)>,
    scratch: Vec<f32>,
    full: bool,
}

#[derive(Clone, Copy)]
struct Format {
    sample_rate: u32,
    channels: u16,
}

impl WavEncoder {
    pub(super) fn new(tags: RecordingTags) -> Self {
        Self {
            tags,
            format: None,
            header_len: 0,
            data_size_offset: 0,
            data_bytes: 0,
            resampler: None,
            scratch: Vec::new(),
            full: false,
        }
    }

    fn start(&mut self, format: Format, out: &mut Vec<u8>) {
        let header = wav_header(format, &self.tags);
        self.header_len = header.len() as u64;
        self.data_size_offset = self.header_len - 4;
        out.extend_from_slice(&header);
        self.format = Some(format);
    }

    fn push_pcm(&mut self, samples: &[f32], out: &mut Vec<u8>) -> Result<(), String> {
        let room = MAX_DATA_BYTES.saturating_sub(self.data_bytes) as usize;
        let channels = self.format.map_or(2, |f| f.channels as usize);
        // Whole frames only
        let fits = (room / 2 / channels * channels).min(samples.len());
        out.reserve(fits * 2);
        for &s in &samples[..fits] {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        self.data_bytes += fits as u64 * 2;
        if fits < samples.len() {
            self.full = true;
            return Err("WAV files are limited to 4 GB, so the recording stopped".to_string());
        }
        Ok(())
    }
}

impl AudioEncoder for WavEncoder {
    fn encode(
        &mut self,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        if self.full {
            return Ok(());
        }
        let format = match self.format {
            Some(f) => f,
            None => {
                let f = Format {
                    sample_rate,
                    channels,
                };
                self.start(f, out);
                f
            }
        };

        let converted;
        let samples = if channels == format.channels {
            samples
        } else {
            converted = super::convert_channels(samples, channels, format.channels);
            &converted[..]
        };

        // A new rate: first write what the old resampler still holds.
        if let Some((_, mut old)) = self.resampler.take_if(|(rate, _)| *rate != sample_rate) {
            let mut tail = Vec::new();
            old.finish(&mut tail)?;
            self.push_pcm(&tail, out)?;
        }
        if sample_rate == format.sample_rate {
            return self.push_pcm(samples, out);
        }
        if self.resampler.is_none() {
            self.resampler = Some((
                sample_rate,
                Resampler::new(sample_rate, format.sample_rate, format.channels)?,
            ));
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        if let Some((_, r)) = self.resampler.as_mut() {
            r.process(samples, &mut scratch)?;
        }
        let result = self.push_pcm(&scratch, out);
        self.scratch = scratch;
        result
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        if self.format.is_none() {
            self.start(
                Format {
                    sample_rate: 44_100,
                    channels: 2,
                },
                out,
            );
        }
        if let Some((_, mut r)) = self.resampler.take() {
            let mut tail = Vec::new();
            r.finish(&mut tail)?;
            if !self.full {
                // Hitting the size limit here only loses the last few ms.
                let _ = self.push_pcm(&tail, out);
            }
        }
        Ok(())
    }

    fn header_patches(&self) -> Vec<(u64, Vec<u8>)> {
        let data = self.data_bytes as u32;
        // RIFF size counts everything after its own 8 bytes
        let riff = (self.header_len + self.data_bytes - 8) as u32;
        vec![
            (RIFF_SIZE_OFFSET, riff.to_le_bytes().to_vec()),
            (self.data_size_offset, data.to_le_bytes().to_vec()),
        ]
    }
}

/// RIFF/WAVE header with a LIST INFO chunk for the tags, ending with the
/// "data" chunk header (sizes zero until the end).
fn wav_header(format: Format, tags: &RecordingTags) -> Vec<u8> {
    let block_align = format.channels * 2;
    let mut h = Vec::with_capacity(256);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&0u32.to_le_bytes());
    h.extend_from_slice(b"WAVE");

    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&format.channels.to_le_bytes());
    h.extend_from_slice(&format.sample_rate.to_le_bytes());
    h.extend_from_slice(&(format.sample_rate * block_align as u32).to_le_bytes());
    h.extend_from_slice(&block_align.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());

    let mut info = Vec::new();
    for (id, text) in [
        (b"INAM", &tags.title),
        (b"IART", &tags.artist),
        (b"IPRD", &tags.album),
        (b"ICMT", &tags.comment),
    ] {
        if text.is_empty() {
            continue;
        }
        // UTF-8, NUL-terminated, padded to an even length
        let len = text.len() + 1;
        info.extend_from_slice(id);
        info.extend_from_slice(&(len as u32).to_le_bytes());
        info.extend_from_slice(text.as_bytes());
        info.push(0);
        if len % 2 == 1 {
            info.push(0);
        }
    }
    if !info.is_empty() {
        h.extend_from_slice(b"LIST");
        h.extend_from_slice(&(info.len() as u32 + 4).to_le_bytes());
        h.extend_from_slice(b"INFO");
        h.extend_from_slice(&info);
    }

    h.extend_from_slice(b"data");
    h.extend_from_slice(&0u32.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_and_patches() {
        let tags = RecordingTags {
            title: "Test FM".into(), // 7 bytes + NUL: even, no pad
            artist: "Ab".into(),     // 2 + NUL: padded
            ..Default::default()
        };
        let mut enc = WavEncoder::new(tags);
        let mut out = Vec::new();
        enc.encode(44100, 2, &[0.5, -0.5, 1.5, -1.5], &mut out)
            .unwrap();
        enc.finish(&mut out).unwrap();

        assert_eq!(&out[..4], b"RIFF");
        assert_eq!(&out[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([out[22], out[23]]), 2);
        assert_eq!(u32::from_le_bytes(out[24..28].try_into().unwrap()), 44100);
        assert_eq!(&out[36..40], b"LIST");
        let data_at = enc.header_len as usize;
        assert_eq!(&out[data_at - 8..data_at - 4], b"data");
        assert_eq!(out.len(), data_at + 8);
        // Clipped to full scale
        let s = |i: usize| i16::from_le_bytes([out[data_at + i * 2], out[data_at + i * 2 + 1]]);
        assert_eq!((s(0), s(1), s(2), s(3)), (16384, -16384, 32767, -32767));

        let patches = enc.header_patches();
        assert_eq!(
            patches[0],
            (4, ((data_at + 8 - 8) as u32).to_le_bytes().to_vec())
        );
        assert_eq!(
            patches[1],
            (data_at as u64 - 4, 8u32.to_le_bytes().to_vec())
        );
    }
}
