//! Which AAC decoder plays a stream
//!
//! AAC-LC is decoded by Symphonia's own decoder, which is part of the
//! program. AAC+ (HE-AAC v1 and v2: SBR and Parametric Stereo) needs the
//! Fraunhofer FDK AAC library. Its licence doesn't go with the GPL, so the
//! program doesn't contain it: with the `system-fdk` feature it looks for
//! the system's `libfdk-aac` when it first plays AAC, and uses it if it is
//! there and decodes HE-AAC. Otherwise the built-in decoder plays the
//! AAC-LC core of the stream: an AAC+ station still plays, at half the
//! bandwidth (and in mono for HE-AAC v2).
//!
//! Without the feature there is no FDK code in the program at all.

use std::fmt;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::well_known::profiles::CODEC_PROFILE_AAC_LC;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{
    CodecRegistry, RegisterableAudioDecoder, SupportedAudioCodec,
};
use symphonia::core::codecs::CodecInfo;
use symphonia::core::errors::Result;
use symphonia::core::io::{BitReaderLtr, ReadBitsLtr};
use symphonia::core::packet::PacketRef;
// The two macros name the crate they come from
use symphonia::core as symphonia_core;
use symphonia::core::{codec_profile, support_audio_codec};
use symphonia::default::codecs::AacDecoder as SymphoniaAac;

#[cfg(feature = "system-fdk")]
mod fdk;

/// The AAC decoder in use
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AacDecoderKind {
    /// The system's libfdk-aac, loaded at run time: AAC and AAC+
    FdkSystem,
    /// The decoder in the program: AAC-LC only
    BuiltIn,
}

impl AacDecoderKind {
    /// The name to show: "FDK (system)" or "built-in AAC-LC"
    pub fn label(self) -> &'static str {
        match self {
            Self::FdkSystem => "FDK (system)",
            Self::BuiltIn => "built-in AAC-LC",
        }
    }

    /// Whether AAC+ (HE-AAC v1 and v2) plays in full. If not, only its
    /// AAC-LC core does.
    pub fn decodes_he_aac(self) -> bool {
        self == Self::FdkSystem
    }
}

impl fmt::Display for AacDecoderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The AAC decoder this program plays AAC with.
///
/// The first call looks for the system's libfdk-aac (with the `system-fdk`
/// feature); the answer then stays the same for as long as the program
/// runs.
pub fn aac_decoder() -> AacDecoderKind {
    #[cfg(feature = "system-fdk")]
    if fdk::system().is_some() {
        return AacDecoderKind::FdkSystem;
    }
    AacDecoderKind::BuiltIn
}

/// The system library's version (FDK's AAC decoder module, such as
/// "3.2.0"), if it is in use
pub fn fdk_version() -> Option<&'static str> {
    #[cfg(feature = "system-fdk")]
    return fdk::system().map(|fdk| fdk.version());
    #[cfg(not(feature = "system-fdk"))]
    None
}

