//! VideoToolbox consumes the capture's IOSurface directly; only compressed bytes cross into Rust.
//!
//! Frames are submitted without waiting for the one before: the media engine
//! overlaps them, so a stream whose frame interval is shorter than one
//! frame's encode time still runs at its rate (1080p HEVC at 240 fps,
//! 3024x1964 at 120 on an M3 Pro), and each frame leaves as soon as it is done.

use super::ffi::*;
use crate::video::{Codec, EncodedVideo, VideoConfig};
use anyhow::{bail, ensure, Result};
use parking_lot::{Condvar, Mutex};
use std::{
    collections::VecDeque,
    ffi::c_void,
    ptr,
    time::{Duration, Instant},
};

/// The picture depth a session encodes: what the capture hands over, and
/// what the stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// NV12 in, 8-bit H.264 or HEVC Main out.
    Eight,
    /// P010 in, HEVC Main10 out.
    #[allow(dead_code)]
    Ten,
}
impl Depth {
    pub fn pixel_format(self) -> u32 {
        match self {
            Depth::Eight => NV12,
            Depth::Ten => P010,
        }
    }
}

/// One frame out of the encoder: its index, when it came out, and the frame
/// (`None` when the encoder dropped it).
pub type Done = (i64, Instant, Option<Result<EncodedVideo>>);
/// The callback's side of a session: capture times of the frames still in
/// the encoder, by index, and what came out, oldest first.
struct Output {
    hevc: bool,
    pending: VecDeque<(i64, Instant)>,
    done: VecDeque<Done>,
    tokens: VecDeque<(u64, i64)>,
    latest_idr: u64,
}
type Wake = Box<dyn Fn() + Send + Sync>;
struct Callback {
    output: Mutex<Output>,
    ready: Condvar,
    /// Also called when a frame comes out, after `output` is unlocked, so a
    /// caller can wait for a frame and a picture on one condition.
    wake: Mutex<Option<Wake>>,
}

pub(super) struct Encoder {
    session: Owned,
    output: Box<Callback>,
    pub hardware: bool,
    config: VideoConfig,
    depth: Depth,
    index: i64,
    /// Discard output until an IDR submitted at or after this boundary.
    recovery: Option<i64>,
    forced: Option<i64>,
    ltr: bool,
    /// No recovery may cross an IDR or a previously acknowledged base.
    ltr_floor: u64,
    acknowledged_base: u64,
    refresh: Option<(i64, u64)>,
    acknowledged: Vec<i64>,
}

