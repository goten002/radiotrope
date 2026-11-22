//! The system's libfdk-aac, loaded at run time
//!
//! A hand-written binding for the few decoder calls of `aacdecoder_lib.h`
//! that playing a stream needs, and a Symphonia decoder on top of them.
//! Nothing of FDK is compiled or linked into the program: the library is
//! opened by name when AAC is first played, and a system without it (or
//! with a build that has HE-AAC taken out, as Fedora's and newer Ubuntu's
//! `fdk-aac-free`) plays AAC with the built-in decoder.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::fmt;
use std::sync::OnceLock;

use libloading::Library;
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
use symphonia::core::packet::PacketRef;
// The two macros name the crate they come from
use symphonia::core as symphonia_core;
use symphonia::core::{codec_profile, support_audio_codec};

use super::{lc_config, BuiltInAac};

/// The names the library goes by. The number is its ABI version: this
/// binding is for version 2 (fdk-aac 2.0 and later).
#[cfg(all(unix, not(target_os = "macos")))]
const LIBRARY_NAMES: &[&str] = &["libfdk-aac.so.2"];
#[cfg(target_os = "macos")]
const LIBRARY_NAMES: &[&str] = &["libfdk-aac.2.dylib"];
/// MinGW and MSYS2 builds, then MSVC builds (CMake, vcpkg)
#[cfg(windows)]
const LIBRARY_NAMES: &[&str] = &["libfdk-aac-2.dll", "fdk-aac.dll", "libfdk-aac.dll"];

// --- aacdecoder_lib.h and FDK_audio.h, as far as they are used ---

/// `HANDLE_AACDECODER`
type Handle = *mut c_void;

/// `TT_MP4_RAW`: one access unit a call, configured out of band
const TRANSPORT_RAW: c_int = 0;

/// `AAC_DEC_OK`
const OK: c_int = 0;

/// `AAC_PCM_MAX_OUTPUT_CHANNELS`: more channels than this are mixed down
const PARAM_MAX_OUTPUT_CHANNELS: c_int = 0x0012;
/// `AAC_TPDEC_CLEAR_BUFFER`: drop the input not decoded yet
const PARAM_CLEAR_BUFFER: c_int = 0x0603;

/// `FDK_MODULE_ID`: the AAC decoder, and the SBR decoder that HE-AAC needs
const MODULE_NONE: c_int = 0;
const MODULE_AAC_DECODER: c_int = 3;
const MODULE_SBR_DECODER: c_int = 5;
/// Room for more modules than any version lists (`FDK_MODULE_LAST` is 39)
const MODULE_SLOTS: usize = 64;

/// The most samples one frame decodes to: 4096 a channel (xHE-AAC), stereo
const MAX_FRAME_SAMPLES: usize = 8192;

/// `LIB_INFO`: one module of the library
#[repr(C)]
struct LibInfo {
    title: *const c_char,
    build_date: *const c_char,
    build_time: *const c_char,
    module_id: c_int,
    version: c_int,
    flags: c_uint,
    version_str: [c_char; 32],
}

/// The start of `CStreamInfo`, up to the last field read here. The library
/// owns it; it is only read through a pointer.
#[repr(C)]
struct StreamInfo {
    /// The rate of the decoded audio (after SBR)
    sample_rate: c_int,
    /// Samples a channel in a decoded frame
    frame_size: c_int,
    /// Channels of the decoded audio
    num_channels: c_int,
}

type OpenFn = unsafe extern "C" fn(transport: c_int, layers: c_uint) -> Handle;
type CloseFn = unsafe extern "C" fn(decoder: Handle);
type ConfigRawFn =
    unsafe extern "C" fn(decoder: Handle, config: *mut *mut u8, length: *const c_uint) -> c_int;
type FillFn = unsafe extern "C" fn(
    decoder: Handle,
    buffer: *mut *mut u8,
    size: *const c_uint,
    valid: *mut c_uint,
) -> c_int;
type DecodeFrameFn =
    unsafe extern "C" fn(decoder: Handle, pcm: *mut i16, samples: c_int, flags: c_uint) -> c_int;
