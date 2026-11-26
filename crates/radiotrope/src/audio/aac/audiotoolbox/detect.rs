//! Which kind of AAC a stream is, as AudioToolbox reads it
//!
//! ADTS streams (ICY stations, HLS TS segments) and many MP4 tracks declare
//! plain AAC-LC at the core's rate, and signal SBR and Parametric Stereo
//! only inside the frames. AudioToolbox's AAC-LC decoder plays just the
//! core of such a stream, so the HE-AAC decoder has to be asked for by
//! name. Its stream parser (`AudioFileStream`) is what tells: given one
//! frame with an ADTS header, it lists the formats the stream can be
//! decoded as, the best one first:
//!
//! - plain AAC: `aac ` at the declared rate
//! - HE-AAC, stereo core: `aach` at twice the rate, then `aac `
//! - a mono core with SBR: `aacp` (HE-AAC v2) in stereo at twice the rate,
//!   then `aach` in mono, then `aac `. Parametric Stereo can't be told
//!   from one frame, and the v2 decoder plays both: plain mono HE-AAC
//!   comes out on two equal channels (FDK does the same).
//!
//! A frame without an SBR payload (the silent first frame that Apple's own
//! encoder writes) leaves the parser undecided until the next one.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;

use super::converter::Format;
use super::sys::*;

/// An ADTS header for the frames of one track, which reach the decoder
/// without theirs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AdtsHeader {
    /// `sampling_frequency_index`: the core's rate
    pub rate_index: u8,
    /// `channel_configuration`: 1 (mono) or 2 (stereo)
    pub channels: u8,
}

/// The longest frame an ADTS header can describe (13 bits, header included)
const MAX_ADTS_FRAME: usize = 0x1FFF;
const HEADER_LEN: usize = 7;

impl AdtsHeader {
    /// `payload` as an ADTS frame: MPEG-4, AAC-LC, no CRC. `None` for a
    /// packet too long to be one.
    pub(super) fn frame(&self, payload: &[u8]) -> Option<Vec<u8>> {
        let len = payload.len() + HEADER_LEN;
        if len > MAX_ADTS_FRAME {
            return None;
        }
        let mut frame = Vec::with_capacity(len);
        frame.extend_from_slice(&[
            0xFF,
            0xF1,
            // Profile (object type 2, less one), the rate, the channels
            (1 << 6) | (self.rate_index << 2) | (self.channels >> 2),
            ((self.channels & 3) << 6) | (len >> 11) as u8,
            (len >> 3) as u8,
            // The buffer fullness of a variable bit rate (all ones)
            ((len & 7) << 5) as u8 | 0x1F,
            0xFC,
        ]);
        frame.extend_from_slice(payload);
        Some(frame)
    }
}

/// The formats in `items` that this decoder can be made for, in their
/// order: AAC, HE-AAC and HE-AAC v2 in mono or stereo
fn usable(items: &[AudioFormatListItem]) -> Vec<Format> {
    items
        .iter()
        .map(|item| item.mASBD)
        .filter(|format| {
            matches!(
                format.mFormatID,
                kAudioFormatMPEG4AAC | kAudioFormatMPEG4AAC_HE | kAudioFormatMPEG4AAC_HE_V2
            ) && (1..=2).contains(&format.mChannelsPerFrame)
                && (8000.0..=96000.0).contains(&format.mSampleRate)
                && matches!(format.mFramesPerPacket, 1024 | 2048)
        })
        .map(|format| Format {
            id: format.mFormatID,
            rate: format.mSampleRate as u32,
            channels: format.mChannelsPerFrame,
            frames_per_packet: format.mFramesPerPacket,
        })
        .collect()
}

/// The parser tells its client that a property has a value
unsafe extern "C" fn on_property(
    client: *mut c_void,
    _stream: AudioFileStreamID,
    property: u32,
    _flags: *mut u32,
) {
    if property == kAudioFileStreamProperty_ReadyToProducePackets {
        // SAFETY: the client is the `Cell` its `Detector` keeps boxed
        unsafe { (*client.cast::<Cell<bool>>()).set(true) };
    }
}

/// The parser hands over the packets it found: not needed, the demuxer
/// has them already
unsafe extern "C" fn on_packets(
    _client: *mut c_void,
    _bytes: u32,
    _packets: u32,
    _data: *const c_void,
    _descriptions: *mut AudioStreamPacketDescription,
) {
}

/// AudioToolbox's parser, reading the frames of one ADTS stream until it
/// knows what they are
pub(super) struct Detector {
    stream: AudioFileStreamID,
    /// Set by the parser's callback when it has read enough
    ready: Box<Cell<bool>>,
}

// SAFETY: the parser has no ties to a thread, and it (and `ready`, which
// only its callback writes, during `feed`) is used through `&mut self`
unsafe impl Send for Detector {}
// SAFETY: nothing reaches the parser through `&self`
unsafe impl Sync for Detector {}

impl Drop for Detector {
    fn drop(&mut self) {
        // SAFETY: opened in `new`, closed only here
        unsafe { AudioFileStreamClose(self.stream) };
    }
}

