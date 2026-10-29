//! The small C surface not covered by the Objective-C capture bindings.

use anyhow::{bail, Result};
pub use objc2_core_media::CMTime;
use std::ffi::c_void;
use std::ptr;

pub type Ref = *const c_void;
pub const NV12: u32 = u32::from_be_bytes(*b"420v");

/// Owns one Create/Copy reference, including on an error path.
pub struct Owned(pub Ref);
impl Owned {
    pub fn new(value: Ref) -> Result<Self> {
        if value.is_null() {
            bail!("Apple framework returned a null object");
        }
        Ok(Self(value))
    }
    pub fn retain(value: Ref) -> Self {
        // SAFETY: callers supply a live CF object; the returned owner balances this retain.
        Self(unsafe { CFRetain(value) })
    }
}
// SAFETY: the CF objects used here are immutable buffers or sessions exclusively owned by a pipeline.
unsafe impl Send for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: Owned always holds exactly one nonnull Create/Copy/retain reference.
        unsafe { CFRelease(self.0) }
    }
}

pub fn check(status: i32, operation: &str) -> Result<()> {
    if status != 0 {
        bail!("{operation} failed ({status})");
    }
    Ok(())
}
pub fn number(value: i64) -> Owned {
    // SAFETY: CFNumber copies the signed 64-bit value before returning.
    Owned::new(unsafe { CFNumberCreate(ptr::null(), 4, (&value as *const i64).cast()) }).unwrap()
}
pub fn dictionary(entries: &[(Ref, Ref)]) -> Owned {
    let keys: Vec<_> = entries.iter().map(|e| e.0).collect();
    let values: Vec<_> = entries.iter().map(|e| e.1).collect();
    // SAFETY: the arrays have equal lengths and the CF callbacks retain each key and value.
    Owned::new(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    })
    .unwrap()
}
pub fn time(value: i64, fps: u32) -> CMTime {
    CMTime {
        value,
        timescale: fps as i32,
        flags: objc2_core_media::CMTimeFlags(1),
        epoch: 0,
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRetain(value: Ref) -> Ref;
    pub fn CFRelease(value: Ref);
    pub static kCFBooleanTrue: Ref;
    pub static kCFBooleanFalse: Ref;
    pub static kCFTypeDictionaryKeyCallBacks: c_void;
    pub static kCFTypeDictionaryValueCallBacks: c_void;
    pub static kCFTypeArrayCallBacks: c_void;
    pub fn CFNumberCreate(allocator: Ref, kind: isize, value: Ref) -> Ref;
    pub fn CFNumberGetValue(number: Ref, kind: isize, out: *mut c_void) -> bool;
    pub fn CFBooleanGetValue(value: Ref) -> bool;
    pub fn CFDictionaryCreate(
        allocator: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        key_callbacks: Ref,
        value_callbacks: Ref,
    ) -> Ref;
    pub fn CFDictionaryGetValue(dict: Ref, key: Ref) -> Ref;
    pub fn CFArrayCreate(allocator: Ref, values: *const Ref, count: isize, callbacks: Ref) -> Ref;
    pub fn CFArrayGetCount(array: Ref) -> isize;
    pub fn CFArrayGetValueAtIndex(array: Ref, index: isize) -> Ref;
}
#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    pub static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: Ref;
    pub static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: Ref;
    pub static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: Ref;
    pub static kVTCompressionPropertyKey_RealTime: Ref;
    pub static kVTCompressionPropertyKey_AllowFrameReordering: Ref;
    pub static kVTCompressionPropertyKey_ProfileLevel: Ref;
    pub static kVTCompressionPropertyKey_AverageBitRate: Ref;
    pub static kVTCompressionPropertyKey_DataRateLimits: Ref;
    pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: Ref;
    pub static kVTCompressionPropertyKey_ExpectedFrameRate: Ref;
    pub static kVTProfileLevel_HEVC_Main_AutoLevel: Ref;
    pub static kVTProfileLevel_H264_High_AutoLevel: Ref;
    pub static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: Ref;
    pub static kVTProfileLevel_H264_Baseline_AutoLevel: Ref;
    pub static kVTEncodeFrameOptionKey_ForceKeyFrame: Ref;
    pub fn VTCompressionSessionCreate(
        allocator: Ref,
        width: i32,
        height: i32,
        codec: u32,
        specification: Ref,
        attributes: Ref,
        compressed_allocator: Ref,
        callback: unsafe extern "C" fn(*mut c_void, *mut c_void, i32, u32, Ref),
        refcon: *mut c_void,
        out: *mut Ref,
    ) -> i32;
    pub fn VTSessionSetProperty(session: Ref, key: Ref, value: Ref) -> i32;
    pub fn VTSessionCopyProperty(session: Ref, key: Ref, allocator: Ref, out: *mut Ref) -> i32;
    pub fn VTCompressionSessionPrepareToEncodeFrames(session: Ref) -> i32;
    pub fn VTCompressionSessionEncodeFrame(
        session: Ref,
        image: Ref,
        pts: CMTime,
        duration: CMTime,
        properties: Ref,
        refcon: *mut c_void,
        flags: *mut u32,
    ) -> i32;
    pub fn VTCompressionSessionCompleteFrames(session: Ref, until: CMTime) -> i32;
    pub fn VTCompressionSessionInvalidate(session: Ref);
}
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    pub static kCMSampleAttachmentKey_NotSync: Ref;
    pub fn CMSampleBufferGetImageBuffer(sample: Ref) -> Ref;
    pub fn CMSampleBufferGetSampleAttachmentsArray(sample: Ref, create: bool) -> Ref;
    pub fn CMSampleBufferGetFormatDescription(sample: Ref) -> Ref;
    pub fn CMSampleBufferGetDataBuffer(sample: Ref) -> Ref;
    pub fn CMBlockBufferGetDataLength(block: Ref) -> usize;
    pub fn CMBlockBufferCopyDataBytes(
        block: Ref,
        offset: usize,
        length: usize,
        out: *mut u8,
    ) -> i32;
    pub fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format: Ref,
        index: usize,
        pointer: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        header: *mut i32,
    ) -> i32;
    pub fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        format: Ref,
        index: usize,
        pointer: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        header: *mut i32,
    ) -> i32;
    pub fn CMAudioFormatDescriptionGetStreamBasicDescription(format: Ref) -> *const AudioFormat;
    pub fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sample: Ref,
        size: *mut usize,
        list: *mut c_void,
        capacity: usize,
        allocator: Ref,
        memory_allocator: Ref,
        flags: u32,
        block: *mut Ref,
    ) -> i32;
}
#[repr(C)]
pub struct AudioFormat {
    pub rate: f64,
    pub format: u32,
    pub flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels: u32,
    pub bits: u32,
    pub reserved: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioBuffer {
    pub channels: u32,
    pub size: u32,
    pub data: *const f32,
}
#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    #[cfg(test)]
    pub static kCVPixelBufferIOSurfacePropertiesKey: Ref;
    #[cfg(test)]
    pub static kCVImageBufferYCbCrMatrixKey: Ref;
    pub static kCVImageBufferYCbCrMatrix_ITU_R_709_2: Ref;
    #[cfg(test)]
    pub fn CVPixelBufferCreate(
        allocator: Ref,
        width: usize,
        height: usize,
        format: u32,
        attributes: Ref,
        out: *mut Ref,
    ) -> i32;
    #[cfg(test)]
    pub fn CVPixelBufferLockBaseAddress(buffer: Ref, flags: u64) -> i32;
    #[cfg(test)]
    pub fn CVPixelBufferUnlockBaseAddress(buffer: Ref, flags: u64) -> i32;
    #[cfg(test)]
    pub fn CVPixelBufferGetBaseAddressOfPlane(buffer: Ref, plane: usize) -> *mut u8;
    #[cfg(test)]
    pub fn CVPixelBufferGetBytesPerRowOfPlane(buffer: Ref, plane: usize) -> usize;
    #[cfg(test)]
    pub fn CVBufferSetAttachment(buffer: Ref, key: Ref, value: Ref, mode: u32);
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGMainDisplayID() -> u32;
}
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    pub fn CMSampleBufferGetPresentationTimeStamp(sample: Ref) -> CMTime;
    pub fn CMClockGetHostTimeClock() -> Ref;
    pub fn CMClockGetTime(clock: Ref) -> CMTime;
}
#[cfg(test)]
extern "C" {
    pub fn CVPixelBufferGetIOSurface(image: Ref) -> Ref;
}
