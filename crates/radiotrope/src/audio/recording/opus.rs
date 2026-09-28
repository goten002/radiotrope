//! Opus in Ogg (`.opus`) recordings
//!
//! Encodes with the libopus the decoder already builds (opusic-sys), and
//! writes the Ogg pages by hand (RFC 3533 and RFC 7845). Opus runs at
//! 48 kHz, so other rates are resampled first.

use std::ffi::c_int;

use opusic_sys as ffi;

use super::resample::Resampler;
use super::{AudioEncoder, RecordingTags};

/// Opus always works at 48 kHz.
const OPUS_RATE: u32 = 48_000;

/// 20 ms frames, the usual size for music.
const FRAME: usize = 960;

/// Largest Opus packet (RFC 6716) times three frames of headroom.
const MAX_PACKET: usize = 1275 * 3;

/// Opus bitrates allowed, in kbps.
const MIN_KBPS: u32 = 6;
const MAX_KBPS: u32 = 510;

/// Ogg pages are closed after about this much audio (1 s of 20 ms packets).
const PACKETS_PER_PAGE: usize = 50;

/// Owns a libopus encoder.
struct Encoder(*mut ffi::OpusEncoder);

impl Encoder {
    fn new(channels: u16, kbps: u32) -> Result<Self, String> {
        let mut err: c_int = 0;
        // SAFETY: valid rate, channel count and application; err is written.
        let ptr = unsafe {
            ffi::opus_encoder_create(
                OPUS_RATE as i32,
                channels as c_int,
                ffi::OPUS_APPLICATION_AUDIO,
                &mut err,
            )
        };
        if ptr.is_null() || err != ffi::OPUS_OK {
            return Err(format!("Could not create the Opus encoder ({err})"));
        }
        let enc = Self(ptr);
        let bitrate = (kbps.clamp(MIN_KBPS, MAX_KBPS) * 1000) as i32;
        // SAFETY: ptr is a live encoder; these requests take one opus_int32.
        unsafe {
            ffi::opus_encoder_ctl(enc.0, ffi::OPUS_SET_BITRATE_REQUEST, bitrate);
            ffi::opus_encoder_ctl(enc.0, ffi::OPUS_SET_SIGNAL_REQUEST, ffi::OPUS_SIGNAL_MUSIC);
        }
        Ok(enc)
    }

    /// Samples the encoder delays its output by, at 48 kHz.
    fn lookahead(&self) -> u16 {
        let mut value: i32 = 0;
        // SAFETY: ptr is a live encoder; this request takes an opus_int32 pointer.
        let err = unsafe {
            ffi::opus_encoder_ctl(
                self.0,
                ffi::OPUS_GET_LOOKAHEAD_REQUEST,
                &mut value as *mut i32,
            )
        };
        if err == ffi::OPUS_OK {
            value.clamp(0, u16::MAX as i32) as u16
        } else {
            312
        }
    }

    /// Encode one frame of `FRAME` interleaved samples per channel.
    fn encode(&mut self, pcm: &[f32], packet: &mut [u8]) -> Result<usize, String> {
        // SAFETY: pcm holds FRAME frames for the encoder's channel count,
        // and packet is writable for its whole length.
        let n = unsafe {
            ffi::opus_encode_float(
                self.0,
                pcm.as_ptr(),
                FRAME as c_int,
                packet.as_mut_ptr(),
                packet.len() as i32,
            )
        };
        if n < 0 {
            Err(format!("Opus encoding failed ({n})"))
        } else {
            Ok(n as usize)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create and destroyed once.
        unsafe { ffi::opus_encoder_destroy(self.0) }
    }
}

pub(super) struct OpusEncoder {
    tags: RecordingTags,
    kbps: u32,
    /// Set up on the first audio, when the channel count is known
    state: Option<Stream>,
}

struct Stream {
    encoder: Encoder,
    channels: u16,
    pre_skip: u16,
    /// Resampler for the current input rate, if it isn't 48 kHz
    resampler: Option<(u32, Resampler)>,
    /// 48 kHz samples waiting for a full frame
    pending: Vec<f32>,
    resampled: Vec<f32>,
    packet: Vec<u8>,
    ogg: OggWriter,
    /// Real 48 kHz frames encoded so far (without padding)
    frames: u64,
    /// Frames encoded so far, padding included
    encoded: u64,
}

impl OpusEncoder {
    pub(super) fn new(tags: RecordingTags, kbps: u32) -> Self {
        Self {
            tags,
            kbps,
            state: None,
        }
    }

