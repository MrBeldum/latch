//! Hardware H.264 and HEVC decode on macOS through VideoToolbox, bound by
//! hand: a dozen C functions from CoreMedia, CoreVideo and VideoToolbox.
//!
//! Each access unit arrives in Annex B. Parameter sets go into a format
//! description; the slices are re-framed with 4-byte lengths (AVCC/HVCC) into
//! one sample buffer and decoded synchronously; `decode` also waits out any
//! frame the decoder held back, so the output callback has always run, on
//! this thread, before `decode` returns.

use super::{nal_units, Decoder, Frame};
use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;
use std::ptr;

type OSStatus = i32;
type CFTypeRef = *const c_void;
type CFAllocatorRef = CFTypeRef;
type CFDictionaryRef = CFTypeRef;
type CFStringRef = CFTypeRef;
type CFNumberRef = CFTypeRef;
type CMFormatDescriptionRef = CFTypeRef;
type CMBlockBufferRef = CFTypeRef;
type CMSampleBufferRef = CFTypeRef;
type CVImageBufferRef = CFTypeRef;
type VTDecompressionSessionRef = CFTypeRef;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

type OutputCallback =
    unsafe extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CVImageBufferRef, CMTime, CMTime);

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    callback: Option<OutputCallback>,
    refcon: *mut c_void,
}

const K_CF_NUMBER_SINT32: isize = 3;
const K_CV_PIXEL_FORMAT_420V: u32 = 0x3432_3076; // '420v', video range
const K_CV_PIXEL_FORMAT_420F: u32 = 0x3432_3066; // '420f', full range
const K_CV_LOCK_READ_ONLY: u64 = 1;

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {}
#[link(name = "CoreVideo", kind = "framework")]
extern "C" {}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {}
#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCFBooleanTrue: CFTypeRef;
    static kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: CFStringRef;
    static kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder: CFStringRef;
    static kVTDecompressionPropertyKey_RealTime: CFStringRef;
    fn CFBooleanGetValue(value: CFTypeRef) -> u8;
    fn VTSessionSetProperty(session: CFTypeRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTSessionCopyProperty(
        session: CFTypeRef,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        out: *mut CFTypeRef,
    ) -> OSStatus;

    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: isize,
        ptr: *const c_void,
    ) -> CFNumberRef;

    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_unit_header_length: i32,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_unit_header_length: i32,
        extensions: CFDictionaryRef,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source: *const c_void,
        destination: CMBlockBufferRef,
        offset: usize,
        length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format: CMFormatDescriptionRef,
        num_samples: isize,
        num_timing_entries: isize,
        timing: *const c_void,
        num_size_entries: isize,
        sizes: *const usize,
        out: *mut CMSampleBufferRef,
    ) -> OSStatus;

    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        format: CMFormatDescriptionRef,
        decoder_specification: CFDictionaryRef,
        destination_attributes: CFDictionaryRef,
        callback: *const VTDecompressionOutputCallbackRecord,
        out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample: CMSampleBufferRef,
        flags: u32,
        frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    fn VTDecompressionSessionCanAcceptFormatDescription(
        session: VTDecompressionSessionRef,
        format: CMFormatDescriptionRef,
    ) -> u8;

    fn CVPixelBufferLockBaseAddress(pb: CVImageBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: CVImageBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: CVImageBufferRef, plane: usize) -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: CVImageBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pb: CVImageBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pb: CVImageBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetPixelFormatType(pb: CVImageBufferRef) -> u32;
    fn CVPixelBufferGetPlaneCount(pb: CVImageBufferRef) -> usize;
}

pub struct VideoToolbox {
    hevc: bool,
    hardware: bool,
    format: CMFormatDescriptionRef,
    session: VTDecompressionSessionRef,
    /// Parameter sets the current format description was built from.
    params: Vec<Vec<u8>>,
    avcc: Vec<u8>,
}

unsafe impl Send for VideoToolbox {}

impl VideoToolbox {
    pub fn new(hevc: bool) -> Result<Self> {
        Ok(Self {
            hevc,
            hardware: false,
            format: ptr::null(),
            session: ptr::null(),
            params: Vec::new(),
            avcc: Vec::new(),
        })
    }

    fn nal_type(&self, nal: &[u8]) -> u8 {
        if self.hevc {
            (nal[0] >> 1) & 0x3f
        } else {
            nal[0] & 0x1f
        }
    }

    fn is_parameter_set(&self, t: u8) -> bool {
        if self.hevc {
            (32..=34).contains(&t)
        } else {
            t == 7 || t == 8
        }
    }

