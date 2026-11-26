//! macOS's own AAC decoders (AudioToolbox)
//!
//! Every Mac decodes AAC, HE-AAC and HE-AAC v2, so on macOS AAC+ needs no
//! libfdk-aac. A Symphonia decoder on top of an `AudioConverter`, fed one
//! access unit a call as the demuxers hand them over.
//!
//! The binding is a small hand-written one (`sys`), like FDK's: the nine
//! C calls used here need no crate of their own, and the framework is part
//! of the system.
//!
//! What a stream is (plain AAC, or AAC+ with SBR and maybe Parametric
//! Stereo) is mostly not in its headers, and AudioToolbox has one decoder
//! for each: see `detect`. So the kind is found from the first frames, and
//! the converter is made for it then. SBR doubles the rate the track
//! declares and Parametric Stereo makes a mono core stereo: both show in
//! the buffers this decoder gives, from the first one on.

// AudioToolbox's constants keep the names its headers give them
#![allow(non_upper_case_globals)]

mod converter;
mod detect;
mod sys;

use symphonia::core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia::core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
};
use symphonia::core::codecs::audio::well_known::profiles::{
    CODEC_PROFILE_AAC_HE, CODEC_PROFILE_AAC_HE_V2, CODEC_PROFILE_AAC_LC,
};
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia::core::codecs::CodecInfo;
use symphonia::core::errors::{decode_error, unsupported_error, Result};
use symphonia::core::io::{BitReaderLtr, ReadBitsLtr};
use symphonia::core::packet::PacketRef;
// The two macros name the crate they come from
use symphonia::core as symphonia_core;
use symphonia::core::{codec_profile, support_audio_codec};

use self::converter::{Converter, Format};
use self::detect::{AdtsHeader, Detector};
use self::sys::{kAudioFormatMPEG4AAC, kAudioFormatMPEG4AAC_HE, kAudioFormatMPEG4AAC_HE_V2};
use super::{open_without_audio_toolbox, AAC_SAMPLE_RATES, AOT_AAC_LC, AOT_PS, AOT_SBR};

/// Frames that may pass before the parser says what the stream is. It
/// knows from the first frame that has an SBR payload or real audio; a
/// stream may open with a silent frame or two.
const MAX_UNDECIDED: usize = 4;

/// The most frames a packet decodes to: 2048 with SBR
const MAX_PACKET_FRAMES: usize = 2048;

/// A stream playing as plain AAC is looked at again every this many
/// packets (about 12 seconds at 22.05 kHz): an HLS stream can move to
/// HE-AAC on the same track
const LOOK_AGAIN_AFTER: u32 = 256;

/// What a track declares: its AAC-LC core, and whether it names SBR
#[derive(Clone, Copy, Debug, PartialEq)]
struct Declared {
    header: AdtsHeader,
    /// The core's rate
    rate: u32,
    /// The config names SBR or PS outright (HE-AAC in MP4 often does)
    sbr: bool,
}

impl Declared {
    /// From the track's AudioSpecificConfig (MP4, fMP4) or, for ADTS, from
    /// the rate and channels of its headers. `None` for what this decoder
    /// doesn't play: surround, other object types (USAC), odd rates.
    fn of(params: &AudioCodecParameters) -> Option<Self> {
        let (rate_index, channels, sbr) = match params.extra_data.as_deref() {
            Some(config) => Self::read_config(config)?,
            None => {
                let rate = params.sample_rate?;
                let index = AAC_SAMPLE_RATES.iter().position(|&r| r == rate)?;
                (
                    index as u32,
                    params.channels.as_ref()?.count() as u32,
                    false,
                )
            }
        };
        let rate = *AAC_SAMPLE_RATES.get(rate_index as usize)?;
        if !(1..=2).contains(&channels) {
            return None;
        }
        Some(Self {
            header: AdtsHeader {
                rate_index: rate_index as u8,
                channels: channels as u8,
            },
            rate,
            sbr,
        })
    }