type GetStreamInfoFn = unsafe extern "C" fn(decoder: Handle) -> *const StreamInfo;
type SetParamFn = unsafe extern "C" fn(decoder: Handle, param: c_int, value: c_int) -> c_int;
type GetLibInfoFn = unsafe extern "C" fn(info: *mut LibInfo) -> c_int;

/// Why the system library is not in use
#[derive(Debug)]
pub(super) enum LoadError {
    /// No library by any of the names: the last error of the loader
    NotFound(String),
    /// A library without one of the calls: not libfdk-aac 2
    MissingSymbol(&'static str),
    /// A library that reports no AAC decoder
    NoDecoder,
    /// A build with SBR taken out (`fdk-aac-free`): no HE-AAC
    NoHeAac,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(error) => write!(f, "libfdk-aac not found: {error}"),
            Self::MissingSymbol(name) => write!(f, "libfdk-aac has no {name}"),
            Self::NoDecoder => f.write_str("libfdk-aac has no AAC decoder"),
            Self::NoHeAac => {
                f.write_str("the system's libfdk-aac is a build without HE-AAC (fdk-aac-free)")
            }
        }
    }
}

/// libfdk-aac, loaded: the decoder calls
pub(super) struct Fdk {
    open: OpenFn,
    close: CloseFn,
    config_raw: ConfigRawFn,
    fill: FillFn,
    decode_frame: DecodeFrameFn,
    get_stream_info: GetStreamInfoFn,
    set_param: SetParamFn,
    version: String,
    /// Keeps the calls above loaded
    _library: Library,
}

impl Fdk {
    /// Load the library by its usual names
    fn load() -> std::result::Result<Self, LoadError> {
        let mut last = String::new();
        for name in LIBRARY_NAMES {
            match open_library(name) {
                Ok(library) => return Self::from_library(library),
                Err(error) => last = error.to_string(),
            }
        }
        Err(LoadError::NotFound(last))
    }

    /// Take the calls out of `library`, if it is a libfdk-aac that decodes
    /// HE-AAC
    fn from_library(library: Library) -> std::result::Result<Self, LoadError> {
        /// # Safety
        /// `T` is the type the library declares `name` with
        unsafe fn symbol<T: Copy>(
            library: &Library,
            name: &'static str,
        ) -> std::result::Result<T, LoadError> {
            library
                .get::<T>(name.as_bytes())
                .map(|symbol| *symbol)
                .map_err(|_| LoadError::MissingSymbol(name))
        }

        // SAFETY: the types are those of aacdecoder_lib.h, the same since
        // fdk-aac 2.0 (the library's ABI version 2)
        let (fdk, get_lib_info) = unsafe {
            let get_lib_info: GetLibInfoFn = symbol(&library, "aacDecoder_GetLibInfo")?;
            let fdk = Self {
                open: symbol(&library, "aacDecoder_Open")?,
                close: symbol(&library, "aacDecoder_Close")?,
                config_raw: symbol(&library, "aacDecoder_ConfigRaw")?,
                fill: symbol(&library, "aacDecoder_Fill")?,
                decode_frame: symbol(&library, "aacDecoder_DecodeFrame")?,
                get_stream_info: symbol(&library, "aacDecoder_GetStreamInfo")?,
                set_param: symbol(&library, "aacDecoder_SetParam")?,
                version: String::new(),
                _library: library,
            };
            (fdk, get_lib_info)
        };

        // What the library is made of. It fills the free slots (module
        // NONE, which is zero) with the modules it was built with.
        // SAFETY: all zeroes is a valid LibInfo (null pointers, NONE), and
        // the library writes only into free slots of the array
        let modules = unsafe {
            let mut modules: [LibInfo; MODULE_SLOTS] = std::mem::zeroed();
            get_lib_info(modules.as_mut_ptr());
            modules
        };
        let module = |id: c_int| {
            modules
                .iter()
                .take_while(|module| module.module_id != MODULE_NONE)
                .find(|module| module.module_id == id)
        };
        let decoder = module(MODULE_AAC_DECODER).ok_or(LoadError::NoDecoder)?;
        if module(MODULE_SBR_DECODER).is_none() {
            return Err(LoadError::NoHeAac);
        }
        let version: Vec<u8> = decoder
            .version_str
            .iter()
            .map(|&c| c as u8)
            .take_while(|&c| c != 0)
            .collect();
        Ok(Self {
            version: String::from_utf8_lossy(&version).into_owned(),
            ..fdk
        })
    }