impl Detector {
    pub(super) fn new() -> Option<Self> {
        let ready = Box::new(Cell::new(false));
        let mut stream: AudioFileStreamID = ptr::null_mut();
        // SAFETY: the client pointer stays valid for as long as the parser
        // is open: the box is dropped after `drop` closes it
        let status = unsafe {
            AudioFileStreamOpen(
                ptr::from_ref::<Cell<bool>>(&ready).cast_mut().cast(),
                on_property,
                on_packets,
                kAudioFileAAC_ADTSType,
                &mut stream,
            )
        };
        (status == noErr && !stream.is_null()).then_some(Self { stream, ready })
    }

    /// Read one more ADTS frame. Gives the formats the stream decodes as,
    /// the best first, once the parser knows; `None` until then.
    pub(super) fn feed(&mut self, frame: &[u8]) -> Option<Vec<Format>> {
        // SAFETY: an open parser and `frame.len()` readable bytes, which
        // it copies what it needs from
        let status = unsafe {
            AudioFileStreamParseBytes(self.stream, frame.len() as u32, frame.as_ptr().cast(), 0)
        };
        if status != noErr || !self.ready.get() {
            return None;
        }
        Some(usable(&self.format_list()))
    }

    fn format_list(&self) -> Vec<AudioFormatListItem> {
        let item = std::mem::size_of::<AudioFormatListItem>();
        let mut size = 0u32;
        let mut writable = 0u8;
        // SAFETY: an open parser and places for the two answers
        let status = unsafe {
            AudioFileStreamGetPropertyInfo(
                self.stream,
                kAudioFileStreamProperty_FormatList,
                &mut size,
                &mut writable,
            )
        };
        let count = size as usize / item;
        if status != noErr || count == 0 {
            return Vec::new();
        }
        let mut items = vec![AudioFormatListItem::default(); count];
        let mut size = (count * item) as u32;
        // SAFETY: `items` has room for `size` bytes
        let status = unsafe {
            AudioFileStreamGetProperty(
                self.stream,
                kAudioFileStreamProperty_FormatList,
                &mut size,
                items.as_mut_ptr().cast(),
            )
        };
        if status != noErr {
            return Vec::new();
        }
        items.truncate(size as usize / item);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packet_gets_its_adts_header_back() {
        let header = AdtsHeader {
            rate_index: 7,
            channels: 2,
        };
        let frame = header.frame(&[0xAA; 364]).unwrap();
        // As FDK's encoder writes it for 22.05 kHz stereo, 371 bytes
        assert_eq!(frame[..6], [0xFF, 0xF1, 0x5C, 0x80, 0x2E, 0x7F]);
        assert_eq!(frame.len(), 371);
        assert_eq!(frame[7..], [0xAA; 364]);
        // The length the demuxer reads back
        let len =
            ((frame[3] as usize & 3) << 11) | ((frame[4] as usize) << 3) | (frame[5] as usize >> 5);
        assert_eq!(len, 371);

        let mono = AdtsHeader {
            rate_index: 4,
            channels: 1,
        };
        assert_eq!(mono.frame(&[]).unwrap()[..4], [0xFF, 0xF1, 0x50, 0x40]);
        assert_eq!(
            header.frame(&vec![0; MAX_ADTS_FRAME - 7]).unwrap().len(),
            8191
        );
        assert_eq!(header.frame(&vec![0; MAX_ADTS_FRAME - 6]), None);
    }

    fn item(id: u32, rate: f64, channels: u32, frames: u32) -> AudioFormatListItem {
        AudioFormatListItem {
            mASBD: AudioStreamBasicDescription {
                mSampleRate: rate,
                mFormatID: id,
                mChannelsPerFrame: channels,
                mFramesPerPacket: frames,
                ..Default::default()
            },
            mChannelLayoutTag: 0,
        }
    }

    #[test]
    fn the_formats_that_can_be_decoded_are_kept_in_order() {
        // What the parser lists for a mono core with SBR
        let list = [
            item(kAudioFormatMPEG4AAC_HE_V2, 44_100.0, 2, 2048),
            item(kAudioFormatMPEG4AAC_HE, 44_100.0, 1, 2048),
            item(kAudioFormatMPEG4AAC, 22_050.0, 1, 1024),
        ];
        let formats = usable(&list);
        assert_eq!(formats.len(), 3);
        assert_eq!(
            formats[0],
            Format {
                id: kAudioFormatMPEG4AAC_HE_V2,
                rate: 44_100,
                channels: 2,
                frames_per_packet: 2048
            }
        );
        assert_eq!(formats[2].rate, 22_050);
        // Surround, another codec and a format without its numbers are
        // left out
        let odd = [
            item(kAudioFormatMPEG4AAC, 48_000.0, 6, 1024),
            item(fourcc(b".mp3"), 44_100.0, 2, 1152),
            item(kAudioFormatMPEG4AAC_HE, 0.0, 2, 2048),
            item(kAudioFormatMPEG4AAC_HE, 44_100.0, 2, 0),
        ];
        assert_eq!(usable(&odd), Vec::new());
    }
}
