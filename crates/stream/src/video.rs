//! Decoded frames and the decoders that make them. Frames are NV12 (a Y plane
//! and an interleaved UV plane at half resolution), which is what
//! VideoToolbox produces and what the renderer's shader expects.

#[cfg(windows)]
pub mod mediafoundation;
use anyhow::Result;
use latch_proto::control::Codec;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

mod software;
#[cfg(target_os = "macos")]
mod videotoolbox;

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct NativeFrame {
    pixel_buffer: *const std::ffi::c_void,
}

#[cfg(target_os = "macos")]
unsafe impl Send for NativeFrame {}
#[cfg(target_os = "macos")]
unsafe impl Sync for NativeFrame {}

#[cfg(target_os = "macos")]
impl Drop for NativeFrame {
    fn drop(&mut self) {
        if !self.pixel_buffer.is_null() {
            unsafe {
                videotoolbox::cf_release(self.pixel_buffer);
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Clone for NativeFrame {
    fn clone(&self) -> Self {
        if !self.pixel_buffer.is_null() {
            unsafe {
                videotoolbox::cf_retain(self.pixel_buffer);
            }
        }
        Self {
            pixel_buffer: self.pixel_buffer,
        }
    }
}

#[cfg(target_os = "macos")]
impl NativeFrame {
    pub fn new(pixel_buffer: *const std::ffi::c_void) -> Self {
        Self { pixel_buffer }
    }

    pub fn pixel_buffer(&self) -> *const std::ffi::c_void {
        self.pixel_buffer
    }
}

#[derive(Default)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: usize,
    pub uv: Vec<u8>,
    pub uv_stride: usize,
    pub full_range: bool,
    #[cfg(target_os = "macos")]
    pub native: Option<NativeFrame>,
}

/// Drawn frames whose buffers are kept for the decoder to fill again.
const SPARES: usize = 3;

impl Frame {
    /// Fill this frame's buffers from raw planes, keeping the allocations
    /// when they are large enough already. A 3024×1964 frame is nine
    /// megabytes; taking fresh pages for each one at 60 fps costs the
    /// decoder thread milliseconds it does not have.
    /// `y` and `uv` are each a plane and its stride.
    pub fn fill(
        &mut self,
        (width, height): (u32, u32),
        (y, y_stride): (&[u8], usize),
        (uv, uv_stride): (&[u8], usize),
        full_range: bool,
    ) {
        self.width = width;
        self.height = height;
        self.y_stride = y_stride;
        self.uv_stride = uv_stride;
        self.full_range = full_range;
        self.y.clear();
        self.y.extend_from_slice(y);
        self.uv.clear();
        self.uv.extend_from_slice(uv);
        #[cfg(target_os = "macos")]
        {
            self.native = None;
        }
    }

    /// Run a closure with the Y and UV plane byte slices and strides.
    /// If this frame holds a native CVPixelBuffer, locks it read-only for the
    /// duration of the closure. Otherwise uses the CPU vectors.
    pub fn with_planes<R>(&self, f: impl FnOnce(&[u8], usize, &[u8], usize) -> R) -> R {
        #[cfg(target_os = "macos")]
        if let Some(native) = &self.native {
            let pb = native.pixel_buffer();
            if !pb.is_null()
                && unsafe {
                    videotoolbox::cv_pixel_buffer_lock_base_address(
                        pb,
                        videotoolbox::K_CV_LOCK_READ_ONLY,
                    )
                } == 0
            {
                let h = unsafe { videotoolbox::cv_pixel_buffer_get_height_of_plane(pb, 0) };
                let ys = unsafe { videotoolbox::cv_pixel_buffer_get_bytes_per_row_of_plane(pb, 0) };
                let uvh = unsafe { videotoolbox::cv_pixel_buffer_get_height_of_plane(pb, 1) };
                let uvs =
                    unsafe { videotoolbox::cv_pixel_buffer_get_bytes_per_row_of_plane(pb, 1) };
                let yp = unsafe { videotoolbox::cv_pixel_buffer_get_base_address_of_plane(pb, 0) };
                let uvp = unsafe { videotoolbox::cv_pixel_buffer_get_base_address_of_plane(pb, 1) };
                let res = if !yp.is_null() && !uvp.is_null() {
                    let y_slice = unsafe { std::slice::from_raw_parts(yp, ys * h) };
                    let uv_slice = unsafe { std::slice::from_raw_parts(uvp, uvs * uvh) };
                    f(y_slice, ys, uv_slice, uvs)
                } else {
                    f(&self.y, self.y_stride, &self.uv, self.uv_stride)
                };
                unsafe {
                    videotoolbox::cv_pixel_buffer_unlock_base_address(
                        pb,
                        videotoolbox::K_CV_LOCK_READ_ONLY,
                    )
                };
                return res;
            }
        }
        f(&self.y, self.y_stride, &self.uv, self.uv_stride)
    }

    /// Populates CPU plane vectors from native storage if they are not already populated.
    pub fn ensure_cpu_planes(&mut self) {
        #[cfg(target_os = "macos")]
        if self.y.is_empty() {
            if let Some(native) = &self.native {
                let pb = native.pixel_buffer();
                if !pb.is_null()
                    && unsafe {
                        videotoolbox::cv_pixel_buffer_lock_base_address(
                            pb,
                            videotoolbox::K_CV_LOCK_READ_ONLY,
                        )
                    } == 0
                {
                    let h = unsafe { videotoolbox::cv_pixel_buffer_get_height_of_plane(pb, 0) };
                    let ys =
                        unsafe { videotoolbox::cv_pixel_buffer_get_bytes_per_row_of_plane(pb, 0) };
                    let uvh = unsafe { videotoolbox::cv_pixel_buffer_get_height_of_plane(pb, 1) };
                    let uvs =
                        unsafe { videotoolbox::cv_pixel_buffer_get_bytes_per_row_of_plane(pb, 1) };
                    let yp =
                        unsafe { videotoolbox::cv_pixel_buffer_get_base_address_of_plane(pb, 0) };
                    let uvp =
                        unsafe { videotoolbox::cv_pixel_buffer_get_base_address_of_plane(pb, 1) };
                    if !yp.is_null() && !uvp.is_null() {
                        self.y.clear();
                        self.y
                            .extend_from_slice(unsafe { std::slice::from_raw_parts(yp, ys * h) });
                        self.uv.clear();
                        self.uv.extend_from_slice(unsafe {
                            std::slice::from_raw_parts(uvp, uvs * uvh)
                        });
                    }
                    unsafe {
                        videotoolbox::cv_pixel_buffer_unlock_base_address(
                            pb,
                            videotoolbox::K_CV_LOCK_READ_ONLY,
                        )
                    };
                }
            }
        }
    }

    /// Check visible NV12 pixels, excluding row padding. A completely black
    /// capture can still be a valid, successfully decoded video stream.
    pub fn is_black(&self) -> bool {
        self.with_planes(|y, y_stride, uv, uv_stride| {
            let width = self.width as usize;
            let height = self.height as usize;
            let chroma_width = width.div_ceil(2) * 2;
            let chroma_height = height.div_ceil(2);
            if width == 0
                || height == 0
                || y_stride < width
                || uv_stride < chroma_width
                || y.len() < y_stride.saturating_mul(height)
                || uv.len() < uv_stride.saturating_mul(chroma_height)
            {
                return false;
            }
            let black = if self.full_range { 1 } else { 17 };
            // Whole-row reductions rather than a short-circuiting `all`: the
            // compiler turns a max or min over a row into vector instructions,
            // and the scan still stops at the first row with a visible pixel.
            // A black frame is every row, sixty times a second.
            let max = |row: &[u8]| row.iter().fold(0, |m, &v| m.max(v));
            let min = |row: &[u8]| row.iter().fold(u8::MAX, |m, &v| m.min(v));
            y.chunks(y_stride)
                .take(height)
                .all(|row| max(&row[..width]) <= black)
                && uv.chunks(uv_stride).take(chroma_height).all(|row| {
                    let row = &row[..chroma_width];
                    min(row) >= 127 && max(row) <= 129
                })
        })
    }
}

/// The newest decoded frame, replaced rather than queued: a renderer that
/// falls behind shows the latest picture instead of catching up on old ones.
/// Frames the renderer has drawn come back through [`FrameSlot::recycle`]
/// and go out again through [`FrameSlot::spare`], so the decoder fills the
/// same few buffers over and over.
#[derive(Default)]
pub struct FrameSlot {
    latest: Mutex<Option<Frame>>,
    seq: AtomicU64,
    spare: Mutex<Vec<Frame>>,
}

impl FrameSlot {
    pub fn publish(&self, frame: Frame) {
        if let Some(undrawn) = self.latest.lock().replace(frame) {
            self.recycle(undrawn);
        }
        self.seq.fetch_add(1, Ordering::Release);
    }

    pub fn take(&self) -> Option<Frame> {
        self.latest.lock().take()
    }

    /// Increments on every published frame; cheap to poll.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// A drawn frame's buffers, for the decoder to fill again.
    pub fn recycle(&self, frame: Frame) {
        let mut spare = self.spare.lock();
        if spare.len() < SPARES {
            spare.push(frame);
        }
    }

    /// Buffers that have come back from the renderer, if any.
    pub fn spare(&self) -> Option<Frame> {
        self.spare.lock().pop()
    }

    pub fn clear(&self) {
        *self.latest.lock() = None;
        self.spare.lock().clear();
    }
}

pub trait Decoder: Send {
    /// One access unit in Annex B. `None` when the decoder needs more data.
    /// `spare` is a drawn frame whose buffers may be filled instead of
    /// allocating new ones.
    fn decode(&mut self, annexb: &[u8], idr: bool, spare: Option<Frame>) -> Result<Option<Frame>>;
    fn name(&self) -> &'static str;
}

/// What this machine decodes, best first. HEVC and H.264 High need the
/// platform's decoder; `latch-codec` decodes Constrained Baseline anywhere.
///
/// A Windows viewer asks for Constrained Baseline only: Media Foundation's
/// decoder is used when it works, and when it gives no picture the stream
/// carries on in `latch-codec` without the host noticing (see
/// [`Fallback`]). Media Foundation has not been seen decoding on real
/// hardware yet; a High-profile stream would leave no way out.
pub fn decodable() -> Vec<Codec> {
    #[cfg(target_os = "macos")]
    {
        vec![Codec::Hevc, Codec::H264High, Codec::H264Baseline]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![Codec::H264Baseline]
    }
}

/// A decoder for `codec`: the platform's where there is one, else
/// `latch-codec`'s for Constrained Baseline.
pub fn new_decoder(codec: Codec) -> Result<Box<dyn Decoder>> {
    #[cfg(target_os = "macos")]
    {
        match videotoolbox::VideoToolbox::new(codec == Codec::Hevc) {
            Ok(d) if codec == Codec::H264Baseline => {
                return Ok(Box::new(Fallback::new(Box::new(d))));
            }
            Ok(d) => return Ok(Box::new(d)),
            Err(e) if codec == Codec::H264Baseline => {
                tracing::warn!("VideoToolbox: {e:#}; decoding in software");
            }
            Err(e) => return Err(e),
        }
    }
    #[cfg(windows)]
    {
        if codec == Codec::H264Baseline {
            match mediafoundation::MfDecoder::new() {
                Ok(d) => return Ok(Box::new(Fallback::new(Box::new(MediaFoundation(d))))),
                Err(e) => tracing::warn!("Media Foundation: {e:#}; decoding in software"),
            }
        }
    }
    match codec {
        Codec::H264Baseline => Ok(Box::new(software::Software::new())),
        other => anyhow::bail!("this machine cannot decode {}", other.name()),
    }
}

/// Access units a platform decoder may take without giving a picture or
/// while failing before the stream moves to `latch-codec`. A hardware
/// decoder holds a frame or two at most; ten is a decoder that does not
/// work here.
const PROBATION: u32 = 10;

/// A platform decoder for a Constrained Baseline stream, with `latch-codec`
/// behind it: if the platform's decoder gives no picture for its first
/// [`PROBATION`] access units, or fails on them, every access unit from the
/// next keyframe on goes to `latch-codec` instead. Once the platform's
/// decoder has given a picture it is trusted, and its errors are the
/// stream's (a lost reference, say), as with any decoder.
pub struct Fallback {
    platform: Option<Box<dyn Decoder>>,
    software: Option<software::Software>,
    proven: bool,
    tries: u32,
}

impl Fallback {
    pub fn new(platform: Box<dyn Decoder>) -> Self {
        Self {
            platform: Some(platform),
            software: None,
            proven: false,
            tries: 0,
        }
    }
}

impl Decoder for Fallback {
    fn decode(
        &mut self,
        annexb: &[u8],
        idr: bool,
        mut spare: Option<Frame>,
    ) -> Result<Option<Frame>> {
        if let Some(platform) = self.platform.as_mut() {
            let result = platform.decode(annexb, idr, spare.take());
            if self.proven {
                return result;
            }
            match result {
                Ok(Some(frame)) => {
                    self.proven = true;
                    return Ok(Some(frame));
                }
                Ok(None) | Err(_) => {
                    self.tries += 1;
                    if self.tries < PROBATION {
                        return result;
                    }
                    tracing::warn!(
                        "{} gave no picture for {} access units; decoding in software",
                        platform.name(),
                        self.tries
                    );
                    self.platform = None;
                    self.software = Some(software::Software::new());
                    if !idr {
                        // Software needs a keyframe to start from; the
                        // session asks for one when this fails.
                        anyhow::bail!("switching to software decoding; waiting for a keyframe");
                    }
                }
            }
        }
        // Software fills the renderer's spare, once the platform has not
        // taken it on the way here.
        match self.software.as_mut() {
            Some(software) => software.decode(annexb, idr, spare),
            None => Ok(None),
        }
    }

    fn name(&self) -> &'static str {
        match &self.platform {
            Some(platform) => platform.name(),
            None => "Latch H.264 (software)",
        }
    }
}

/// Media Foundation's decoder as a [`Decoder`].
#[cfg(windows)]
struct MediaFoundation(mediafoundation::MfDecoder);

#[cfg(windows)]
impl Decoder for MediaFoundation {
    fn decode(
        &mut self,
        annexb: &[u8],
        _idr: bool,
        _spare: Option<Frame>,
    ) -> Result<Option<Frame>> {
        Ok(self.0.decode(annexb)?.map(|d| Frame {
            width: d.width,
            height: d.height,
            y: d.y,
            y_stride: d.y_stride,
            uv: d.uv,
            uv_stride: d.uv_stride,
            full_range: false,
        }))
    }

    fn name(&self) -> &'static str {
        self.0.name()
    }
}

/// Split an Annex B byte stream into NAL units without their start codes.
#[cfg(any(target_os = "macos", test))]
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map(|&n| n - 3).unwrap_or(data.len());
        // A 4-byte start code leaves a trailing zero on the previous NAL.
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&data[s..end]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A platform decoder that takes everything and shows nothing.
    struct Silent;