    /// Rebuild the format description (and, if needed, the session) from a
    /// fresh set of parameter NALs.
    fn set_parameters(&mut self, sets: Vec<Vec<u8>>) -> Result<()> {
        if sets == self.params && !self.format.is_null() {
            // A sleep/wake or GPU reset invalidates the session while the
            // parameter sets stay the same. Recreate rather than decoding
            // into a dead session until the user reconnects.
            if self.session.is_null() {
                self.create_session()?;
            }
            return Ok(());
        }
        let ptrs: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
        let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
        let mut fmt: CMFormatDescriptionRef = ptr::null();
        let status = unsafe {
            if self.hevc {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    ptr::null(),
                    sets.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    ptr::null(),
                    &mut fmt,
                )
            } else {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    ptr::null(),
                    sets.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut fmt,
                )
            }
        };
        if status != 0 || fmt.is_null() {
            bail!("format description failed ({status})");
        }
        unsafe {
            if !self.session.is_null()
                && VTDecompressionSessionCanAcceptFormatDescription(self.session, fmt) == 0
            {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
                self.session = ptr::null();
            }
            if !self.format.is_null() {
                CFRelease(self.format);
            }
        }
        self.format = fmt;
        self.params = sets;
        if self.session.is_null() {
            self.create_session()?;
        }
        Ok(())
    }

    fn create_session(&mut self) -> Result<()> {
        unsafe {
            // The stream is limited range (`COLOR_RANGE_LIMITED`).
            let pixel_format = K_CV_PIXEL_FORMAT_420V as i32;
            let number = CFNumberCreate(
                ptr::null(),
                K_CF_NUMBER_SINT32,
                &pixel_format as *const i32 as *const c_void,
            );
            let keys = [kCVPixelBufferPixelFormatTypeKey];
            let values = [number];
            let attrs = CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let spec = CFDictionaryCreate(
                ptr::null(),
                [kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder].as_ptr(),
                [kCFBooleanTrue].as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let record = VTDecompressionOutputCallbackRecord {
                callback: Some(output),
                refcon: ptr::null_mut(),
            };
            let mut session: VTDecompressionSessionRef = ptr::null();
            let status = VTDecompressionSessionCreate(
                ptr::null(),
                self.format,
                spec,
                attrs,
                &record,
                &mut session,
            );
            CFRelease(spec);
            CFRelease(attrs);
            CFRelease(number);
            if status != 0 || session.is_null() {
                bail!("VideoToolbox session failed ({status})");
            }
            let rc = VTSessionSetProperty(
                session,
                kVTDecompressionPropertyKey_RealTime,
                kCFBooleanTrue,
            );
            if rc != 0 {
                tracing::debug!("VideoToolbox realtime property: {rc}");
            }
            let mut value = ptr::null();
            self.hardware = VTSessionCopyProperty(
                session,
                kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                ptr::null(),
                &mut value,
            ) == 0
                && !value.is_null()
                && CFBooleanGetValue(value) != 0;
            if !value.is_null() {
                CFRelease(value);
            }
            self.session = session;
        }
        Ok(())
    }
}

impl VideoToolbox {
    fn drop_session(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
                self.session = ptr::null();
            }
        }
    }
}

impl Drop for VideoToolbox {
    fn drop(&mut self) {
        self.drop_session();
        unsafe {
            if !self.format.is_null() {
                CFRelease(self.format);
            }
        }
    }
}

#[derive(Default)]
struct Decoded {
    /// Buffers to fill, from a frame already drawn.
    spare: Option<Frame>,
    frame: Option<Frame>,
    status: OSStatus,
    error: Option<&'static str>,
}

/// Runs inside `VTDecompressionSessionDecodeFrame`. The callback has its
/// own status: successful submission does not guarantee successful decode.
unsafe extern "C" fn output(
    _decoder_refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    _flags: u32,
    image: CVImageBufferRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    if frame_refcon.is_null() {
        return;
    }
    let out = &mut *(frame_refcon as *mut Decoded);
    out.status = status;
    if status != 0 || image.is_null() {
        return;
    }
    let format = CVPixelBufferGetPixelFormatType(image);
    if CVPixelBufferGetPlaneCount(image) != 2
        || !matches!(format, K_CV_PIXEL_FORMAT_420V | K_CV_PIXEL_FORMAT_420F)
    {
        out.error = Some("VideoToolbox returned a non-NV12 picture");
        return;
    }
    if CVPixelBufferLockBaseAddress(image, K_CV_LOCK_READ_ONLY) != 0 {
        out.error = Some("VideoToolbox picture could not be read");
        return;
    }
    let w = CVPixelBufferGetWidthOfPlane(image, 0);
    let h = CVPixelBufferGetHeightOfPlane(image, 0);
    let ys = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
    let uvh = CVPixelBufferGetHeightOfPlane(image, 1);
    let uvs = CVPixelBufferGetBytesPerRowOfPlane(image, 1);
    let yp = CVPixelBufferGetBaseAddressOfPlane(image, 0);
    let uvp = CVPixelBufferGetBaseAddressOfPlane(image, 1);
    if !yp.is_null() && !uvp.is_null() {
        let mut frame = out.spare.take().unwrap_or_default();
        frame.fill(
            (w as u32, h as u32),
            (std::slice::from_raw_parts(yp, ys * h), ys),
            (std::slice::from_raw_parts(uvp, uvs * uvh), uvs),
            format == K_CV_PIXEL_FORMAT_420F,
        );
        out.frame = Some(frame);
    } else {
        out.error = Some("VideoToolbox picture has no pixel data");
    }
    CVPixelBufferUnlockBaseAddress(image, K_CV_LOCK_READ_ONLY);
}

