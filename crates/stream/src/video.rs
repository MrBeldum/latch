//! Decoded frames and the decoders that make them. Frames are NV12 (a Y plane
//! and an interleaved UV plane at half resolution), which is what
//! VideoToolbox produces and what the renderer's shader expects.

use anyhow::Result;
use latch_proto::control::Codec;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

mod software;
#[cfg(target_os = "macos")]
mod videotoolbox;

#[derive(Default)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: usize,
    pub uv: Vec<u8>,
    pub uv_stride: usize,
    pub full_range: bool,
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
    }

    /// Check visible NV12 pixels, excluding row padding. A completely black
    /// capture can still be a valid, successfully decoded video stream.
    pub fn is_black(&self) -> bool {
        let width = self.width as usize;
        let height = self.height as usize;
        let chroma_width = width.div_ceil(2) * 2;
        let chroma_height = height.div_ceil(2);
        if width == 0
            || height == 0
            || self.y_stride < width
            || self.uv_stride < chroma_width
            || self.y.len() < self.y_stride.saturating_mul(height)
            || self.uv.len() < self.uv_stride.saturating_mul(chroma_height)
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
        self.y
            .chunks(self.y_stride)
            .take(height)
            .all(|row| max(&row[..width]) <= black)
            && self
                .uv
                .chunks(self.uv_stride)
                .take(chroma_height)
                .all(|row| {
                    let row = &row[..chroma_width];
                    min(row) >= 127 && max(row) <= 129
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
pub fn decodable() -> Vec<Codec> {
    #[cfg(target_os = "macos")]
    {
        vec![Codec::Hevc, Codec::H264High, Codec::H264Baseline]
    }
    #[cfg(windows)]
    {
        if mediafoundation::MfDecoder::new().is_ok() {
            vec![Codec::H264High, Codec::H264Baseline]
        } else {
            vec![Codec::H264Baseline]
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
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
            Ok(d) => return Ok(Box::new(d)),
            Err(e) if codec == Codec::H264Baseline => {
                tracing::warn!("VideoToolbox: {e:#}; decoding in software");
            }
            Err(e) => return Err(e),
        }
    }
    #[cfg(windows)]
    {
        if codec != Codec::Hevc {
            match mediafoundation::MfDecoder::new() {
                Ok(d) => return Ok(Box::new(MediaFoundation(d))),
                Err(e) if codec == Codec::H264Baseline => {
                    tracing::warn!("Media Foundation: {e:#}; decoding in software");
                }
                Err(e) => return Err(e),
            }
        }
    }
    match codec {
        Codec::H264Baseline => Ok(Box::new(software::Software::new())),
        other => anyhow::bail!("this machine cannot decode {}", other.name()),
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

    #[test]
    fn black_detection_checks_visible_pixels_and_both_ranges() {
        let mut frame = Frame {
            width: 2,
            height: 2,
            y: vec![16, 16, 255, 16, 16, 255],
            y_stride: 3,
            uv: vec![128, 128, 255, 255],
            uv_stride: 4,
            full_range: false,
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