    /// The core's rate index and channel configuration, and whether SBR
    /// is named, from an AudioSpecificConfig
    fn read_config(config: &[u8]) -> Option<(u32, u32, bool)> {
        let mut bits = BitReaderLtr::new(config);
        let object_type = bits.read_bits_leq32(5).ok()?;
        // (An explicit 24-bit rate, index 15, falls out of the table)
        let rate_index = bits.read_bits_leq32(4).ok()?;
        let channels = bits.read_bits_leq32(4).ok()?;
        match object_type {
            AOT_AAC_LC => Some((rate_index, channels, false)),
            AOT_SBR | AOT_PS => {
                // SBR's rate, then the core's own object type
                if bits.read_bits_leq32(4).ok()? == 15 {
                    bits.read_bits_leq32(24).ok()?;
                }
                (bits.read_bits_leq32(5).ok()? == AOT_AAC_LC)
                    .then_some((rate_index, channels, true))
            }
            _ => None,
        }
    }

    /// The AAC-LC core alone
    fn core(&self) -> Format {
        Format {
            id: kAudioFormatMPEG4AAC,
            rate: self.rate,
            channels: u32::from(self.header.channels),
            frames_per_packet: 1024,
        }
    }

    /// What to decode as when the parser hasn't said: what the config
    /// names, as the parser would list it
    fn guess(&self) -> Vec<Format> {
        if !self.sbr {
            return Vec::new();
        }
        let format = |id, channels| Format {
            id,
            rate: self.rate * 2,
            channels,
            frames_per_packet: 2048,
        };
        match self.header.channels {
            1 => vec![
                format(kAudioFormatMPEG4AAC_HE_V2, 2),
                format(kAudioFormatMPEG4AAC_HE, 1),
            ],
            _ => vec![format(kAudioFormatMPEG4AAC_HE, 2)],
        }
    }
}

/// The name of a format for the log
fn format_name(id: u32) -> &'static str {
    match id {
        kAudioFormatMPEG4AAC_HE => "HE-AAC",
        kAudioFormatMPEG4AAC_HE_V2 => "HE-AAC v2",
        _ => "AAC-LC",
    }
}

/// One stream decoded by AudioToolbox
struct AppleDecoder {
    params: AudioCodecParameters,
    declared: Declared,
    /// The decoder in use: the AAC-LC one until the stream's kind is
    /// known, then the one for it
    converter: Converter,
    /// Whether the stream's kind is known
    decided: bool,
    /// The parser, while it reads the first frames
    detector: Option<Detector>,
    /// The packets it has read without an answer, to decode once it has
    held: Vec<Box<[u8]>>,
    /// Packets decoded as plain AAC since the stream was last looked at
    since_look: u32,
    buf: AudioBuffer<f32>,
    pcm: Box<[f32]>,
}

impl AppleDecoder {
    fn open(params: &AudioCodecParameters) -> Result<Self> {
        let Some(declared) = Declared::of(params) else {
            return unsupported_error("aac: not mono or stereo AAC at a standard rate");
        };
        // Made now, so that a system that can't decode AAC at all says so
        // before the first packet
        let core = declared.core();
        let converter = match Converter::new(core) {
            Ok(converter) => converter,
            Err(status) => {
                log::debug!("AudioConverterNew: {status}");
                return unsupported_error("aac: AudioToolbox could not make a decoder");
            }
        };
        let frames = MAX_UNDECIDED * MAX_PACKET_FRAMES;
        Ok(Self {
            params: params.clone(),
            declared,
            converter,
            decided: false,
            detector: None,
            held: Vec::new(),
            since_look: 0,
            buf: AudioBuffer::new(Self::spec(core), frames),
            pcm: vec![0.0; frames * 2].into_boxed_slice(),
        })
    }