    fn start(&mut self, sample_rate: u32, channels: u16, out: &mut Vec<u8>) -> Result<(), String> {
        let encoder = Encoder::new(channels, self.kbps)?;
        let pre_skip = encoder.lookahead();
        let mut ogg = OggWriter::new(serial_number());
        ogg.packet(&opus_head(channels, pre_skip, sample_rate), 0, out);
        ogg.flush(out, false);
        ogg.packet(&opus_tags(&self.tags), 0, out);
        ogg.flush(out, false);
        self.state = Some(Stream {
            encoder,
            channels,
            pre_skip,
            resampler: None,
            pending: Vec::new(),
            resampled: Vec::new(),
            packet: vec![0; MAX_PACKET],
            ogg,
            frames: 0,
            encoded: 0,
        });
        Ok(())
    }
}

/// A complete Ogg Opus stream of `samples`, for decoder tests
#[cfg(test)]
pub(crate) fn encode_ogg_opus(sample_rate: u32, channels: u16, samples: &[f32]) -> Vec<u8> {
    let mut encoder = OpusEncoder::new(RecordingTags::default(), 96);
    let mut out = Vec::new();
    encoder
        .encode(sample_rate, channels, samples, &mut out)
        .unwrap();
    encoder.finish(&mut out).unwrap();
    out
}

impl AudioEncoder for OpusEncoder {
    fn encode(
        &mut self,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        if self.state.is_none() {
            self.start(sample_rate, channels, out)?;
        }
        let Some(s) = self.state.as_mut() else {
            return Ok(());
        };

        // The stream keeps its first channel count.
        let converted;
        let samples = if channels == s.channels {
            samples
        } else {
            converted = super::convert_channels(samples, channels, s.channels);
            &converted[..]
        };

        // A new rate: first take what the old resampler still holds.
        if let Some((_, mut old)) = s.resampler.take_if(|(rate, _)| *rate != sample_rate) {
            old.finish(&mut s.pending)?;
        }
        if sample_rate == OPUS_RATE {
            s.pending.extend_from_slice(samples);
        } else {
            if s.resampler.is_none() {
                s.resampler = Some((
                    sample_rate,
                    Resampler::new(sample_rate, OPUS_RATE, s.channels)?,
                ));
            }
            if let Some((_, r)) = s.resampler.as_mut() {
                s.resampled.clear();
                r.process(samples, &mut s.resampled)?;
                s.pending.extend_from_slice(&s.resampled);
            }
        }
        s.encode_frames(false, out)
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        if self.state.is_none() {
            // Nothing was recorded: still write a valid, empty stream.
            self.start(OPUS_RATE, 2, out)?;
        }
        let Some(s) = self.state.as_mut() else {
            return Ok(());
        };
        if let Some((_, mut r)) = s.resampler.take() {
            r.finish(&mut s.pending)?;
        }
        s.encode_frames(true, out)
    }
}

impl Stream {
    /// Encode every full frame in `pending`; at the end, also the last
    /// partial frame (padded with silence) and the encoder's delay.
    fn encode_frames(&mut self, last: bool, out: &mut Vec<u8>) -> Result<(), String> {
        let ch = self.channels as usize;
        let frame_len = FRAME * ch;
        if last {
            self.frames += (self.pending.len() / ch) as u64;
            // Flush the encoder's lookahead with silence, then pad.
            let total = self.pending.len() + self.pre_skip as usize * ch;
            let padded = total.div_ceil(frame_len).max(1) * frame_len;
            self.pending.resize(padded, 0.0);
        }
        let mut offset = 0;
        while self.pending.len() - offset >= frame_len {
            let pcm = &self.pending[offset..offset + frame_len];
            let n = self.encoder.encode(pcm, &mut self.packet)?;
            offset += frame_len;
            if !last {
                self.frames += FRAME as u64;
            }
            self.encoded += FRAME as u64;
            let done = last && self.pending.len() - offset < frame_len;
            // The last page's granule marks where the real audio ends.
            let granule = if done {
                self.frames + self.pre_skip as u64
            } else {
                self.encoded
            };
            self.ogg.packet(&self.packet[..n], granule, out);
            if done {
                self.ogg.flush(out, true);
            } else if self.ogg.packets_on_page >= PACKETS_PER_PAGE {
                self.ogg.flush(out, false);
            }
        }
        self.pending.drain(..offset);
        Ok(())
    }
}

/// Stream serial number: any value will do, but different files differ.
fn serial_number() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(0);
    nanos ^ std::process::id().rotate_left(16)
}

