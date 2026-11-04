//! Hardware H.264 and HEVC decode on macOS through VideoToolbox, bound by
//! hand: a dozen C functions from CoreMedia, CoreVideo and VideoToolbox.
//!
//! Each access unit arrives in Annex B. Parameter sets go into a format
//! description; the slices are re-framed with 4-byte lengths (AVCC/HVCC) into
//! one sample buffer and decoded synchronously; `decode` also waits out any
//! frame the decoder held back, so the output callback has always run before
//! `decode` returns.
//!
//! The decode calls run on a worker thread of the session, and `decode`
//! waits for them at most [`DECODE_DEADLINE`]: Apple's H.264 decoder can
//! block forever inside them (a stream whose reference pictures overflow
//! `max_num_ref_frames` after a loss, which FFmpeg decodes). A session that
//! stalls is abandoned to its worker and replaced, and the error makes the
//! viewer ask for a keyframe, so the picture freezes for the deadline, not
//! for good.

use super::{nal_units, Decoder, Frame, PixelFormat};
use anyhow::{anyhow, bail, Result};
use latch_proto::control::Codec;
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc;
use std::time::Duration;

/// How long one access unit may stay in VideoToolbox. A hardware decode
/// takes milliseconds, even the first of an 8K session; this is a decoder
/// that will not return.
const DECODE_DEADLINE: Duration = Duration::from_secs(1);
/// Sessions a process may abandon. An abandoned session cannot be
/// invalidated (that blocks too), so its thread and pictures stay; past
/// this, the decoder fails every access unit and the stream renegotiates
/// without its codec.
const MAX_ABANDONED: u32 = 3;
static ABANDONED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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
const K_CV_PIXEL_FORMAT_X420: u32 = u32::from_be_bytes(*b"x420");
const K_CV_PIXEL_FORMAT_XF20: u32 = u32::from_be_bytes(*b"xf20");
const K_CV_PIXEL_FORMAT_420F: u32 = 0x3432_3066; // '420f', full range
pub(crate) const K_CV_LOCK_READ_ONLY: u64 = 1;

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
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    static kCVPixelBufferMetalCompatibilityKey: CFStringRef;
    static kCFBooleanTrue: CFTypeRef;
    static kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: CFStringRef;
    static kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder: CFStringRef;
    static kVTDecompressionPropertyKey_RealTime: CFStringRef;
    fn VTIsHardwareDecodeSupported(codec: u32) -> u8;
    fn CMVideoFormatDescriptionCreate(
        allocator: CFAllocatorRef,
        codec: u32,
        width: i32,
        height: i32,
        extensions: CFDictionaryRef,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFStringRef;
    fn CFStringCreateWithCString(
        allocator: CFAllocatorRef,
        string: *const u8,
        encoding: u32,
    ) -> CFStringRef;
    fn CFDataCreate(allocator: CFAllocatorRef, bytes: *const u8, len: isize) -> CFTypeRef;
    fn CFBooleanGetValue(value: CFTypeRef) -> u8;
    fn VTSessionSetProperty(session: CFTypeRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTSessionCopyProperty(
        session: CFTypeRef,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        out: *mut CFTypeRef,
    ) -> OSStatus;

    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
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

pub(crate) unsafe fn cf_retain(cf: *const c_void) -> *const c_void {
    CFRetain(cf as CFTypeRef)
}

pub(crate) unsafe fn cf_release(cf: *const c_void) {
    CFRelease(cf as CFTypeRef);
}

pub(crate) unsafe fn cv_pixel_buffer_lock_base_address(pb: *const c_void, flags: u64) -> i32 {
    CVPixelBufferLockBaseAddress(pb as CVImageBufferRef, flags)
}

pub(crate) unsafe fn cv_pixel_buffer_unlock_base_address(pb: *const c_void, flags: u64) -> i32 {
    CVPixelBufferUnlockBaseAddress(pb as CVImageBufferRef, flags)
}

pub(crate) unsafe fn cv_pixel_buffer_get_height_of_plane(pb: *const c_void, plane: usize) -> usize {
    CVPixelBufferGetHeightOfPlane(pb as CVImageBufferRef, plane)
}

pub(crate) unsafe fn cv_pixel_buffer_get_bytes_per_row_of_plane(
    pb: *const c_void,
    plane: usize,
) -> usize {
    CVPixelBufferGetBytesPerRowOfPlane(pb as CVImageBufferRef, plane)
}

pub(crate) unsafe fn cv_pixel_buffer_get_base_address_of_plane(
    pb: *const c_void,
    plane: usize,
) -> *const u8 {
    CVPixelBufferGetBaseAddressOfPlane(pb as CVImageBufferRef, plane)
}

pub struct VideoToolbox {
    codec: Codec,
    output: PixelFormat,
    hardware: bool,
    format: CMFormatDescriptionRef,
    session: VTDecompressionSessionRef,
    /// Parameter sets the current format description was built from.
    params: Vec<Vec<u8>>,
    avcc: Vec<u8>,
    /// Runs the current session's decode calls; replaced with the session
    /// when one stalls.
    worker: Option<Worker>,
    deadline: Duration,
}

unsafe impl Send for VideoToolbox {}

/// One access unit for the worker: the session and sample each hold a
/// reference the worker releases, and `decoded` is where the output
/// callback writes, owned by whoever waits for it.
struct Job {
    session: VTDecompressionSessionRef,
    sample: CMSampleBufferRef,
    decoded: Box<Decoded>,
}
// SAFETY: the job owns its CF references; VideoToolbox sessions may be used
// from any thread, one call at a time, which the worker's queue guarantees.
unsafe impl Send for Job {}

struct Worker {
    jobs: mpsc::Sender<Job>,
    done: mpsc::Receiver<(OSStatus, Box<Decoded>)>,
}

impl Worker {
    fn spawn() -> Result<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (finished, done) = mpsc::channel();
        std::thread::Builder::new()
            .name("latch-vt-decode".into())
            .spawn(move || {
                crate::priority::media_thread();
                while let Ok(mut job) = queue.recv() {
                    // SAFETY: the job holds the session and sample; the
                    // callback writes through `decoded`, which this thread
                    // owns until the wait below has drained every frame.
                    let status = unsafe {
                        let status = VTDecompressionSessionDecodeFrame(
                            job.session,
                            job.sample,
                            0,
                            &mut *job.decoded as *mut Decoded as *mut c_void,
                            ptr::null_mut(),
                        );
                        CFRelease(job.sample);
                        // Synchronous decode calls back before returning,
                        // but a decoder may still hold a frame back.
                        if status == 0 {
                            VTDecompressionSessionWaitForAsynchronousFrames(job.session);
                        }
                        CFRelease(job.session);
                        status
                    };
                    if finished.send((status, job.decoded)).is_err() {
                        // Abandoned after a stall: nobody waits any more.
                        return;
                    }
                }
            })?;
        Ok(Self { jobs, done })
    }
}

pub fn supported(codec: Codec) -> bool {
    static CODECS: std::sync::OnceLock<Vec<Codec>> = std::sync::OnceLock::new();
    CODECS
        .get_or_init(|| {
            [
                (
                    Codec::Av1,
                    *b"av01",
                    include_bytes!("../../tests/fixtures/gray-bars.av1").as_slice(),
                ),
                (
                    Codec::Hevc,
                    *b"hvc1",
                    include_bytes!("../../tests/fixtures/gray-bars.hevc").as_slice(),
                ),
                (
                    Codec::H264High,
                    *b"avc1",
                    include_bytes!("../../tests/fixtures/decode-probe.h264").as_slice(),
                ),
            ]
            .into_iter()
            .filter_map(|(codec, fourcc, bytes)| {
                if unsafe { VTIsHardwareDecodeSupported(u32::from_be_bytes(fourcc)) } == 0 {
                    return None;
                }
                // The capability query can succeed even when no usable decoder
                // session is available (for example on virtualized CI Macs).
                let result = (|| {
                    let mut decoder = VideoToolbox::with_format(codec, PixelFormat::Nv12)?;
                    let picture = decoder.decode(bytes, true, None)?;
                    anyhow::ensure!(picture.is_some() && decoder.hardware, "no hardware picture");
                    Ok::<_, anyhow::Error>(())
                })();
                match result {
                    Ok(()) => Some(codec),
                    Err(error) => {
                        tracing::info!(?codec, "hardware decode unavailable: {error:#}");
                        None
                    }
                }
            })
            .collect()
        })
        .contains(&codec)
}

impl VideoToolbox {
    #[cfg(test)]
    pub fn new(codec: Codec) -> Result<Self> {
        Self::with_format(codec, PixelFormat::Nv12)
    }

    pub fn with_format(codec: Codec, output: PixelFormat) -> Result<Self> {
        Ok(Self {
            codec,
            output,
            hardware: false,
            format: ptr::null(),
            session: ptr::null(),
            params: Vec::new(),
            avcc: Vec::new(),
            worker: None,
            deadline: DECODE_DEADLINE,
        })
    }

    /// Gives up on a session whose decode call has not returned: its worker
    /// keeps it until (if ever) the call returns, and a fresh session from
    /// the same format takes its place.
    fn abandon_session(&mut self) -> Result<()> {
        self.worker = None;
        let stalled = std::mem::replace(&mut self.session, ptr::null());
        if !stalled.is_null() {
            // Invalidating may block as the decode does, so it happens on a
            // thread of its own; it also releases this reference.
            let stalled = stalled as usize;
            let _ = std::thread::Builder::new()
                .name("latch-vt-abandon".into())
                .spawn(move || unsafe {
                    let session = stalled as VTDecompressionSessionRef;
                    VTDecompressionSessionInvalidate(session);
                    CFRelease(session);
                });
        }
        let abandoned = ABANDONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if abandoned > MAX_ABANDONED {
            // No session: every access unit fails from here on.
            self.params.clear();
            bail!("VideoToolbox stalled {abandoned} times; not decoding here again");
        }
        if !self.format.is_null() {
            self.create_session()?;
        }
        Ok(())
    }

    fn nal_type(&self, nal: &[u8]) -> u8 {
        if self.codec == Codec::Hevc {
            (nal[0] >> 1) & 0x3f
        } else {
            nal[0] & 0x1f
        }
    }

    fn is_parameter_set(&self, t: u8) -> bool {
        if self.codec == Codec::Hevc {
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
            if self.codec == Codec::Hevc {
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
        self.params = sets;
        self.set_format(fmt)
    }

    fn set_format(&mut self, fmt: CMFormatDescriptionRef) -> Result<()> {
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
        if self.session.is_null() {
            self.create_session()?;
        }
        Ok(())
    }

    fn set_av1(&mut self, data: &[u8]) -> Result<()> {
        let Some(sequence) = super::av1::sequence(data)? else {
            return Ok(());
        };
        if self.params.first() == Some(&sequence.config) && !self.format.is_null() {
            if self.session.is_null() {
                self.create_session()?;
            }
            return Ok(());
        }
        unsafe {
            let key = CFStringCreateWithCString(ptr::null(), c"av1C".as_ptr().cast(), 0x08000100);
            let bytes = CFDataCreate(
                ptr::null(),
                sequence.config.as_ptr(),
                sequence.config.len() as isize,
            );
            let atoms = CFDictionaryCreate(
                ptr::null(),
                [key].as_ptr(),
                [bytes].as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let extensions = CFDictionaryCreate(
                ptr::null(),
                [kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms].as_ptr(),
                [atoms].as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let mut fmt = ptr::null();
            let status = CMVideoFormatDescriptionCreate(
                ptr::null(),
                u32::from_be_bytes(*b"av01"),
                sequence.width,
                sequence.height,
                extensions,
                &mut fmt,
            );
            for value in [extensions, atoms, bytes, key] {
                CFRelease(value);
            }
            if status != 0 || fmt.is_null() {
                bail!("AV1 format description failed ({status})");
            }
            self.params = vec![sequence.config];
            self.set_format(fmt)
        }
    }

    fn create_session(&mut self) -> Result<()> {
        unsafe {
            // The stream is limited range (`COLOR_RANGE_LIMITED`).
            let pixel_format = match self.output {
                PixelFormat::Nv12 => K_CV_PIXEL_FORMAT_420V,
                PixelFormat::P010 => K_CV_PIXEL_FORMAT_X420,
            } as i32;
            let number = CFNumberCreate(
                ptr::null(),
                K_CF_NUMBER_SINT32,
                &pixel_format as *const i32 as *const c_void,
            );
            let empty_dict = CFDictionaryCreate(
                ptr::null(),
                ptr::null(),
                ptr::null(),
                0,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let keys = [
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ];
            let values = [number, empty_dict, kCFBooleanTrue];
            let attrs = CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                3,
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
            CFRelease(empty_dict);
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
        || !matches!(
            format,
            K_CV_PIXEL_FORMAT_420V
                | K_CV_PIXEL_FORMAT_420F
                | K_CV_PIXEL_FORMAT_X420
                | K_CV_PIXEL_FORMAT_XF20
        )
    {
        out.error = Some("VideoToolbox returned an unsupported pixel format");
        return;
    }
    let w = CVPixelBufferGetWidthOfPlane(image, 0);
    let h = CVPixelBufferGetHeightOfPlane(image, 0);
    let ys = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
    let uvs = CVPixelBufferGetBytesPerRowOfPlane(image, 1);
    let mut frame = out.spare.take().unwrap_or_default();
    frame.width = w as u32;
    frame.height = h as u32;
    frame.y_stride = ys;
    frame.uv_stride = uvs;
    frame.full_range = matches!(format, K_CV_PIXEL_FORMAT_420F | K_CV_PIXEL_FORMAT_XF20);
    frame.format = if matches!(format, K_CV_PIXEL_FORMAT_X420 | K_CV_PIXEL_FORMAT_XF20) {
        PixelFormat::P010
    } else {
        PixelFormat::Nv12
    };
    frame.color = latch_proto::color::ColorDescription {
        bit_depth: if frame.format == PixelFormat::P010 {
            10
        } else {
            8
        },
        ..Default::default()
    };
    frame.y.clear();
    frame.uv.clear();
    CFRetain(image);
    frame.native = Some(super::NativeFrame::new(image));
    out.frame = Some(frame);
}

impl Decoder for VideoToolbox {
    fn decode(&mut self, annexb: &[u8], _idr: bool, spare: Option<Frame>) -> Result<Option<Frame>> {
        self.avcc.clear();
        if self.codec == Codec::Av1 {
            self.set_av1(annexb)?;
            self.avcc.extend_from_slice(annexb);
        } else {
            let mut sets = Vec::new();
            for nal in nal_units(annexb) {
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
        }
        if ABANDONED.load(std::sync::atomic::Ordering::Relaxed) > MAX_ABANDONED {
            bail!("VideoToolbox stalled too often in this process");
        }
        if self.session.is_null() {
            bail!("no parameter sets yet");
        }
        if self.avcc.is_empty() {
            return Ok(None);
        }
        let len = self.avcc.len();
        let decoded = Box::new(Decoded {
            spare,
            ..Default::default()
        });
        let sample = unsafe {
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
            sample
        };
        if self.worker.is_none() {
            match Worker::spawn() {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    unsafe { CFRelease(sample) };
                    return Err(anyhow!("VideoToolbox decode thread: {error}"));
                }
            }
        }
        let worker = self.worker.as_ref().expect("worker");
        let job = Job {
            // SAFETY: the worker releases this reference after its calls.
            session: unsafe { CFRetain(self.session) },
            sample,
            decoded,
        };
        if let Err(mpsc::SendError(job)) = worker.jobs.send(job) {
            unsafe {
                CFRelease(job.sample);
                CFRelease(job.session);
            }
            self.worker = None;
            bail!("VideoToolbox decode thread stopped");
        }
        let (status, decoded) = match worker.done.recv_timeout(self.deadline) {
            Ok(done) => done,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    "VideoToolbox gave no picture in {:?}; replacing the decoder session",
                    self.deadline
                );
                self.abandon_session()?;
                bail!("VideoToolbox decode stalled; decoder session replaced");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.worker = None;
                bail!("VideoToolbox decode thread stopped");
            }
        };
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
        if let Some(error) = decoded.error {
            bail!("{error}");
        }
        Ok(decoded.frame)
    }

    fn name(&self) -> &'static str {
        match (self.codec, self.hardware) {
            (Codec::Av1, true) => "AV1 · hardware",
            (Codec::Av1, false) => "AV1 · software",
            (Codec::Hevc, true) => "HEVC · hardware",
            (Codec::Hevc, false) => "HEVC · software",
            (_, true) => "H.264 · hardware",
            (_, false) => "H.264 · software",
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
        let mut decoder = VideoToolbox::new(Codec::H264High).unwrap();
        let frame = decoder
            .decode(encoded, true, None)
            .unwrap()
            .expect("decoded picture");
        assert_eq!((frame.width, frame.height), (64, 64));
        assert!(!frame.is_black());
        assert!(frame.native.is_some(), "frame has native CVPixelBuffer");
        frame.with_planes(|y, ys, _, _| {
            assert!(y[16 * ys + 32] >= 230);
            assert!(y[48 * ys + 32] <= 20);
        });
        let again = decoder
            .decode(encoded, true, Some(frame))
            .unwrap()
            .expect("decoded picture");
        assert!(
            again.native.is_some(),
            "second frame has native CVPixelBuffer"
        );
        again.with_planes(|y, ys, _, _| {
            assert!(y[16 * ys + 32] >= 230);
        });
    }

    #[test]
    fn decodes_av1_and_hevc_on_supported_hardware() {
        for (codec, bytes) in [
            (
                Codec::Av1,
                include_bytes!("../../tests/fixtures/gray-bars.av1").as_slice(),
            ),
            (
                Codec::Hevc,
                include_bytes!("../../tests/fixtures/gray-bars.hevc").as_slice(),
            ),
        ] {
            if !supported(codec) {
                continue;
            }
            let mut decoder = VideoToolbox::new(codec).unwrap();
            let frame = decoder
                .decode(bytes, true, None)
                .unwrap()
                .expect("hardware picture");
            assert_eq!((frame.width, frame.height), (64, 64));
            assert!(decoder.hardware);
            assert!(frame.native.is_some());
            frame.with_planes(|y, ys, _, _| {
                assert!(y[16 * ys + 32] >= 230);
                assert!(y[48 * ys + 32] <= 20);
            });
        }
    }

    #[test]
    fn main10_retains_ten_bit_samples_in_an_iosurface() {
        if !supported(Codec::Hevc) {
            return;
        }
        let mut decoder = VideoToolbox::with_format(Codec::Hevc, PixelFormat::P010).unwrap();
        let frame = decoder
            .decode(
                include_bytes!("../../tests/fixtures/gray-bars-main10.hevc"),
                true,
                None,
            )
            .unwrap()
            .expect("Main10 picture");
        assert_eq!(frame.format, PixelFormat::P010);
        assert!(!frame.is_black());
        assert!(frame.native.is_some());
        frame.with_planes(|y, stride, _, _| {
            let white = u16::from_le_bytes([y[16 * stride + 64], y[16 * stride + 65]]) >> 6;
            let black = u16::from_le_bytes([y[48 * stride + 64], y[48 * stride + 65]]) >> 6;
            assert!(white >= 920, "{white}");
            assert!(black <= 80, "{black}");
        });
    }

    /// Access units of an Annex B stream with one slice per picture.
    fn pictures(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut unit = Vec::new();
        for nal in nal_units(stream) {
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(nal);
            if matches!(nal[0] & 0x1f, 1 | 5) {
                out.push(std::mem::take(&mut unit));
            }
        }
        out
    }

    /// A VideoToolbox H.264 LTR stream after a loss, recorded by the
    /// engine's pipelined simulator with the refresh the host now withholds
    /// (960×540): the lost pictures carried long-term marks, so after the
    /// frame_num gap the viewer's references exceed `max_num_ref_frames`
    /// (14 of 10) once the encoder marks those slots again. FFmpeg decodes
    /// it, warning; Apple's decoder blocks forever on its last picture (the
    /// #60 review's 1.1 MB stream did the same 41 pictures in). A viewer
    /// that meets one must not freeze.
    #[test]
    fn a_decode_that_never_returns_is_abandoned_and_the_next_keyframe_decodes() {
        if !supported(Codec::H264High) {
            return;
        }
        let path = std::env::var("LATCH_VT_STALL_FIXTURE").unwrap_or_else(|_| {
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/vt-ltr-overflow.h264"
            )
            .into()
        });
        let units = pictures(&std::fs::read(path).unwrap());
        let mut decoder = VideoToolbox::new(Codec::H264High).unwrap();
        decoder.deadline = Duration::from_millis(500);
        let mut stalled = None;
        for (i, unit) in units.iter().enumerate() {
            let start = std::time::Instant::now();
            let result = decoder.decode(unit, i == 0, None);
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "picture {i} took {:?}",
                start.elapsed()
            );
            match result {
                Ok(picture) => assert!(picture.is_some(), "picture {i}"),
                Err(error) => {
                    eprintln!("picture {i}: {error:#}");
                    stalled = Some(i);
                    break;
                }
            }
        }
        eprintln!("stalled at {stalled:?} of {}", units.len());
        // Whether or not this macOS still stalls, the session takes a
        // keyframe afterwards and gives its picture promptly.
        let start = std::time::Instant::now();
        let picture = decoder
            .decode(&units[0], true, None)
            .unwrap()
            .expect("keyframe after the stall");
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!((picture.width, picture.height), (960, 540));
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
