//! AudioToolbox's C interface, as far as decoding AAC needs it
//!
//! Written by hand from `AudioConverter.h`,
//! `AudioFileStream.h` and `CoreAudioBaseTypes.h`: nine C functions and
//! five plain structs, the same since Mac OS X 10.5. The framework is part
//! of macOS, so nothing is built or shipped for it.

#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::c_void;

pub(super) type OSStatus = i32;
pub(super) type AudioConverterRef = *mut c_void;
pub(super) type AudioFileStreamID = *mut c_void;

/// A four-character code, as the headers write their constants
pub(super) const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

pub(super) const noErr: OSStatus = 0;

pub(super) const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");
pub(super) const kAudioFormatMPEG4AAC: u32 = fourcc(b"aac ");
pub(super) const kAudioFormatMPEG4AAC_HE: u32 = fourcc(b"aach");
pub(super) const kAudioFormatMPEG4AAC_HE_V2: u32 = fourcc(b"aacp");

pub(super) const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub(super) const kAudioFormatFlagIsPacked: u32 = 1 << 3;

pub(super) const kAudioFileAAC_ADTSType: u32 = fourcc(b"adts");
pub(super) const kAudioFileStreamProperty_ReadyToProducePackets: u32 = fourcc(b"redy");
pub(super) const kAudioFileStreamProperty_FormatList: u32 = fourcc(b"flst");

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct AudioStreamPacketDescription {
    pub mStartOffset: i64,
    pub mVariableFramesInPacket: u32,
    pub mDataByteSize: u32,
}

#[repr(C)]
pub(super) struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

/// With its one buffer: all that interleaved audio needs
#[repr(C)]
pub(super) struct AudioBufferList {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct AudioFormatListItem {
    pub mASBD: AudioStreamBasicDescription,
    pub mChannelLayoutTag: u32,
}

pub(super) type AudioConverterComplexInputDataProc = unsafe extern "C" fn(
    converter: AudioConverterRef,
    ioNumberDataPackets: *mut u32,
    ioData: *mut AudioBufferList,
    outDataPacketDescription: *mut *mut AudioStreamPacketDescription,
    inUserData: *mut c_void,
) -> OSStatus;

pub(super) type AudioFileStream_PropertyListenerProc = unsafe extern "C" fn(
    inClientData: *mut c_void,
    inAudioFileStream: AudioFileStreamID,
    inPropertyID: u32,
    ioFlags: *mut u32,
);

pub(super) type AudioFileStream_PacketsProc = unsafe extern "C" fn(
    inClientData: *mut c_void,
    inNumberBytes: u32,
    inNumberPackets: u32,
    inInputData: *const c_void,
    inPacketDescriptions: *mut AudioStreamPacketDescription,
);

#[cfg_attr(target_os = "macos", link(name = "AudioToolbox", kind = "framework"))]
extern "C" {
    pub(super) fn AudioConverterNew(
        inSourceFormat: *const AudioStreamBasicDescription,
        inDestinationFormat: *const AudioStreamBasicDescription,
        outAudioConverter: *mut AudioConverterRef,
    ) -> OSStatus;
    pub(super) fn AudioConverterDispose(inAudioConverter: AudioConverterRef) -> OSStatus;
    pub(super) fn AudioConverterReset(inAudioConverter: AudioConverterRef) -> OSStatus;
    pub(super) fn AudioConverterFillComplexBuffer(
        inAudioConverter: AudioConverterRef,
        inInputDataProc: AudioConverterComplexInputDataProc,
        inInputDataProcUserData: *mut c_void,
        ioOutputDataPacketSize: *mut u32,
        outOutputData: *mut AudioBufferList,
        outPacketDescription: *mut AudioStreamPacketDescription,
    ) -> OSStatus;

    pub(super) fn AudioFileStreamOpen(
        inClientData: *mut c_void,
        inPropertyListenerProc: AudioFileStream_PropertyListenerProc,
        inPacketsProc: AudioFileStream_PacketsProc,
        inFileTypeHint: u32,
        outAudioFileStream: *mut AudioFileStreamID,
    ) -> OSStatus;
    pub(super) fn AudioFileStreamParseBytes(
        inAudioFileStream: AudioFileStreamID,
        inDataByteSize: u32,
        inData: *const c_void,
        inFlags: u32,
    ) -> OSStatus;
    pub(super) fn AudioFileStreamGetPropertyInfo(
        inAudioFileStream: AudioFileStreamID,
        inPropertyID: u32,
        outPropertyDataSize: *mut u32,
        outWritable: *mut u8,
    ) -> OSStatus;
    pub(super) fn AudioFileStreamGetProperty(
        inAudioFileStream: AudioFileStreamID,
        inPropertyID: u32,
        ioPropertyDataSize: *mut u32,
        outPropertyData: *mut c_void,
    ) -> OSStatus;
    pub(super) fn AudioFileStreamClose(inAudioFileStream: AudioFileStreamID) -> OSStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_structs_are_laid_out_as_in_c() {
        assert_eq!(std::mem::size_of::<AudioStreamBasicDescription>(), 40);
        assert_eq!(std::mem::size_of::<AudioStreamPacketDescription>(), 16);
        assert_eq!(std::mem::size_of::<AudioBuffer>(), 16);
        assert_eq!(std::mem::size_of::<AudioBufferList>(), 24);
        assert_eq!(std::mem::size_of::<AudioFormatListItem>(), 48);
        assert_eq!(kAudioFormatMPEG4AAC, 0x6161_6320);
    }
}