impl Encoder {
    pub fn new(config: &VideoConfig, depth: Depth) -> Result<Self> {
        Self::open(
            config,
            true,
            depth,
            std::env::var_os("LATCH_MAC_LTR").is_some_and(|v| v == "1"),
        )
    }
    #[cfg(test)]
    fn create(config: &VideoConfig, allow_hardware: bool) -> Result<Self> {
        Self::open(config, allow_hardware, Depth::Eight, false)
    }
    fn open(
        config: &VideoConfig,
        allow_hardware: bool,
        depth: Depth,
        low_latency: bool,
    ) -> Result<Self> {
        ensure!(
            config.width > 0
                && config.height > 0
                && config.width.is_multiple_of(2)
                && config.height.is_multiple_of(2)
                && config.width <= i32::MAX as u32
                && config.height <= i32::MAX as u32,
            "VideoToolbox requires positive, even picture dimensions"
        );
        ensure!(
            (1..=latch_proto::control::MAX_FPS).contains(&config.fps) && config.bitrate_kbps > 0,
            "invalid frame rate or bitrate"
        );
        ensure!(
            depth == Depth::Eight || config.codec == Codec::Hevc,
            "10-bit video needs HEVC"
        );
        let mut output = Box::new(Callback {
            output: Mutex::new(Output {
                hevc: config.codec == Codec::Hevc,
                pending: VecDeque::new(),
                done: VecDeque::new(),
                tokens: VecDeque::new(),
                latest_idr: 0,
            }),
            ready: Condvar::new(),
            wake: Mutex::new(None),
        });
        let mut session = ptr::null();
        let mut last_status = 0;
        let mut required_hardware = false;
        // SAFETY: all specification values are framework constants; the boxed callback state stays
        // at a stable address until invalidate has drained callbacks in Drop.
        unsafe {
            // Not EnableLowLatencyRateControl: on Apple Silicon it doubles a
            // frame's encode time (1080p 4.7 -> 9.4 ms), drops frames (45 of
            // 60 fps) and sends a fifth of the asked-for bitrate.
            for hardware in [true, false] {
                if hardware && !allow_hardware {
                    continue;
                }
                let mut spec = vec![if hardware {
                    (
                        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                        kCFBooleanTrue,
                    )
                } else {
                    // Omitting RequireHardware alone can select the same failing
                    // hardware encoder again. Explicitly disable it for this retry.
                    (
                        kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                        kCFBooleanFalse,
                    )
                }];
                if hardware && low_latency {
                    if let Some(key) =
                        optional(c"kVTVideoEncoderSpecification_EnableLowLatencyRateControl")
                    {
                        spec.push((key, kCFBooleanTrue));
                    }
                }
                let spec = dictionary(&spec);
                last_status = VTCompressionSessionCreate(
                    ptr::null(),
                    config.width as i32,
                    config.height as i32,
                    if config.codec == Codec::Hevc {
                        u32::from_be_bytes(*b"hvc1")
                    } else {
                        u32::from_be_bytes(*b"avc1")
                    },
                    spec.0,
                    ptr::null(),
                    ptr::null(),
                    encoded,
                    (&mut *output as *mut Callback).cast(),
                    &mut session,
                );
                #[cfg(test)]
                eprintln!(
                    "create {:?} {depth:?} hardware={hardware}: {last_status}",
                    config.codec
                );
                if last_status == 0 && !session.is_null() {
                    required_hardware = hardware;
                    break;
                }
                if !session.is_null() {
                    VTCompressionSessionInvalidate(session);
                    CFRelease(session);
                    session = ptr::null();
                }
            }
        }
        check(last_status, "create VideoToolbox encoder")?;
        let mut encoder = Self {
            session: Owned::new(session)?,
            output,
            hardware: required_hardware,
            config: VideoConfig {
                color: Default::default(),
                bitrate_kbps: 0,
                ..config.clone()
            },
            depth,
            index: 0,
            recovery: None,
            forced: None,
            ltr: false,
            ltr_floor: 0,
            acknowledged_base: 0,
            refresh: None,
            acknowledged: Vec::new(),
        };
        // SAFETY: the session is live and each property receives the documented CF value type.
        unsafe {
            encoder.set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)?;
            encoder.set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
            )?;
            // On M3 Pro the quality path restores 7 dB at 1080p120 while
            // keeping pace. Above that pixel rate it misses frames (190/240
            // at 1080p, 70/120 at 3024x1964), so retain the fast path there.
            let quality = required_hardware
                && config.codec == Codec::Hevc
                && depth == Depth::Eight
                && u64::from(config.width) * u64::from(config.height)
                    <= 1920 * 1080 * 120 / u64::from(config.fps);
            let _ = VTSessionSetProperty(
                encoder.session.0,
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                if quality {
                    kCFBooleanFalse
                } else {
                    kCFBooleanTrue
                },
            );
            let _ = VTSessionSetProperty(
                encoder.session.0,
                kVTCompressionPropertyKey_MaxFrameDelayCount,
                number(0).0,
            );
            let profile = match (config.codec, depth) {
                (Codec::Hevc, Depth::Ten) => kVTProfileLevel_HEVC_Main10_AutoLevel,
                (Codec::Hevc, Depth::Eight) => kVTProfileLevel_HEVC_Main_AutoLevel,
                (Codec::H264High, _) => kVTProfileLevel_H264_High_AutoLevel,
                (Codec::H264Baseline, _) => kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
                (Codec::Av1, _) => anyhow::bail!("VideoToolbox does not encode AV1"),
            };
            if let Err(error) = encoder.set(kVTCompressionPropertyKey_ProfileLevel, profile) {
                if config.codec != Codec::H264Baseline {
                    return Err(error);
                }
                encoder.set(
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_H264_Baseline_AutoLevel,
                )?;
            }
            encoder.set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                number(i32::MAX as i64).0,
            )?;
            let _ = VTSessionSetProperty(
                encoder.session.0,
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                float_number(0.0).0,
            );
            encoder.set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                number(config.fps as i64).0,
            )?;
            // macOS 15+: the most frames a second this session can be handed,
            // which keeps the media engine clocked for it (a few per cent
            // faster at 240 fps).
            if let Some(key) = optional(c"kVTCompressionPropertyKey_MaximumRealTimeFrameRate") {
                let _ = VTSessionSetProperty(encoder.session.0, key, number(config.fps as i64).0);
            }
            // The viewer converts as BT.709 limited range, and the Windows
            // encoder says so in the stream; untagged, other decoders guess.
            // Tags only: VideoToolbox does not convert the pixels.
            for (key, value) in [
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    if config.color.is_hdr() {
                        kCVImageBufferColorPrimaries_ITU_R_2020
                    } else {
                        kCVImageBufferColorPrimaries_ITU_R_709_2
                    },
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    if config.color.is_hdr() {
                        kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ
                    } else {
                        kCVImageBufferTransferFunction_ITU_R_709_2
                    },
                ),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    if config.color.is_hdr() {
                        kCVImageBufferYCbCrMatrix_ITU_R_2020
                    } else {
                        kCVImageBufferYCbCrMatrix_ITU_R_709_2
                    },
                ),
            ] {
                if let Err(error) = encoder.set(key, value) {
                    if config.color.is_hdr() {
                        return Err(error);
                    }
                    tracing::debug!(%error, "VideoToolbox encoder does not take colour tags");
                }
            }
            if let Some(key) = optional(c"kVTCompressionPropertyKey_EnableLTR") {
                encoder.ltr = optional(c"kVTEncodeFrameOptionKey_AcknowledgedLTRTokens").is_some()
                    && optional(c"kVTEncodeFrameOptionKey_ForceLTRRefresh").is_some()
                    && optional(c"kVTSampleAttachmentKey_RequireLTRAcknowledgementToken").is_some()
                    && VTSessionSetProperty(encoder.session.0, key, kCFBooleanTrue) == 0;
                tracing::debug!(codec = ?config.codec, ltr = encoder.ltr, "VideoToolbox reference control");
            }
            encoder.bitrate(config.bitrate_kbps)?;
            check(
                VTCompressionSessionPrepareToEncodeFrames(session),
                "prepare VideoToolbox encoder",
            )?;
            let mut hardware = ptr::null();
            let hw_status = VTSessionCopyProperty(
                session,
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                ptr::null(),
                &mut hardware,
            );
            if hw_status == 0 && !hardware.is_null() {
                let value = Owned(hardware);
                encoder.hardware = CFBooleanGetValue(value.0);
            }
        }
        Ok(encoder)
    }
    fn set(&self, key: Ref, value: Ref) -> Result<()> {
        // SAFETY: private callers provide the documented type for this property on this live session.
        check(
            unsafe { VTSessionSetProperty(self.session.0, key, value) },
            "set VideoToolbox property",
        )
    }
    pub fn bitrate(&mut self, kbps: u32) -> Result<()> {
        ensure!(kbps > 0, "bitrate must be positive");
        if kbps == self.config.bitrate_kbps {
            return Ok(());
        }
        if self.config.bitrate_kbps != 0 {
            // Finish old-rate work first, so every picture submitted after
            // this encodes at the new rate. What already came out still
            // leaves, in order: later P-frames reference it, and dropping it
            // would cost an IDR at every step of a ramp.
            // SAFETY: the live session owns all outstanding submissions.
            check(
                unsafe { VTCompressionSessionCompleteFrames(self.session.0, INVALID_TIME) },
                "complete pictures before bitrate change",
            )?;
        }
        let rate = number(i64::from(kbps) * 1000);
        let bytes = number(i64::from(kbps) * 1000 * 3 / 16);
        let seconds = number(1);
        // SAFETY: CFArray retains the two numeric values; set copies or retains its argument.
        unsafe {
            self.set(kVTCompressionPropertyKey_AverageBitRate, rate.0)?;
            // VideoToolbox holds P-frames to this but not keyframes, which
            // come out 20-40x a P-frame whatever is set here (see MACOS.md).
            let limits = Owned::new(CFArrayCreate(
                ptr::null(),
                [bytes.0, seconds.0].as_ptr(),
                2,
                &kCFTypeArrayCallBacks,
            ))?;
            let status = VTSessionSetProperty(
                self.session.0,
                kVTCompressionPropertyKey_DataRateLimits,
                limits.0,
            );
            if status != 0 && status != -12900 {
                check(status, "set VideoToolbox data rate limits")?;
            }
            self.config.bitrate_kbps = kbps;
            Ok(())
        }
    }
    /// Whether a hardware session encodes a `width` × `height` keyframe
    /// without falling back to software: VideoToolbox accepts any size,
    /// and a media engine past its own limit says so only by failing over.
    pub fn hardware_encodes(codec: Codec, width: u32, height: u32) -> bool {
        let config = VideoConfig {
            color: Default::default(),
            width,
            height,
            fps: 60,
            bitrate_kbps: 20_000,
            codec,
        };
        let Ok(mut encoder) = Self::new(&config, Depth::Eight) else {
            return false;
        };
        if !encoder.hardware {
            return false;
        }
        // SAFETY: an owned, blank NV12 buffer of the probed size, alive for the encode.
        let image = unsafe {
            let empty = dictionary(&[]);
            let attrs = dictionary(&[(kCVPixelBufferIOSurfacePropertiesKey, empty.0)]);
            let mut image = ptr::null();
            if CVPixelBufferCreate(
                ptr::null(),
                width as usize,
                height as usize,
                NV12,
                attrs.0,
                &mut image,
            ) != 0
            {
                return false;
            }
            match Owned::new(image) {
                Ok(image) => image,
                Err(_) => return false,
            }
        };
        let encoded = encoder.encode(image.0, Instant::now(), true);
        matches!(encoded, Ok(Some(_))) && encoder.hardware
    }
    /// Encodes one picture and waits for it.
    pub fn encode(
        &mut self,
        image: Ref,
        captured: Instant,
        keyframe: bool,
    ) -> Result<Option<EncodedVideo>> {
        match self.encode_once(image, captured, keyframe) {
            Err(error) if self.hardware => {
                tracing::warn!(%error, "VideoToolbox hardware encode failed; retrying in software");
                self.software()?;
                self.encode_once(image, captured, true)
            }
            result => result,
        }
    }
    /// Replaces a failed hardware session with a software one; frames still
    /// in the old session are lost, so the next frame must be a keyframe.
    pub fn software(&mut self) -> Result<()> {
        // Destroy the failed session before selecting the replacement. Its
        // callback state stays alive until the old encoder is dropped below.
        unsafe {
            VTCompressionSessionInvalidate(self.session.0);
        }
        let wake = self.output.wake.lock().take();
        let index = self.index;
        *self = Self::open(&self.config, false, self.depth, false)?;
        self.index = index;
        *self.output.wake.lock() = wake;
        self.request_keyframe();
        Ok(())
    }
    fn encode_once(
        &mut self,
        image: Ref,
        captured: Instant,
        keyframe: bool,
    ) -> Result<Option<EncodedVideo>> {
        let index = self.submit(image, captured, keyframe)?;
        // SAFETY: the session is live; this returns once every frame up to index has called back.
        check(
            unsafe {
                VTCompressionSessionCompleteFrames(self.session.0, time(index, self.config.fps))
            },
            "complete picture",
        )?;
        while let Some((i, _, frame)) = self.collect(Duration::ZERO) {
            if i == index {
                return frame.transpose();
            }
        }
        Ok(None)
    }
    /// Hands one picture to the encoder without waiting for it to come out.
    pub fn submit(&mut self, image: Ref, captured: Instant, keyframe: bool) -> Result<i64> {
        let index = self.index;
        self.index += 1;
        let recovery_keyframe = self.recovery.is_some() && self.forced.is_none();
        let keyframe = keyframe || recovery_keyframe;
        if keyframe {
            self.ltr_floor = index as u64 + 1;
            self.refresh = None;
        }
        self.output
            .output
            .lock()
            .pending
            .push_back((index, captured));
        // SAFETY: the caller retains image until this returns; VideoToolbox retains it while
        // encoding. The frame refcon carries only the index, never a pointer.
        let status = unsafe {
            let mut properties = vec![(
                kVTEncodeFrameOptionKey_ForceKeyFrame,
                if keyframe {
                    kCFBooleanTrue
                } else {
                    kCFBooleanFalse
                },
            )];
            if self.refresh.is_some_and(|(boundary, _)| boundary == index) {
                properties.push((
                    optional(c"kVTEncodeFrameOptionKey_ForceLTRRefresh").unwrap(),
                    kCFBooleanTrue,
                ));
            }
            let numbers: Vec<_> = self
                .acknowledged
                .iter()
                .map(|&token| number(token))
                .collect();
            let values: Vec<_> = numbers.iter().map(|n| n.0).collect();
            let tokens = if values.is_empty() {
                None
            } else {
                Some(Owned::new(CFArrayCreate(
                    ptr::null(),
                    values.as_ptr(),
                    values.len() as isize,
                    &kCFTypeArrayCallBacks,
                ))?)
            };
            if !values.is_empty() {
                properties.push((
                    optional(c"kVTEncodeFrameOptionKey_AcknowledgedLTRTokens").unwrap(),
                    tokens.as_ref().unwrap().0,
                ));
            }
            let options = dictionary(&properties);
            VTCompressionSessionEncodeFrame(
                self.session.0,
                image,
                time(index, self.config.fps),
                time(1, self.config.fps),
                options.0,
                index as *mut c_void,
                ptr::null_mut(),
            )
        };
        if let Err(error) = check(status, "encode picture") {
            self.output
                .output
                .lock()
                .pending
                .retain(|(i, _)| *i != index);
            return Err(error);
        }
        self.acknowledged.clear();
        if recovery_keyframe {
            self.forced = Some(index);
        }
        Ok(index)
    }
    pub fn recovers(&self) -> bool {
        self.ltr
    }
    pub fn recover_from(&mut self, picture: u64) -> bool {
        if !self.ltr
            || self.recovery.is_some()
            || picture < self.ltr_floor
            || picture < self.acknowledged_base
            || picture > self.index as u64
        {
            return false;
        }
        let out = self.output.output.lock();
        if picture < out.latest_idr {
            return false;
        }
        self.acknowledged = out
            .tokens
            .iter()
            .filter(|(p, _)| *p >= self.ltr_floor && *p <= picture)
            .map(|(_, token)| *token)
            .collect();
        if self.acknowledged.is_empty() {
            return false;
        }
        self.acknowledged_base = picture;
        self.refresh = Some((self.index, picture));
        true
    }
    /// Repeated requests while an IDR is pending share the same boundary.
    pub fn request_keyframe(&mut self) {
        self.refresh = None;
        self.acknowledged.clear();
        if self.recovery.is_none() {
            self.recovery = Some(self.index);
            self.forced = None;
        }
    }
    pub fn needs_keyframe(&self) -> bool {
        self.recovery.is_some() || self.refresh.is_some()
    }
    /// The oldest usable output. During recovery, nothing before the fresh
    /// IDR may leave: those P-frames can reference pictures the viewer lost.
    pub fn collect(&mut self, wait: Duration) -> Option<Done> {
        let deadline = Instant::now() + wait;
        loop {
            let done = {
                let mut out = self.output.output.lock();
                while out.done.is_empty() {
                    if wait.is_zero()
                        || self.output.ready.wait_until(&mut out, deadline).timed_out()
                    {
                        break;
                    }
                }
                out.done.pop_front()
            }?;
            if let Some(boundary) = self.recovery {
                if done.0 < boundary {
                    continue;
                }
                match &done.2 {
                    Some(Ok(frame)) if frame.keyframe => {
                        self.recovery = None;
                        self.forced = None;
                    }
                    Some(Err(_)) => return Some(done),
                    _ => {
                        if self.forced.is_some_and(|index| done.0 >= index) {
                            // The forced picture was dropped or was not an IDR.
                            self.forced = None;
                        }
                        continue;
                    }
                }
            }
            let mut done = done;
            if let Some((boundary, base)) = self.refresh {
                if done.0 < boundary {
                    continue;
                }
                if let Some(Err(_)) = &done.2 {
                    return Some(done);
                }
                if let Some(Ok(frame)) = &mut done.2 {
                    if done.0 != boundary && !frame.keyframe {
                        // VT may omit a dropped frame's callback entirely.
                        self.request_keyframe();
                        continue;
                    }
                    if !frame.keyframe {
                        frame.recovery = Some(base);
                    }
                    self.refresh = None;
                } else {
                    // A lost refresh cannot authorize any later dependent output.
                    self.request_keyframe();
                    continue;
                }
            }
            return Some(done);
        }
    }
    /// How many submitted frames have not come out yet.
    pub fn in_flight(&self) -> usize {
        self.output.output.lock().pending.len()
    }
    /// Called (on VideoToolbox's thread) whenever a frame comes out.
    pub fn on_output(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.output.wake.lock() = Some(Box::new(wake));
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: completing every pending frame, then invalidating, leaves no
        // callback that could outlive the boxed output state.
        unsafe {
            VTCompressionSessionCompleteFrames(self.session.0, INVALID_TIME);
            VTCompressionSessionInvalidate(self.session.0);
        }
    }
}
unsafe extern "C" fn encoded(
    context: *mut c_void,
    frame: *mut c_void,
    status: i32,
    _: u32,
    sample: Ref,
) {
    // SAFETY: context points to the session's stable boxed state until invalidation completes.
    let callback = unsafe { &*context.cast::<Callback>() };
    let index = frame as i64;
    {
        let mut out = callback.output.lock();
        // Frames come out in order; any older one still listed was dropped
        // without a callback.
        let mut captured = None;
        while let Some(&(i, at)) = out.pending.front() {
            if i > index {
                break;
            }
            out.pending.pop_front();
            if i == index {
                captured = Some(at);
            }
        }
        let captured = captured.unwrap_or_else(Instant::now);
        let hevc = out.hevc;
        let frame = callback_frame(status, sample.is_null(), || {
            // SAFETY: the callback's sample and all of its borrowed data stay live for this scope.
            unsafe {
                if let Some(key) =
                    optional(c"kVTSampleAttachmentKey_RequireLTRAcknowledgementToken")
                {
                    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
                    let token = if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
                        CFDictionaryGetValue(CFArrayGetValueAtIndex(attachments, 0), key)
                    } else {
                        ptr::null()
                    };
                    let mut value = 0i64;
                    if !token.is_null()
                        && CFNumberGetValue(token, 4, (&mut value as *mut i64).cast())
                    {
                        out.tokens.push_back((index as u64 + 1, value));
                        while out.tokens.len() > 256 {
                            out.tokens.pop_front();
                        }
                    }
                }
                let mut frame = annex_b(sample, hevc, captured)?;
                frame.picture = index as u64 + 1;
                Ok(frame)
            }
        });
        if matches!(&frame, Some(Ok(f)) if f.keyframe) {
            out.latest_idr = index as u64 + 1;
        }
        out.done.push_back((index, Instant::now(), frame));
    }
    callback.ready.notify_all();
    if let Some(wake) = &*callback.wake.lock() {
        wake();
    }
}