impl Decoder for VideoToolbox {
    fn decode(&mut self, annexb: &[u8], _idr: bool, spare: Option<Frame>) -> Result<Option<Frame>> {
        let nals = nal_units(annexb);
        let mut sets = Vec::new();
        self.avcc.clear();
        for nal in nals {
            let t = self.nal_type(nal);
            if self.is_parameter_set(t) {
                sets.push(nal.to_vec());
            } else {
                self.avcc
                    .extend_from_slice(&(nal.len() as u32).to_be_bytes());
                self.avcc.extend_from_slice(nal);
            }
        }
        if !sets.is_empty() {
            self.set_parameters(sets)?;
        }
        if self.session.is_null() {
            bail!("no parameter sets yet");
        }
        if self.avcc.is_empty() {
            return Ok(None);
        }
        let len = self.avcc.len();
        let mut decoded = Decoded {
            spare,
            ..Default::default()
        };
        unsafe {
            let mut block: CMBlockBufferRef = ptr::null();
            let status = CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(),
                len,
                ptr::null(),
                ptr::null(),
                0,
                len,
                0,
                &mut block,
            );
            if status != 0 {
                bail!("block buffer failed ({status})");
            }
            let status =
                CMBlockBufferReplaceDataBytes(self.avcc.as_ptr() as *const c_void, block, 0, len);
            if status != 0 {
                CFRelease(block);
                bail!("block buffer copy failed ({status})");
            }
            let mut sample: CMSampleBufferRef = ptr::null();
            let status = CMSampleBufferCreateReady(
                ptr::null(),
                block,
                self.format,
                1,
                0,
                ptr::null(),
                1,
                &len,
                &mut sample,
            );
            CFRelease(block);
            if status != 0 {
                bail!("sample buffer failed ({status})");
            }
            let status = VTDecompressionSessionDecodeFrame(
                self.session,
                sample,
                0,
                &mut decoded as *mut Decoded as *mut c_void,
                ptr::null_mut(),
            );
            CFRelease(sample);
            // `decoded` lives on this stack frame and the callback writes
            // through it. Synchronous decode calls it before returning, but
            // a decoder may still hold a frame back; wait it out here so
            // the callback can never run after this frame is gone.
            if status == 0 {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            }
            let status = if status != 0 { status } else { decoded.status };
            if status != 0 {
                // -12903 kVTInvalidSessionErr, -12911 kVTVideoDecoderMalfunctionErr
                if status == -12903 || status == -12911 {
                    self.drop_session();
                    if !self.format.is_null() {
                        let _ = self.create_session();
                    }
                }
                return Err(anyhow!("decode failed ({status})"));
            }
        }
        if let Some(error) = decoded.error {
            bail!("{error}");
        }
        Ok(decoded.frame)
    }

    fn name(&self) -> &'static str {
        match (self.hevc, self.hardware) {
            (true, true) => "HEVC · hardware",
            (false, true) => "H.264 · hardware",
            (true, false) => "HEVC · software",
            (false, false) => "H.264 · software",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_visible_pixels_from_h264() {
        // 64x64, white top half and black bottom half, generated with OpenH264.
        let encoded = include_bytes!("../../tests/fixtures/gray-bars.h264");
        let mut decoder = VideoToolbox::new(false).unwrap();
        let frame = decoder
            .decode(encoded, true, None)
            .unwrap()
            .expect("decoded picture");
        assert_eq!((frame.width, frame.height), (64, 64));
        assert!(!frame.is_black());
        assert!(frame.y[16 * frame.y_stride + 32] >= 230);
        assert!(frame.y[48 * frame.y_stride + 32] <= 20);
        // Given the drawn frame back, the decoder fills its buffers again.
        let y_ptr = frame.y.as_ptr();
        let again = decoder
            .decode(encoded, true, Some(frame))
            .unwrap()
            .expect("decoded picture");
        assert_eq!(again.y.as_ptr(), y_ptr, "the spare's buffer was reused");
        assert!(again.y[16 * again.y_stride + 32] >= 230);
    }

    #[test]
    fn preserves_callback_errors_when_no_picture_is_returned() {
        let mut decoded = Decoded::default();
        let time = CMTime {
            value: 0,
            timescale: 1,
            flags: 1,
            epoch: 0,
        };
        unsafe {
            output(
                ptr::null_mut(),
                &mut decoded as *mut Decoded as *mut c_void,
                -12911,
                0,
                ptr::null(),
                time,
                time,
            );
        }
        assert_eq!(decoded.status, -12911);
        assert!(decoded.frame.is_none());
    }
}