    fn spec(format: Format) -> AudioSpec {
        let channels = match format.channels {
            1 => CHANNEL_LAYOUT_MONO,
            _ => CHANNEL_LAYOUT_STEREO,
        };
        AudioSpec::new(format.rate, channels)
    }

    /// What the parser makes of `packet`, after the ones it has read:
    /// the stream's formats, or `None` if it can't say yet
    fn look(&mut self, packet: &[u8]) -> Option<Vec<Format>> {
        let frame = self.declared.header.frame(packet)?;
        if self.detector.is_none() {
            self.detector = Detector::new();
        }
        self.detector.as_mut()?.feed(&frame)
    }

    /// Decode as the first of `formats` that a decoder can be made for,
    /// or else as the core
    fn settle(&mut self, formats: Vec<Format>) {
        self.decided = true;
        self.detector = None;
        self.since_look = 0;
        for format in formats {
            if format == self.converter.format() {
                return;
            }
            match Converter::new(format) {
                Ok(converter) => {
                    log::info!(
                        "AAC: AudioToolbox decodes {} at {} Hz, {} ch",
                        format_name(format.id),
                        format.rate,
                        format.channels
                    );
                    self.converter = converter;
                    return;
                }
                Err(status) => log::warn!(
                    "AAC: AudioToolbox has no {} decoder ({status})",
                    format_name(format.id)
                ),
            }
        }
    }

    /// A stream that plays as plain AAC, looked at again now and then:
    /// moves to the HE-AAC decoder if the frames have grown an SBR
    /// payload. (The other way needs nothing: the HE-AAC decoders play
    /// plain AAC too, at the rate they were made for.)
    fn look_again(&mut self, packet: &[u8]) {
        self.since_look += 1;
        if self.converter.format().id != kAudioFormatMPEG4AAC || self.since_look < LOOK_AGAIN_AFTER
        {
            return;
        }
        self.since_look = 0;
        self.detector = None;
        let formats = self.look(packet);
        self.detector = None;
        if let Some(formats) = formats {
            if formats
                .first()
                .is_some_and(|best| best.id != kAudioFormatMPEG4AAC)
            {
                self.settle(formats);
            }
        }
    }

    /// Decode `held` and then `packet` into `buf`
    fn decode(&mut self, packet: &PacketRef) -> Result<GenericAudioBufferRef<'_>> {
        let format = self.converter.format();
        let channels = format.channels as usize;
        let mut frames = 0;
        let mut failed = None;
        let held = std::mem::take(&mut self.held);
        let count = held.len() + 1;
        for data in held.iter().map(|held| &**held).chain([packet.data]) {
            let room = &mut self.pcm[frames * channels..];
            if room.len() < MAX_PACKET_FRAMES * channels {
                break;
            }
            match self.converter.decode(data, room) {
                Ok(decoded) => frames += decoded,
                Err(status) => {
                    // The decoder is of no use after an error until reset
                    self.converter.reset();
                    failed = Some(status);
                }
            }
        }

        let spec = Self::spec(format);
        if self.buf.spec() != &spec {
            self.buf = AudioBuffer::new(spec, MAX_UNDECIDED * MAX_PACKET_FRAMES);
        }
        self.buf.clear();
        if let (Some(status), 0) = (failed, frames) {
            log::debug!("AudioConverterFillComplexBuffer: {status}");
            return decode_error("aac: AudioToolbox could not decode the frame");
        }
        self.buf.render_uninit(Some(frames));
        let pcm = &self.pcm[..frames * channels];
        self.buf.copy_from_slice_interleaved(&pcm);
        if count == 1 {
            let trim_start = (packet.trim_start.get() as usize).min(frames);
            let trim_end = (packet.trim_end.get() as usize).min(frames - trim_start);
            self.buf.trim(trim_start, trim_end);
        }
        Ok(self.buf.as_generic_audio_buffer_ref())
    }
}