#[cfg(test)]
thread_local! {
    /// A test on this thread plays AAC as if libfdk-aac were missing
    static BUILT_IN_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with the built-in decoder only, as on a system without
/// libfdk-aac. It covers decoders made on this thread.
#[cfg(test)]
pub(super) fn with_built_in_only<T>(f: impl FnOnce() -> T) -> T {
    let before = BUILT_IN_ONLY.replace(true);
    let result = f();
    BUILT_IN_ONLY.set(before);
    result
}

/// The decoder that new AAC tracks get
pub(super) fn active() -> AacDecoderKind {
    #[cfg(test)]
    if BUILT_IN_ONLY.get() {
        return AacDecoderKind::BuiltIn;
    }
    aac_decoder()
}

/// Put the AAC decoder in use into `registry`, in place of Symphonia's
pub(super) fn register(registry: &mut CodecRegistry) {
    registry.register_audio_decoder::<BuiltInAac>();
    #[cfg(feature = "system-fdk")]
    if active() == AacDecoderKind::FdkSystem {
        registry.register_audio_decoder::<fdk::FdkAac>();
    }
}

/// Sampling rates by `samplingFrequencyIndex` (ISO/IEC 14496-3, 1.6.3.3)
pub(super) const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

const AOT_AAC_LC: u32 = 2;
/// Audio object types that name SBR outright: HE-AAC and HE-AAC v2
pub(super) const AOT_SBR: u32 = 5;
pub(super) const AOT_PS: u32 = 29;
/// The sampling rate index that says a 24-bit rate follows
const RATE_ESCAPE: u32 = 15;

/// An AudioSpecificConfig for AAC-LC at `rate` with the channel
/// configuration `channels` (1: mono, 2: stereo, ... 7: 7.1)
fn lc_config(rate: u32, channels: u32) -> Vec<u8> {
    let mut bits = u64::from(AOT_AAC_LC);
    let mut len = 5;
    let mut put = |value: u32, width: u32| {
        bits = (bits << width) | u64::from(value);
        len += width;
    };
    match AAC_SAMPLE_RATES.iter().position(|&r| r == rate) {
        Some(index) => put(index as u32, 4),
        None => {
            put(RATE_ESCAPE, 4);
            put(rate & 0xFF_FFFF, 24);
        }
    }
    put(channels & 0xF, 4);
    // GASpecificConfig: 1024 samples a frame, no core coder, no extension
    put(0, 3);
    let bytes = len.div_ceil(8);
    (bits << (bytes * 8 - len)).to_be_bytes()[8 - bytes as usize..].to_vec()
}

/// For a config that names SBR outright (HE-AAC, HE-AAC v2), the config of
/// its AAC-LC core: the same stream as a decoder without SBR can play it
fn core_config(config: &[u8]) -> Option<Vec<u8>> {
    let mut bits = BitReaderLtr::new(config);
    let object_type = bits.read_bits_leq32(5).ok()?;
    if object_type != AOT_SBR && object_type != AOT_PS {
        return None;
    }
    // The first rate is the core's (the one after it is SBR's output rate)
    let rate = match bits.read_bits_leq32(4).ok()? {
        RATE_ESCAPE => bits.read_bits_leq32(24).ok()?,
        index => *AAC_SAMPLE_RATES.get(index as usize)?,
    };
    let channels = bits.read_bits_leq32(4).ok()?;
    // Skip SBR's rate: what follows is the core's own object type
    if bits.read_bits_leq32(4).ok()? == RATE_ESCAPE {
        bits.read_bits_leq32(24).ok()?;
    }
    (bits.read_bits_leq32(5).ok()? == AOT_AAC_LC).then(|| lc_config(rate, channels))
}

/// Symphonia's AAC-LC decoder, which also plays the AAC-LC core of a
/// track that declares HE-AAC (Symphonia alone turns such a track down)
pub(super) struct BuiltInAac(SymphoniaAac);

impl BuiltInAac {
    fn open(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>> {
        let core = params.extra_data.as_deref().and_then(core_config);
        let decoder = match core {
            Some(core) => {
                let mut params = params.clone();
                params.with_extra_data(core.into_boxed_slice());
                SymphoniaAac::try_new(&params, opts)?
            }
            None => SymphoniaAac::try_new(params, opts)?,
        };
        Ok(Box::new(Self(decoder)))
    }
}

impl AudioDecoder for BuiltInAac {
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

impl RegisterableAudioDecoder for BuiltInAac {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>> {
        Self::open(params, opts)
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(
            CODEC_ID_AAC,
            "aac",
            "Advanced Audio Coding",
            &[codec_profile!(
                CODEC_PROFILE_AAC_LC,
                "aac-lc",
                "Low Complexity"
            )]
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_labels_are_the_ones_the_ui_shows() {
        assert_eq!(AacDecoderKind::FdkSystem.to_string(), "FDK (system)");
        assert_eq!(AacDecoderKind::BuiltIn.to_string(), "built-in AAC-LC");
        assert!(AacDecoderKind::FdkSystem.decodes_he_aac());
        assert!(!AacDecoderKind::BuiltIn.decodes_he_aac());
    }

    #[test]
    fn the_decoder_in_use_is_reported() {
        let kind = aac_decoder();
        assert_eq!(kind, aac_decoder(), "it stays the same");
        assert_eq!(fdk_version().is_some(), kind == AacDecoderKind::FdkSystem);
        assert_eq!(with_built_in_only(active), AacDecoderKind::BuiltIn);
        assert_eq!(active(), kind);
    }

    #[cfg(not(feature = "system-fdk"))]
    #[test]
    fn without_the_feature_the_decoder_is_built_in() {
        assert_eq!(aac_decoder(), AacDecoderKind::BuiltIn);
        assert_eq!(fdk_version(), None);
    }

    #[test]
    fn an_lc_config_is_written_as_mp4_has_it() {
        // AAC-LC, 44.1 kHz, stereo: the usual two bytes
        assert_eq!(lc_config(44_100, 2), [0x12, 0x10]);
        assert_eq!(lc_config(22_050, 1), [0x13, 0x88]);
        // A rate outside the table is written out in 24 bits
        let odd = lc_config(12_345, 2);
        assert_eq!(odd.len(), 5);
        let mut bits = BitReaderLtr::new(&odd);
        assert_eq!(bits.read_bits_leq32(5).unwrap(), AOT_AAC_LC);
        assert_eq!(bits.read_bits_leq32(4).unwrap(), RATE_ESCAPE);
        assert_eq!(bits.read_bits_leq32(24).unwrap(), 12_345);
        assert_eq!(bits.read_bits_leq32(4).unwrap(), 2);
    }

    /// A config that names SBR (5) or PS (29) outright, over an AAC-LC
    /// core at 22.05 kHz that plays at 44.1 kHz
    fn explicit_config(object_type: u64, channels: u64) -> Vec<u8> {
        let bits: u64 = (object_type << 19) | (7 << 15) | (channels << 11) | (4 << 7) | (2 << 2);
        bits.to_be_bytes()[5..].to_vec()
    }

    #[test]
    fn a_config_that_names_sbr_gives_its_core() {
        assert_eq!(core_config(&explicit_config(5, 2)), Some(vec![0x13, 0x90]));
        // HE-AAC v2: the core is mono
        assert_eq!(core_config(&explicit_config(29, 1)), Some(vec![0x13, 0x88]));
        // Plain AAC-LC is left as it is, and so is a config cut short
        assert_eq!(core_config(&[0x12, 0x10]), None);
        assert_eq!(core_config(&explicit_config(5, 2)[..2]), None);
        assert_eq!(core_config(&[]), None);
    }

    #[test]
    fn the_built_in_decoder_takes_a_track_that_declares_he_aac() {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(44_100)
            .with_extra_data(explicit_config(5, 2).into_boxed_slice());
        let opts = AudioDecoderOptions::default();
        assert!(
            SymphoniaAac::try_new(&params, &opts).is_err(),
            "Symphonia alone takes it now: the core config is no longer needed"
        );
        let decoder = BuiltInAac::open(&params, &opts).unwrap();
        // It plays the core: 22.05 kHz
        assert_eq!(decoder.codec_params().sample_rate, Some(22_050));
    }
}