/// The identification header (RFC 7845 section 5.1).
fn opus_head(channels: u16, pre_skip: u16, input_rate: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1); // version
    h.push(channels as u8);
    h.extend_from_slice(&pre_skip.to_le_bytes());
    h.extend_from_slice(&input_rate.to_le_bytes());
    h.extend_from_slice(&0i16.to_le_bytes()); // output gain
    h.push(0); // mapping family: mono or stereo
    h
}

/// The comment header (RFC 7845 section 5.2), with Vorbis-style tags and
/// the cover as a METADATA_BLOCK_PICTURE.
fn opus_tags(tags: &RecordingTags) -> Vec<u8> {
    let mut comments: Vec<String> = [
        ("TITLE", &tags.title),
        ("ARTIST", &tags.artist),
        ("ALBUM", &tags.album),
        ("COMMENT", &tags.comment),
    ]
    .iter()
    .filter(|(_, v)| !v.is_empty())
    .map(|(k, v)| format!("{k}={v}"))
    .collect();
    if let Some(cover) = tags.cover.as_deref().filter(|c| !c.is_empty()) {
        comments.push(format!(
            "METADATA_BLOCK_PICTURE={}",
            base64(&flac_picture(cover))
        ));
    }

    let vendor = concat!("Radiotrope ", env!("CARGO_PKG_VERSION"));
    let mut t = Vec::new();
    t.extend_from_slice(b"OpusTags");
    t.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    t.extend_from_slice(vendor.as_bytes());
    t.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in &comments {
        t.extend_from_slice(&(c.len() as u32).to_le_bytes());
        t.extend_from_slice(c.as_bytes());
    }
    t
}

/// A FLAC picture block, as Ogg files carry cover art.
fn flac_picture(image: &[u8]) -> Vec<u8> {
    let mime: &[u8] = if image.starts_with(b"\x89PNG") {
        b"image/png"
    } else {
        b"image/jpeg"
    };
    let mut p = Vec::with_capacity(32 + mime.len() + image.len());
    p.extend_from_slice(&3u32.to_be_bytes()); // front cover
    p.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    p.extend_from_slice(mime);
    p.extend_from_slice(&0u32.to_be_bytes()); // description
    for _ in 0..4 {
        p.extend_from_slice(&0u32.to_be_bytes()); // width, height, depth, colours unknown
    }
    p.extend_from_slice(&(image.len() as u32).to_be_bytes());
    p.extend_from_slice(image);
    p
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Ogg pages
// ---------------------------------------------------------------------------

/// Packs packets into Ogg pages for one logical stream.
struct OggWriter {
    serial: u32,
    sequence: u32,
    /// Lacing values and data of the page being built
    lacing: Vec<u8>,
    body: Vec<u8>,
    /// Granule of the last packet that ends on this page, if any
    granule: Option<u64>,
    packets_on_page: usize,
    /// The page being built starts in the middle of a packet
    continued: bool,
    first: bool,
}

impl OggWriter {
    fn new(serial: u32) -> Self {
        Self {
            serial,
            sequence: 0,
            lacing: Vec::new(),
            body: Vec::new(),
            granule: None,
            packets_on_page: 0,
            continued: false,
            first: true,
        }
    }

    /// Add a packet whose last sample is at `granule`. Full pages are written to `out`.
    fn packet(&mut self, data: &[u8], granule: u64, out: &mut Vec<u8>) {
        let mut rest = data;
        // Some of this packet is already on the page being built
        let mut started = false;
        loop {
            if self.lacing.len() == 255 {
                // Page full. The next page continues a packet only if this
                // one was cut; a page that filled up exactly as the previous
                // packet ended starts clean (else demuxers drop a packet).
                self.write_page(out, false);
                self.continued = started;
            }
            started = true;
            let take = rest.len().min(255);
            self.lacing.push(take as u8);
            self.body.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            // A lacing value under 255 ends the packet.
            if take < 255 {
                break;
            }
        }
        self.granule = Some(granule);
        self.packets_on_page += 1;
    }

    /// Write the page being built, if any (always, when it ends the stream).
    fn flush(&mut self, out: &mut Vec<u8>, end: bool) {
        if !self.lacing.is_empty() || end {
            self.write_page(out, end);
        }
    }

    fn write_page(&mut self, out: &mut Vec<u8>, end: bool) {
        let mut flags = 0u8;
        if self.continued {
            flags |= 0x01;
        }
        if self.first {
            flags |= 0x02;
        }
        if end {
            flags |= 0x04;
        }
        // -1: no packet ends on this page
        let granule = self.granule.map(|g| g as i64).unwrap_or(-1);

        let start = out.len();
        out.extend_from_slice(b"OggS");
        out.push(0); // version
        out.push(flags);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&[0; 4]); // CRC, filled in below
        out.push(self.lacing.len() as u8);
        out.extend_from_slice(&self.lacing);
        out.extend_from_slice(&self.body);
        let crc = ogg_crc(&out[start..]);
        out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());

        self.sequence += 1;
        self.lacing.clear();
        self.body.clear();
        self.granule = None;
        self.packets_on_page = 0;
        self.continued = false;
        self.first = false;
    }
}