impl AudioDecoder for AppleDecoder {
    fn reset(&mut self) {
        // A jump in the stream: what follows may be another kind
        self.converter.reset();
        self.decided = false;
        self.detector = None;
        self.held.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        &AppleAac::supported_codecs()[0].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef) -> Result<GenericAudioBufferRef<'_>> {
        if self.decided {
            self.look_again(packet.data);
        } else {
            match self.look(packet.data) {
                Some(formats) => self.settle(formats),
                None if self.held.len() + 1 < MAX_UNDECIDED => {
                    // Nothing to play yet: the packet is decoded with the
                    // next ones, when the parser knows what they are
                    self.held.push(packet.data.into());
                    self.buf.clear();
                    return Ok(self.buf.as_generic_audio_buffer_ref());
                }
                None => self.settle(self.declared.guess()),
            }
        }
        self.decode(packet)
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

/// AAC and AAC+ by macOS's AudioToolbox. A stream it turns down goes to
/// libfdk-aac if that loads, or else to the built-in decoder.
pub(super) struct AppleAac(AppleDecoder);

impl AudioDecoder for AppleAac {
    fn reset(&mut self) {
        self.0.reset();
    }

    fn codec_info(&self) -> &CodecInfo {
        self.0.codec_info()
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        self.0.codec_params()
    }

    fn decode_ref(&mut self, packet: &PacketRef) -> Result<GenericAudioBufferRef<'_>> {
        self.0.decode_ref(packet)
    }

    fn finalize(&mut self) -> FinalizeResult {
        self.0.finalize()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.0.last_decoded()
    }
}