    /// The version of the library's AAC decoder module, such as "3.2.0"
    pub(super) fn version(&self) -> &str {
        &self.version
    }
}

/// Open a library by name, where the system keeps its libraries
#[cfg(not(windows))]
fn open_library(name: &str) -> std::result::Result<Library, libloading::Error> {
    // SAFETY: loading a library runs its initializers; libfdk-aac has none
    // that do anything but set up the library itself
    unsafe { Library::new(name) }
}

/// Open a library by name: next to the program, or in the system folder.
/// Not from the current folder or the PATH, where anyone could have left a
/// DLL by that name.
#[cfg(windows)]
fn open_library(name: &str) -> std::result::Result<Library, libloading::Error> {
    use libloading::os::windows::{Library as WindowsLibrary, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS};
    // SAFETY: as above
    unsafe { WindowsLibrary::load_with_flags(name, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) }
        .map(Library::from)
}

/// The channel configuration for a channel count, as ADTS has it
fn channel_config(count: usize) -> Option<u32> {
    match count {
        1..=6 => Some(count as u32),
        8 => Some(7),
        _ => None,
    }
}

/// The config an ADTS track stands for. ADTS has no AudioSpecificConfig:
/// its headers give the rate and the channels, and the frames are AAC-LC,
/// with SBR and PS (if any) signalled inside them.
fn adts_config(params: &AudioCodecParameters) -> Option<Vec<u8>> {
    let rate = params.sample_rate.filter(|&rate| rate > 0)?;
    let channels = channel_config(params.channels.as_ref()?.count())?;
    Some(lc_config(rate, channels))
}

/// The system's libfdk-aac, if there is one that decodes HE-AAC. Looked
/// for once: the first call loads it.
pub(super) fn system() -> Option<&'static Fdk> {
    static SYSTEM: OnceLock<Option<Fdk>> = OnceLock::new();
    SYSTEM
        .get_or_init(|| match Fdk::load() {
            Ok(fdk) => {
                log::info!(
                    "AAC decoder: FDK (system), libfdk-aac decoder {}",
                    fdk.version
                );
                Some(fdk)
            }
            Err(error) => {
                log::info!("AAC decoder: built-in AAC-LC ({error})");
                None
            }
        })
        .as_ref()
}

/// What a failed call is reported as
fn error_text(code: c_int) -> &'static str {
    match code {
        0x1001 => "aac: FDK lost its place in the stream",
        0x1002 => "aac: FDK ran out of data in the frame",
        0x2000..=0x2FFF => "aac: FDK cannot play this kind of AAC",
        _ => "aac: FDK could not decode the frame",
    }
}

/// One stream decoded by the system's libfdk-aac
struct FdkDecoder {
    fdk: &'static Fdk,
    handle: Handle,
    params: AudioCodecParameters,
    buf: AudioBuffer<i16>,
    pcm: Box<[i16]>,
}

// SAFETY: a decoder instance has no ties to the thread that made it, and
// the handle is only used through `&mut self` (and in `drop`)
unsafe impl Send for FdkDecoder {}
// SAFETY: nothing reaches the handle through `&self`
unsafe impl Sync for FdkDecoder {}

impl Drop for FdkDecoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from `open` and is closed only here
        unsafe { (self.fdk.close)(self.handle) }
    }
}