/// Ogg's CRC-32: polynomial 0x04C11DB7, not reflected, starting at 0.
fn ogg_crc(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut r = (i as u32) << 24;
            for _ in 0..8 {
                r = if r & 0x8000_0000 != 0 {
                    (r << 1) ^ 0x04C1_1DB7
                } else {
                    r << 1
                };
            }
            *v = r;
        }
        t
    });
    data.iter().fold(0u32, |crc, &b| {
        (crc << 8) ^ table[((crc >> 24) as u8 ^ b) as usize]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn crc_matches_ogg_reference() {
        // CRC-32/MPEG-2 style without final xor, init 0 ("CRC-32/BZIP2" differs)
        assert_eq!(ogg_crc(b""), 0);
        assert_eq!(ogg_crc(b"123456789"), 0x89A1_897F);
    }

    #[test]
    fn long_packets_span_pages() {
        let mut ogg = OggWriter::new(7);
        let mut out = Vec::new();
        ogg.packet(&vec![1u8; 255 * 300], 10, &mut out);
        ogg.flush(&mut out, true);
        // Two pages: the first full and without a finished packet
        let second = out[4..].windows(4).position(|w| w == b"OggS").unwrap() + 4;
        assert_eq!(out[5], 0x02, "first page: start of stream");
        assert_eq!(i64::from_le_bytes(out[6..14].try_into().unwrap()), -1);
        assert_eq!(out[26], 255);
        assert_eq!(out[second + 5], 0x01 | 0x04, "continued, end of stream");
        assert_eq!(
            i64::from_le_bytes(out[second + 6..second + 14].try_into().unwrap()),
            10
        );
        // 300 full lacing values then a 0 to end the packet: 45 + 1 on page two
        assert_eq!(out[second + 26], 46);
    }

    #[test]
    fn page_filled_by_a_whole_packet_is_not_continued() {
        let mut ogg = OggWriter::new(7);
        let mut out = Vec::new();
        // 254 lacing values of 255 plus the terminating 0: exactly 255
        ogg.packet(&vec![1u8; 255 * 254], 10, &mut out);
        ogg.packet(&[2u8; 10], 20, &mut out);
        ogg.flush(&mut out, true);
        let second = out[4..].windows(4).position(|w| w == b"OggS").unwrap() + 4;
        assert_eq!(out[26], 255, "first page holds exactly the first packet");
        assert_eq!(
            i64::from_le_bytes(out[6..14].try_into().unwrap()),
            10,
            "first packet ends on the first page"
        );
        assert_eq!(out[second + 5], 0x04, "second page starts a new packet");
    }

    #[test]
    fn rate_returning_to_48k_keeps_the_resampled_end() {
        let mut encoder = OpusEncoder::new(RecordingTags::default(), 96);
        let mut out = Vec::new();
        for rate in [48_000, 44_100, 48_000] {
            let second = vec![0.1; rate as usize * 2];
            encoder.encode(rate, 2, &second, &mut out).unwrap();
        }
        encoder.finish(&mut out).unwrap();
        // The frames the last granule counts: all three seconds
        let frames = encoder.state.as_ref().map(|s| s.frames);
        assert_eq!(frames, Some(3 * 48_000));
    }

    #[test]
    fn head_layout() {
        let h = opus_head(2, 312, 44100);
        assert_eq!(&h[..8], b"OpusHead");
        assert_eq!(h[9], 2);
        assert_eq!(u16::from_le_bytes([h[10], h[11]]), 312);
        assert_eq!(u32::from_le_bytes(h[12..16].try_into().unwrap()), 44100);
        assert_eq!(h.len(), 19);
    }
}