    impl Decoder for Silent {
        fn decode(&mut self, _: &[u8], _: bool, _: Option<Frame>) -> Result<Option<Frame>> {
            Ok(None)
        }

        fn name(&self) -> &'static str {
            "silent"
        }
    }

    #[test]
    fn a_platform_decoder_that_shows_nothing_hands_over_to_software() {
        let mut encoder = latch_codec::h264::Encoder::new(latch_codec::h264::EncoderConfig {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_kbps: 1_000,
            threads: 1,
        })
        .unwrap();
        let mut picture = latch_codec::Yuv420::new(64, 64);
        picture.y.fill(200);
        let mut idr = Vec::new();
        encoder.encode(&picture, true, &mut idr).unwrap();
        let mut decoder = Fallback::new(Box::new(Silent));
        for _ in 1..PROBATION {
            assert!(decoder.decode(&idr, true, None).unwrap().is_none());
            assert_eq!(decoder.name(), "silent");
        }
        let frame = decoder
            .decode(&idr, true, None)
            .unwrap()
            .expect("software's picture");
        assert_eq!((frame.width, frame.height), (64, 64));
        assert!(frame.y[0].abs_diff(200) <= 2);
        assert_eq!(decoder.name(), "Latch H.264 (software)");
        // From then on software fills the renderer's spare buffers.
        let spare = Frame {
            y: Vec::with_capacity(64 * 64 * 2),
            uv: Vec::with_capacity(64 * 64),
            ..Default::default()
        };
        let (y_ptr, uv_ptr) = (spare.y.as_ptr(), spare.uv.as_ptr());
        let frame = decoder
            .decode(&idr, true, Some(spare))
            .unwrap()
            .expect("software's picture");
        assert_eq!((frame.y.as_ptr(), frame.uv.as_ptr()), (y_ptr, uv_ptr));
    }

    #[test]
    fn a_switch_mid_stream_waits_for_a_keyframe() {
        let mut decoder = Fallback::new(Box::new(Silent));
        for _ in 1..PROBATION {
            assert!(decoder
                .decode(&[0, 0, 1, 0x41], false, None)
                .unwrap()
                .is_none());
        }
        assert!(decoder.decode(&[0, 0, 1, 0x41], false, None).is_err());
        assert_eq!(decoder.name(), "Latch H.264 (software)");
    }

    #[test]
    // `native` exists only on macOS.
    #[allow(clippy::needless_update)]
    fn black_detection_checks_visible_pixels_and_both_ranges() {
        let mut frame = Frame {
            width: 2,
            height: 2,
            y: vec![16, 16, 255, 16, 16, 255],
            y_stride: 3,
            uv: vec![128, 128, 255, 255],
            uv_stride: 4,
            full_range: false,
            ..Default::default()
        };
        assert!(frame.is_black(), "padding is not picture data");
        frame.y[4] = 235;
        assert!(!frame.is_black(), "even a small visible detail counts");
        frame.y[4] = 16;
        frame.full_range = true;
        assert!(!frame.is_black());
        frame.y = vec![0, 0, 255, 0, 0, 255];
        assert!(frame.is_black());
        frame.uv[0] = 70;
        assert!(!frame.is_black(), "colored video is not a blank capture");
        frame.uv.clear();
        assert!(
            !frame.is_black(),
            "an invalid frame must not be diagnosed as black"
        );
    }

    #[test]
    fn nal_units_are_split_on_both_start_code_lengths() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS with a 4-byte start code
            0, 0, 1, 0x68, 3, // PPS with a 3-byte start code
            0, 0, 0, 1, 0x65, 4, 5, 6, 0, 0, // slice; trailing zeros belong to nothing
        ];
        let nals = nal_units(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 1, 2]);
        assert_eq!(nals[1], &[0x68, 3]);
        assert_eq!(nals[2], &[0x65, 4, 5, 6]);
        assert!(nal_units(&[1, 2, 3]).is_empty());
    }

    #[test]
    // `native` exists only on macOS.
    #[allow(clippy::needless_update)]
    fn slot_keeps_only_the_newest() {
        let slot = FrameSlot::default();
        let f = |w| Frame {
            width: w,
            height: 1,
            y: vec![],
            y_stride: 0,
            uv: vec![],
            uv_stride: 0,
            full_range: false,
            ..Default::default()
        };
        slot.publish(f(1));
        slot.publish(f(2));
        assert_eq!(slot.seq(), 2);
        assert_eq!(slot.take().unwrap().width, 2);
        assert!(slot.take().is_none());
        // The frame nobody drew went back for the decoder to fill again.
        assert_eq!(slot.spare().map(|f| f.width), Some(1));
        assert!(slot.spare().is_none());
        for w in 10..20 {
            slot.recycle(f(w));
        }
        let kept: Vec<u32> = std::iter::from_fn(|| slot.spare().map(|f| f.width)).collect();
        assert_eq!(
            kept.len(),
            SPARES,
            "a few spares, not every frame ever drawn"
        );
        slot.recycle(f(1));
        slot.clear();
        assert!(slot.spare().is_none(), "a new session starts with nothing");
    }

    #[test]
    fn fill_reuses_the_buffers_it_is_given() {
        let mut frame = Frame {
            y: Vec::with_capacity(64),
            uv: Vec::with_capacity(32),
            ..Default::default()
        };
        let (y_ptr, uv_ptr) = (frame.y.as_ptr(), frame.uv.as_ptr());
        frame.fill((4, 4), (&[7; 16], 4), (&[128; 8], 4), true);
        assert_eq!(
            (frame.width, frame.height, frame.y_stride, frame.uv_stride),
            (4, 4, 4, 4)
        );
        assert!(frame.full_range);
        assert_eq!(frame.y, [7; 16]);
        assert_eq!(frame.uv, [128; 8]);
        assert_eq!(frame.y.as_ptr(), y_ptr, "no new allocation for the Y plane");
        assert_eq!(
            frame.uv.as_ptr(),
            uv_ptr,
            "no new allocation for the UV plane"
        );
    }
}