impl FdkDecoder {
    fn open(fdk: &'static Fdk, params: &AudioCodecParameters) -> Result<Self> {
        // MP4 and fMP4 (HLS) tracks come with their config; ADTS (ICY
        // stations, HLS TS segments) has it in each frame's header, which
        // the demuxer has taken off
        let mut config = match &params.extra_data {
            Some(config) => config.to_vec(),
            None => match adts_config(params) {
                Some(config) => config,
                None => return unsupported_error("aac: the rate and channels are required"),
            },
        };

        // SAFETY: plain calls with valid arguments. A null handle is
        // checked for, and from here on `decoder` closes it when dropped.
        let handle = unsafe { (fdk.open)(TRANSPORT_RAW, 1) };
        if handle.is_null() {
            return unsupported_error("aac: FDK could not make a decoder");
        }
        let rate = params
            .sample_rate
            .filter(|&rate| rate > 0)
            .unwrap_or(44_100);
        let decoder = Self {
            fdk,
            handle,
            params: params.clone(),
            buf: AudioBuffer::new(AudioSpec::new(rate, CHANNEL_LAYOUT_STEREO), 0),
            pcm: vec![0; MAX_FRAME_SAMPLES].into_boxed_slice(),
        };

        let mut config_ptr = config.as_mut_ptr();
        let config_len = config.len() as c_uint;
        // SAFETY: one buffer of `config_len` bytes, which the call reads
        // and doesn't keep
        let error = unsafe { (fdk.config_raw)(handle, &mut config_ptr, &config_len) };
        if error != OK {
            log::debug!("aacDecoder_ConfigRaw: {error:#06x}");
            return unsupported_error("aac: FDK cannot play this kind of AAC");
        }
        // Surround comes out as stereo, which is what gets played. Left
        // as it is if the library can't mix down: such a stream then
        // fails to decode.
        // SAFETY: a plain call on a live handle
        unsafe { (fdk.set_param)(handle, PARAM_MAX_OUTPUT_CHANNELS, 2) };
        Ok(decoder)
    }

    /// Drop the input the library still holds, after a frame it could not
    /// decode or a jump in the stream
    fn clear_input(&mut self) {
        // SAFETY: a plain call on a live handle
        unsafe { (self.fdk.set_param)(self.handle, PARAM_CLEAR_BUFFER, 1) };
    }

    fn failed(&mut self, code: c_int) -> Result<GenericAudioBufferRef<'_>> {
        self.clear_input();
        self.buf.clear();
        decode_error(error_text(code))
    }
}

impl AudioDecoder for FdkDecoder {
    fn reset(&mut self) {
        self.clear_input();
    }

    fn codec_info(&self) -> &CodecInfo {
        &FdkAac::supported_codecs()[0].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef) -> Result<GenericAudioBufferRef<'_>> {
        let fdk = self.fdk;
        // The library takes the input as mutable, but only reads it
        let mut input = packet.data.as_ptr().cast_mut();
        let size = packet.data.len() as c_uint;
        let mut left = size;
        // SAFETY: one buffer of `size` bytes, copied into the library's own
        let error = unsafe { (fdk.fill)(self.handle, &mut input, &size, &mut left) };
        if error != OK || left != 0 {
            // A frame larger than the library's input buffer is no AAC
            return self.failed(error);
        }
        // SAFETY: `pcm` has room for the number of samples given
        let error = unsafe {
            (fdk.decode_frame)(
                self.handle,
                self.pcm.as_mut_ptr(),
                self.pcm.len() as c_int,
                0,
            )
        };
        if error != OK {
            return self.failed(error);
        }

        // SAFETY: the library's own struct, valid while the decoder is
        // open, and laid out as `StreamInfo` at its start
        let info = unsafe { (fdk.get_stream_info)(self.handle).as_ref() };
        let Some(info) = info else {
            return self.failed(OK);
        };
        let (rate, frames) = (info.sample_rate, info.frame_size);
        let channels = match info.num_channels {
            1 => CHANNEL_LAYOUT_MONO,
            2 => CHANNEL_LAYOUT_STEREO,
            _ => return self.failed(0x2000),
        };
        let samples = frames as usize * channels.count();
        if rate <= 0 || frames <= 0 || samples > self.pcm.len() {
            return self.failed(OK);
        }
        let (rate, frames) = (rate as u32, frames as usize);