fn callback_frame(
    status: i32,
    dropped: bool,
    read: impl FnOnce() -> Result<EncodedVideo>,
) -> Option<Result<EncodedVideo>> {
    if status != 0 {
        Some(check(status, "VideoToolbox output").and_then(|()| read()))
    } else if dropped {
        None
    } else {
        Some(read())
    }
}

unsafe fn annex_b(sample: Ref, hevc: bool, captured: Instant) -> Result<EncodedVideo> {
    // SAFETY: caller supplies a live encoded sample; all borrowed pointers are copied before return.
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
        let keyframe = attachments.is_null() || CFArrayGetCount(attachments) == 0 || {
            let not_sync = CFDictionaryGetValue(
                CFArrayGetValueAtIndex(attachments, 0),
                kCMSampleAttachmentKey_NotSync,
            );
            not_sync.is_null() || !CFBooleanGetValue(not_sync)
        };
        let format = CMSampleBufferGetFormatDescription(sample);
        ensure!(!format.is_null(), "encoded picture has no format");
        let get = if hevc {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex
        } else {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex
        };
        let mut header = 0;
        let mut count = 0;
        check(
            get(
                format,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut count,
                &mut header,
            ),
            "read parameter set count",
        )?;
        ensure!((1..=4).contains(&header), "invalid NAL length width");
        let block = CMSampleBufferGetDataBuffer(sample);
        ensure!(!block.is_null(), "encoded picture has no data");
        let total_length = CMBlockBufferGetDataLength(block);
        let mut data = Vec::with_capacity(total_length + (if keyframe { 128 } else { 0 }));
        if keyframe {
            for i in 0..count {
                let mut p = ptr::null();
                let mut size = 0;
                check(
                    get(
                        format,
                        i,
                        &mut p,
                        &mut size,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    ),
                    "read parameter set",
                )?;
                ensure!(!p.is_null() && size > 0, "empty parameter set");
                data.extend_from_slice(&[0, 0, 0, 1]);
                data.extend_from_slice(std::slice::from_raw_parts(p, size));
            }
        }
        let mut length_at_offset = 0;
        let mut block_total = 0;
        let mut data_pointer = ptr::null_mut();
        let status = CMBlockBufferGetDataPointer(
            block,
            0,
            &mut length_at_offset,
            &mut block_total,
            &mut data_pointer,
        );
        if status == 0 && length_at_offset == block_total && !data_pointer.is_null() {
            // Contiguous buffer: convert to Annex B directly from block buffer memory with a single copy.
            let slice = std::slice::from_raw_parts(data_pointer, block_total);
            append_nals(slice, header as usize, &mut data)?;
        } else {
            // Non-contiguous fallback
            let mut bytes = vec![0; total_length];
            check(
                CMBlockBufferCopyDataBytes(block, 0, bytes.len(), bytes.as_mut_ptr()),
                "read encoded bytes",
            )?;
            append_nals(&bytes, header as usize, &mut data)?;
        }
        Ok(EncodedVideo {
            generation: 0,
            data,
            keyframe,
            captured,
            picture: 0,
            recovery: None,
        })
    }
}
fn append_nals(mut bytes: &[u8], header: usize, out: &mut Vec<u8>) -> Result<()> {
    ensure!((1..=4).contains(&header), "invalid NAL length width");
    while !bytes.is_empty() {
        ensure!(bytes.len() >= header, "truncated NAL length");
        let len = bytes[..header]
            .iter()
            .fold(0usize, |n, b| (n << 8) | usize::from(*b));
        bytes = &bytes[header..];
        if len == 0 || len > bytes.len() {
            bail!("truncated or empty NAL");
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&bytes[..len]);
        bytes = &bytes[len..];
    }
    Ok(())
}

#[cfg(test)]
mod tests;
