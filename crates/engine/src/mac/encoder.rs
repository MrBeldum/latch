//! VideoToolbox consumes the capture's IOSurface directly; only compressed bytes cross into Rust.

use super::ffi::*;
use crate::video::{Codec, EncodedVideo, VideoConfig};
use anyhow::{bail, ensure, Result};
use parking_lot::Mutex;
use std::{ffi::c_void, ptr, time::Instant};

struct Output {
    captured: Instant,
    hevc: bool,
    frame: Option<Result<EncodedVideo>>,
}

pub(super) struct Encoder {
    session: Owned,
    output: Box<Mutex<Output>>,
    pub hardware: bool,
    config: VideoConfig,
    index: i64,
}

impl Encoder {
    pub fn new(config: &VideoConfig) -> Result<Self> {
        Self::create(config, true)
    }
    fn create(config: &VideoConfig, allow_hardware: bool) -> Result<Self> {
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
            (1..=240).contains(&config.fps) && config.bitrate_kbps > 0,
            "invalid frame rate or bitrate"
        );
        let mut output = Box::new(Mutex::new(Output {
            captured: Instant::now(),
            hevc: config.codec == Codec::Hevc,
            frame: None,
        }));
        let mut session = ptr::null();
        let mut last_status = 0;
        let mut required_hardware = false;
        // SAFETY: all specification values are framework constants; the boxed callback state stays
        // at a stable address until invalidate has drained callbacks in Drop.
        unsafe {
            for (hardware, low_latency) in [(true, true), (true, false), (false, false)] {
                if hardware && !allow_hardware {
                    continue;
                }
                let mut entries = if hardware {
                    vec![(
                        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                        kCFBooleanTrue,
                    )]
                } else {
                    // Omitting RequireHardware alone can select the same failing
                    // hardware encoder again. Explicitly disable it for this retry.
                    vec![(
                        kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                        kCFBooleanFalse,
                    )]
                };
                if low_latency {
                    entries.push((
                        kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                        kCFBooleanTrue,
                    ));
                }
                let spec = dictionary(&entries);
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
                    (&mut *output as *mut Mutex<Output>).cast(),
                    &mut session,
                );
                #[cfg(test)]
                eprintln!(
                    "create {:?} hardware={hardware} low_latency={low_latency}: {last_status}",
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
            config: config.clone(),
            index: 0,
        };
        // SAFETY: the session is live and each property receives the documented CF value type.
        unsafe {
            encoder.set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)?;
            encoder.set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
            )?;
            let profile = match config.codec {
                Codec::Hevc => kVTProfileLevel_HEVC_Main_AutoLevel,
                Codec::H264High => kVTProfileLevel_H264_High_AutoLevel,
                Codec::H264Baseline => kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
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
            encoder.set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                number(config.fps as i64).0,
            )?;
            // The viewer converts as BT.709 limited range, and the Windows
            // encoder says so in the stream; untagged, other decoders guess.
            // Tags only: VideoToolbox does not convert the pixels.
            for (key, value) in [
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                ),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ),
            ] {
                if let Err(error) = encoder.set(key, value) {
                    tracing::debug!(%error, "VideoToolbox encoder does not take colour tags");
                }
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
            #[cfg(test)]
            eprintln!(
                "hardware property status={hw_status} null={}",
                hardware.is_null()
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
        let rate = number(i64::from(kbps) * 1000);
        let bytes = number(i64::from(kbps) * 1000 * 3 / 16);
        let seconds = number(1);
        // SAFETY: CFArray retains the two numeric values; set copies or retains its argument.
        unsafe {
            self.set(kVTCompressionPropertyKey_AverageBitRate, rate.0)?;
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
            if status == -12900 {
                // kVTPropertyNotSupportedErr (notably software HEVC)
                tracing::debug!(
                    "VideoToolbox encoder does not support peak rate limits; using average bitrate"
                );
            } else {
                check(status, "set VideoToolbox data rate limits")?;
            }
            self.config.bitrate_kbps = kbps;
            Ok(())
        }
    }
    pub fn encode(
        &mut self,
        image: Ref,
        captured: Instant,
        keyframe: bool,
    ) -> Result<Option<EncodedVideo>> {
        match self.encode_once(image, captured, keyframe) {
            Err(error) if self.hardware => {
                tracing::warn!(%error, "VideoToolbox hardware encode failed; retrying in software");
                self.software_retry(image, captured)
            }
            result => result,
        }
    }
    fn software_retry(&mut self, image: Ref, captured: Instant) -> Result<Option<EncodedVideo>> {
        // Destroy the failed session before selecting the replacement. Both its
        // callback state and this call's retained image stay alive during invalidation.
        unsafe {
            VTCompressionSessionInvalidate(self.session.0);
        }
        *self = Self::create(&self.config, false)?;
        self.encode_once(image, captured, true)
    }
    fn encode_once(
        &mut self,
        image: Ref,
        captured: Instant,
        keyframe: bool,
    ) -> Result<Option<EncodedVideo>> {
        {
            let mut out = self.output.lock();
            out.captured = captured;
            out.frame = None;
        }
        // SAFETY: capture retains image throughout this call. Completing the submitted PTS drains
        // the callback before the next submission changes its capture timestamp.
        unsafe {
            let options = dictionary(&[(
                kVTEncodeFrameOptionKey_ForceKeyFrame,
                if keyframe {
                    kCFBooleanTrue
                } else {
                    kCFBooleanFalse
                },
            )]);
            check(
                VTCompressionSessionEncodeFrame(
                    self.session.0,
                    image,
                    time(self.index, self.config.fps),
                    time(1, self.config.fps),
                    options.0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                "encode picture",
            )?;
            check(
                VTCompressionSessionCompleteFrames(
                    self.session.0,
                    time(self.index, self.config.fps),
                ),
                "complete picture",
            )?;
        }
        self.index += 1;
        self.output.lock().frame.take().transpose()
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: invalidation prevents any callback from outliving the boxed output state.
        unsafe { VTCompressionSessionInvalidate(self.session.0) }
    }
}
unsafe extern "C" fn encoded(
    context: *mut c_void,
    _: *mut c_void,
    status: i32,
    _: u32,
    sample: Ref,
) {
    // SAFETY: context points to the session's stable boxed Mutex until invalidation completes.
    let out = unsafe { &*context.cast::<Mutex<Output>>() };
    let mut out = out.lock();
    out.frame = callback_frame(status, sample.is_null(), || {
        // SAFETY: the callback's sample and all of its borrowed data stay live for this scope.
        unsafe { annex_b(sample, out.hevc, out.captured) }
    });
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
        let mut data = Vec::new();
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
        let block = CMSampleBufferGetDataBuffer(sample);
        ensure!(!block.is_null(), "encoded picture has no data");
        let mut bytes = vec![0; CMBlockBufferGetDataLength(block)];
        check(
            CMBlockBufferCopyDataBytes(block, 0, bytes.len(), bytes.as_mut_ptr()),
            "read encoded bytes",
        )?;
        append_nals(&bytes, header as usize, &mut data)?;
        Ok(EncodedVideo {
            data,
            keyframe,
            captured,
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