impl RegisterableAudioDecoder for AppleAac {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>> {
        match AppleDecoder::open(params) {
            Ok(decoder) => Ok(Box::new(Self(decoder))),
            Err(error) => {
                log::warn!("AAC: not playing with AudioToolbox ({error})");
                open_without_audio_toolbox(params, opts)
            }
        }
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(
            CODEC_ID_AAC,
            "aac",
            "Advanced Audio Coding",
            &[
                codec_profile!(CODEC_PROFILE_AAC_LC, "aac-lc", "Low Complexity"),
                codec_profile!(CODEC_PROFILE_AAC_HE, "aac-he", "High Efficiency"),
                codec_profile!(CODEC_PROFILE_AAC_HE_V2, "aac-he-v2", "High Efficiency V2"),
            ]
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adts_track(rate: u32, stereo: bool) -> AudioCodecParameters {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(rate)
            .with_channels(if stereo {
                CHANNEL_LAYOUT_STEREO
            } else {
                CHANNEL_LAYOUT_MONO
            });
        params
    }

    fn mp4_track(config: &[u8]) -> AudioCodecParameters {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(44_100)
            .with_extra_data(config.into());
        params
    }

    /// A config that names SBR (5) or PS (29) outright, over an AAC-LC
    /// core at 22.05 kHz that plays at 44.1 kHz
    fn explicit_config(object_type: u64, channels: u64) -> Vec<u8> {
        let bits: u64 = (object_type << 19) | (7 << 15) | (channels << 11) | (4 << 7) | (2 << 2);
        bits.to_be_bytes()[5..].to_vec()
    }

    #[test]
    fn an_adts_track_declares_its_core() {
        let declared = Declared::of(&adts_track(22_050, true)).unwrap();
        assert_eq!(
            declared.header,
            AdtsHeader {
                rate_index: 7,
                channels: 2
            }
        );
        assert_eq!((declared.rate, declared.sbr), (22_050, false));
        assert_eq!(declared.core().frames_per_packet, 1024);
        // Nothing says SBR: without the parser's word it is plain AAC
        assert_eq!(declared.guess(), Vec::new());
        // A rate outside AAC's table, and a track that says nothing
        assert_eq!(Declared::of(&adts_track(12_345, true)), None);
        let mut bare = AudioCodecParameters::new();
        bare.for_codec(CODEC_ID_AAC);
        assert_eq!(Declared::of(&bare), None);
    }

    #[test]
    fn an_mp4_config_declares_its_core() {
        // AAC-LC, 44.1 kHz, stereo
        let plain = Declared::of(&mp4_track(&[0x12, 0x10])).unwrap();
        assert_eq!(
            (plain.rate, plain.header.channels, plain.sbr),
            (44_100, 2, false)
        );
        // HE-AAC named outright: the core's rate, and what to try
        let he = Declared::of(&mp4_track(&explicit_config(5, 2))).unwrap();
        assert_eq!((he.rate, he.header.channels, he.sbr), (22_050, 2, true));
        let guess = he.guess();
        assert_eq!(guess.len(), 1);
        assert_eq!(
            (guess[0].id, guess[0].rate, guess[0].channels),
            (kAudioFormatMPEG4AAC_HE, 44_100, 2)
        );
        // HE-AAC v2: a mono core that plays in stereo
        let v2 = Declared::of(&mp4_track(&explicit_config(29, 1))).unwrap();
        assert_eq!((v2.header.channels, v2.sbr), (1, true));
        assert_eq!(
            (v2.guess()[0].id, v2.guess()[0].channels),
            (kAudioFormatMPEG4AAC_HE_V2, 2)
        );
    }

    #[test]
    fn what_this_decoder_does_not_play_is_left_to_the_others() {
        // 5.1 (channel configuration 6), USAC (object type 42, written
        // with the escape 31), an explicit rate, and a config cut short
        assert_eq!(Declared::of(&mp4_track(&[0x12, 0x30])), None);
        assert_eq!(Declared::of(&mp4_track(&[0xF9, 0x48, 0x80])), None);
        assert_eq!(
            Declared::of(&mp4_track(&[0x17, 0x80, 0x5D, 0xC0, 0x10])),
            None
        );
        assert_eq!(Declared::of(&mp4_track(&[0x12])), None);
        assert_eq!(Declared::of(&mp4_track(&explicit_config(5, 2)[..2])), None);
        assert!(AppleDecoder::open(&mp4_track(&[0x12, 0x30])).is_err());
    }

    #[test]
    fn a_decoder_is_made_for_each_kind_of_aac() {
        for id in [
            kAudioFormatMPEG4AAC,
            kAudioFormatMPEG4AAC_HE,
            kAudioFormatMPEG4AAC_HE_V2,
        ] {
            let sbr = id != kAudioFormatMPEG4AAC;
            let format = Format {
                id,
                rate: 44_100,
                channels: 2,
                frames_per_packet: if sbr { 2048 } else { 1024 },
            };
            let converter = Converter::new(format);
            assert!(
                converter.is_ok(),
                "{}: {:?}",
                format_name(id),
                converter.err()
            );
        }
        // No decoder for a format that is no audio
        let nothing = Format {
            id: sys::fourcc(b"none"),
            rate: 44_100,
            channels: 2,
            frames_per_packet: 1024,
        };
        assert!(Converter::new(nothing).is_err());
    }

    #[test]
    fn noise_is_not_audio_and_the_decoder_goes_on() {
        let mut decoder = AppleDecoder::open(&adts_track(44_100, true)).unwrap();
        let noise: Vec<u8> = (0..300u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        // Whatever it makes of each packet (nothing yet, an error, or
        // silence), it must not crash or run past its buffers
        for _ in 0..3 * MAX_UNDECIDED {
            let packet = PacketRef::new(0, 0u32.into(), 1024u32.into(), &noise);
            if let Ok(decoded) = decoder.decode_ref(&packet) {
                assert!(decoded.frames() <= MAX_UNDECIDED * MAX_PACKET_FRAMES);
            }
        }
        assert!(decoder.decided, "the kind is settled after a few packets");
        decoder.reset();
        assert!(!decoder.decided);
    }
}
