//! One `AudioConverter` that decodes AAC packets to interleaved floats

use std::ffi::c_void;
use std::ptr;

use super::sys::*;

/// What the input callback answers when it has handed over its one packet.
/// Any status of our own ends the call with what was decoded so far and
/// leaves the converter as it is.
const NO_MORE_INPUT: OSStatus = 1;

/// A compressed format the converter can be made for
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Format {
    /// `kAudioFormatMPEG4AAC`, `_HE` or `_HE_V2`
    pub id: u32,
    /// The rate the audio comes out at (for AAC+, twice the core's)
    pub rate: u32,
    /// The channels that come out (for HE-AAC v2, two from a mono core)
    pub channels: u32,
    /// Frames one packet decodes to: 1024, or 2048 with SBR
    pub frames_per_packet: u32,
}

impl Format {
    fn description(&self) -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            mSampleRate: f64::from(self.rate),
            mFormatID: self.id,
            mChannelsPerFrame: self.channels,
            mFramesPerPacket: self.frames_per_packet,
            ..Default::default()
        }
    }

    /// Interleaved 32-bit floats at the same rate and channels
    fn pcm(&self) -> AudioStreamBasicDescription {
        let bytes = 4 * self.channels;
        AudioStreamBasicDescription {
            mSampleRate: f64::from(self.rate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: bytes,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes,
            mChannelsPerFrame: self.channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }
}

/// The packet a call to the converter decodes, for its input callback
struct Input {
    data: *const u8,
    len: u32,
    taken: bool,
    description: AudioStreamPacketDescription,
}

/// The converter asks for input: one packet, once
unsafe extern "C" fn give_packet(
    _converter: AudioConverterRef,
    packets: *mut u32,
    data: *mut AudioBufferList,
    description: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> OSStatus {
    // SAFETY: `user` is the `Input` of the `decode` call that is running,
    // and the converter passes valid pointers for the rest
    unsafe {
        let input = &mut *user.cast::<Input>();
        if input.taken {
            *packets = 0;
            return NO_MORE_INPUT;
        }
        input.taken = true;
        input.description = AudioStreamPacketDescription {
            mStartOffset: 0,
            mVariableFramesInPacket: 0,
            mDataByteSize: input.len,
        };
        (*data).mNumberBuffers = 1;
        (*data).mBuffers[0] = AudioBuffer {
            mNumberChannels: 0,
            mDataByteSize: input.len,
            // Only read, though the type says otherwise
            mData: input.data.cast_mut().cast(),
        };
        if !description.is_null() {
            *description = &mut input.description;
        }
        *packets = 1;
        noErr
    }
}

/// An AudioToolbox decoder for one compressed format
pub(super) struct Converter {
    raw: AudioConverterRef,
    format: Format,
}

// SAFETY: a converter has no ties to the thread that made it; it is used
// through `&mut self` only (and in `drop`), one call at a time
unsafe impl Send for Converter {}
// SAFETY: nothing reaches the converter through `&self`
unsafe impl Sync for Converter {}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: made by `AudioConverterNew`, disposed of only here
        unsafe { AudioConverterDispose(self.raw) };
    }
}

impl Converter {
    /// A decoder for `format`. The status is AudioToolbox's, when it has
    /// no decoder for the format.
    pub(super) fn new(format: Format) -> Result<Self, OSStatus> {
        let (input, output) = (format.description(), format.pcm());
        let mut raw: AudioConverterRef = ptr::null_mut();
        // SAFETY: two valid descriptions and a place for the result
        let status = unsafe { AudioConverterNew(&input, &output, &mut raw) };
        if status != noErr || raw.is_null() {
            return Err(status);
        }
        Ok(Self { raw, format })
    }

    pub(super) fn format(&self) -> Format {
        self.format
    }

    /// Decode one packet (one access unit, without a header) into `pcm`,
    /// which has room for at least a packet's frames. Gives the number of
    /// frames decoded: none while the decoder starts up.
    pub(super) fn decode(&mut self, packet: &[u8], pcm: &mut [f32]) -> Result<usize, OSStatus> {
        let channels = self.format.channels as usize;
        let mut input = Input {
            data: packet.as_ptr(),
            len: packet.len() as u32,
            taken: false,
            description: AudioStreamPacketDescription::default(),
        };
        let mut frames = (pcm.len() / channels) as u32;
        let mut output = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: self.format.channels,
                mDataByteSize: frames * self.format.channels * 4,
                mData: pcm.as_mut_ptr().cast(),
            }],
        };
        // SAFETY: the converter is live; `input` and `output` outlive the
        // call, which is the only time the callback runs; `output` says
        // how much room `pcm` has
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.raw,
                give_packet,
                ptr::from_mut(&mut input).cast(),
                &mut frames,
                &mut output,
                ptr::null_mut(),
            )
        };
        match status {
            noErr | NO_MORE_INPUT => Ok((frames as usize).min(pcm.len() / channels)),
            error => Err(error),
        }
    }

    /// Forget the stream so far: after a packet that did not decode, or a
    /// jump in the stream
    pub(super) fn reset(&mut self) {
        // SAFETY: a plain call on a live converter
        unsafe { AudioConverterReset(self.raw) };
    }
}