        // SBR doubles the rate, and PS makes mono stereo: what comes out
        // is known from the first frame on, and can change in mid-stream
        let spec = self.buf.spec();
        if spec.rate() != rate || spec.channels() != &channels || self.buf.capacity() != frames {
            self.buf = AudioBuffer::new(AudioSpec::new(rate, channels), frames);
        }
        self.buf.clear();
        self.buf.render_uninit(None);
        let pcm = &self.pcm[..samples];
        self.buf.copy_from_slice_interleaved(&pcm);
        let trim_start = (packet.trim_start.get() as usize).min(frames);
        let trim_end = (packet.trim_end.get() as usize).min(frames - trim_start);
        self.buf.trim(trim_start, trim_end);
        Ok(self.buf.as_generic_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

/// AAC and AAC+ by the system's libfdk-aac. A stream the library turns
/// down goes to the built-in decoder.
pub(super) struct FdkAac(FdkDecoder);

impl AudioDecoder for FdkAac {
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

impl RegisterableAudioDecoder for FdkAac {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>> {
        let opened = match system() {
            Some(fdk) => FdkDecoder::open(fdk, params),
            None => unsupported_error("aac: libfdk-aac is not loaded"),
        };
        match opened {
            Ok(decoder) => Ok(Box::new(Self(decoder))),
            Err(error) => {
                log::warn!("AAC: playing with the built-in decoder ({error})");
                BuiltInAac::try_registry_new(params, opts)
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

    #[test]
    fn channel_counts_map_to_configurations() {
        assert_eq!(channel_config(1), Some(1));
        assert_eq!(channel_config(6), Some(6));
        assert_eq!(channel_config(8), Some(7));
        assert_eq!(channel_config(0), None);
        assert_eq!(channel_config(7), None);
    }

    #[test]
    fn an_adts_track_gets_a_config() {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC);
        assert_eq!(adts_config(&params), None);
        params
            .with_sample_rate(44_100)
            .with_channels(CHANNEL_LAYOUT_STEREO);
        assert_eq!(adts_config(&params), Some(vec![0x12, 0x10]));
    }

    #[test]
    fn a_missing_library_is_reported_not_loaded() {
        let error = open_library("libfdk-aac-that-is-not-there.so.2").unwrap_err();
        let error = LoadError::NotFound(error.to_string());
        assert!(error.to_string().starts_with("libfdk-aac not found: "));
    }

    #[test]
    fn a_library_that_is_not_fdk_is_turned_down() {
        // Any other library the test machine is sure to have
        #[cfg(target_os = "linux")]
        let other = "libm.so.6";
        #[cfg(target_os = "macos")]
        let other = "libSystem.B.dylib";
        #[cfg(windows)]
        let other = "kernel32.dll";
        let library = open_library(other).unwrap();
        assert!(matches!(
            Fdk::from_library(library),
            Err(LoadError::MissingSymbol("aacDecoder_GetLibInfo"))
        ));
    }

    /// Set `RADIOTROPE_TEST_FDK_FREE` to the path of an `fdk-aac-free`
    /// build (Fedora's, or Ubuntu's since 25.10) to check that it is told
    /// apart from the full library
    #[test]
    fn a_build_without_he_aac_is_turned_down() {
        let Some(path) = std::env::var_os("RADIOTROPE_TEST_FDK_FREE") else {
            eprintln!("skipped: RADIOTROPE_TEST_FDK_FREE is not set");
            return;
        };
        // SAFETY: a libfdk-aac, as in `open_library`
        let library = unsafe { Library::new(&path) }.unwrap();
        assert!(matches!(
            Fdk::from_library(library),
            Err(LoadError::NoHeAac)
        ));
    }

    #[test]
    fn the_system_library_reports_its_version() {
        let Some(fdk) = system() else {
            eprintln!("skipped: no libfdk-aac with HE-AAC on this system");
            return;
        };
        let version = fdk.version();
        assert!(
            version.split('.').count() >= 2 && version.starts_with(|c: char| c.is_ascii_digit()),
            "version {version:?}"
        );
    }

    #[test]
    fn a_config_the_library_turns_down_is_an_error() {
        let Some(fdk) = system() else {
            eprintln!("skipped: no libfdk-aac with HE-AAC on this system");
            return;
        };
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC);
        // No config, and nothing to make one from
        assert!(FdkDecoder::open(fdk, &params).is_err());
        // Object type 0 is no audio at all
        params.with_extra_data(Box::new([0x00, 0x00]));
        assert!(FdkDecoder::open(fdk, &params).is_err());
    }
}
