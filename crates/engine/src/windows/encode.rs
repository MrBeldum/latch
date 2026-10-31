use super::capture::Gpu;
use super::{display, mf};
use crate::bitstream::{vbv_bits, Format, Sequence};
use crate::video::{Codec, VideoConfig};
use anyhow::{bail, Context, Result};
use std::{
    mem::ManuallyDrop,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::{implement, Interface, GUID, VARIANT},
    Win32::{
        Graphics::{
            Direct3D11::{ID3D11Texture2D, D3D11_BIND_RENDER_TARGET},
            Dxgi::Common::DXGI_FORMAT_NV12,
        },
        Media::MediaFoundation::*,
        System::Com::*,
    },
};

// One outstanding callback, one bounded wakeup. The MF callback never waits
// for the video worker and passes only owned status/type values across threads.
#[implement(IMFAsyncCallback)]
struct EventCallback {
    generator: IMFMediaEventGenerator,
    reply: mpsc::SyncSender<windows::core::Result<u32>>,
}
impl IMFAsyncCallback_Impl for EventCallback_Impl {
    fn GetParameters(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_NOTIMPL,
        ))
    }
    fn Invoke(&self, result: Option<&IMFAsyncResult>) -> windows::core::Result<()> {
        // SAFETY: MF supplies the result for this generator's BeginGetEvent.
        let event = unsafe {
            self.generator.EndGetEvent(result).and_then(|event| {
                event.GetStatus()?.ok()?;
                event.GetType()
            })
        };
        let _ = self.reply.try_send(event);
        Ok(())
    }
}
/// Set `LATCH_WINDOWS_HEVC=1` to offer HEVC from a GPU's Media Foundation
/// encoder. Off by default until it has run on real AMD, NVIDIA and Intel
/// hardware; with it off nothing about H.264 changes.
fn hevc_enabled() -> bool {
    std::env::var_os("LATCH_WINDOWS_HEVC").is_some_and(|v| v == "1")
}
/// An HEVC encoder that did not work stays off until the process restarts,
/// so the next viewer is offered H.264 instead of failing the same way.
static HEVC_FAILED: AtomicBool = AtomicBool::new(false);
/// A probe encoded a keyframe through the whole HEVC path.
static HEVC_PROVEN: AtomicBool = AtomicBool::new(false);
pub(super) fn hevc_failed(why: &anyhow::Error) {
    tracing::debug!("HEVC encoder is not used again: {why:#}");
    HEVC_FAILED.store(true, Ordering::Relaxed);
}

pub(super) fn media_type(subtype: &GUID, config: &VideoConfig) -> Result<IMFMediaType> {
    // SAFETY: MF is started on the calling worker; the returned type is owned.
    unsafe {
        let ty = mf::MFCreateMediaType()?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        ty.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(config.width) << 32) | u64::from(config.height),
        )?;
        ty.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(config.fps) << 32) | 1)?;
        ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(ty)
    }
}
#[cfg(test)]
pub(super) fn memory_sample(bytes: &[u8]) -> Result<IMFSample> {
    let length = u32::try_from(bytes.len()).context("MF sample exceeds 4 GiB")?;
    // SAFETY: Buffer capacity equals length, Lock is balanced before return.
    unsafe {
        let sample = mf::MFCreateSample()?;
        let buffer = mf::MFCreateMemoryBuffer(length)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(length)?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}
/// An NV12 sample of `width` × `height` made from a mapped surface whose
/// rows are `stride` bytes apart: the luma rows, then the chroma rows, each
/// `width` bytes, copied once.
pub(super) fn nv12_sample(
    surface: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Result<IMFSample> {
    let rows = height * 3 / 2;
    anyhow::ensure!(
        stride >= width && surface.len() >= stride * (rows - 1) + width,
        "NV12 surface smaller than {width}×{height}"
    );
    let length = u32::try_from(width * rows).context("MF sample exceeds 4 GiB")?;
    // SAFETY: Buffer capacity equals length, Lock is balanced before return.
    unsafe {
        let sample = mf::MFCreateSample()?;
        let buffer = mf::MFCreateMemoryBuffer(length)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        let tight = std::slice::from_raw_parts_mut(data, length as usize);
        for (dst, row) in tight.chunks_exact_mut(width).zip(surface.chunks(stride)) {
            dst.copy_from_slice(&row[..width]);
        }
        buffer.Unlock()?;
        buffer.SetCurrentLength(length)?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}
fn activations(flags: MFT_ENUM_FLAG, format: Format) -> Result<Vec<(IMFActivate, String)>> {
    let (mut array, mut count) = (std::ptr::null_mut(), 0);
    let filter = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(format),
    };
    // SAFETY: MFTEnumEx allocates an array of owned activation references. Each
    // slot is moved out once and the array itself is freed with CoTaskMemFree.
    unsafe {
        mf::MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&filter),
            &mut array,
            &mut count,
        )
        .with_context(|| format!("enumerate {} encoders", label(format)))?;
        let mut out = Vec::new();
        for i in 0..count as usize {
            if let Some(activate) = array.add(i).read() {
                let name = (|| -> windows::core::Result<String> {
                    let length = activate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute)?;
                    let mut text = vec![0; length as usize + 1];
                    activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut text, None)?;
                    Ok(String::from_utf16_lossy(&text[..length as usize]))
                })()
                .unwrap_or_else(|_| format!("hardware {} encoder", label(format)));
                out.push((activate, name));
            }
        }
        CoTaskMemFree(Some(array.cast()));
        Ok(out)
    }
}
fn subtype(format: Format) -> GUID {
    match format {
        Format::H264 => MFVideoFormat_H264,
        Format::Hevc => MFVideoFormat_HEVC,
    }
}
fn label(format: Format) -> &'static str {
    match format {
        Format::H264 => "H.264",
        Format::Hevc => "HEVC",
    }
}
fn format_of(codec: Codec) -> Format {
    if codec == Codec::Hevc {
        Format::Hevc
    } else {
        Format::H264
    }
}
fn hardware(format: Format) -> Result<Vec<(IMFActivate, String)>> {
    activations(MFT_ENUM_FLAG_HARDWARE, format)
}
pub(super) fn available() -> bool {
    if hardware(Format::H264).is_ok_and(|v| !v.is_empty()) {
        return true;
    }
    // SAFETY: This probe runs in an initialized MTA with MF started.
    unsafe {
        CoCreateInstance::<_, IMFTransform>(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
            .is_ok()
    }
}
/// HEVC may be opened: enabled, and not given up on.
pub(super) fn hevc_allowed() -> bool {
    hevc_enabled() && !HEVC_FAILED.load(Ordering::Relaxed)
}
/// Whether an HEVC encoder is worth offering: enabled, a GPU has one, and
/// it has encoded a keyframe through the whole path here (activation,
/// configuration, one frame, parameter sets in band). The probe runs once
/// per process when it succeeds.
pub(super) fn hevc_available() -> bool {
    if !hevc_allowed() {
        return false;
    }
    if HEVC_PROVEN.load(Ordering::Relaxed) {
        return true;
    }
    if !hardware(Format::Hevc).is_ok_and(|v| !v.is_empty()) {
        return false;
    }
    let _dpi = display::DpiAwareness::physical();
    // No display or device now says nothing about the encoder: try again at
    // the next connection.
    let Ok(output) = display::select() else {
        return false;
    };
    let Ok(gpu) = Gpu::new(&output.adapter) else {
        return false;
    };
    let config = VideoConfig {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_kbps: 10_000,
        codec: Codec::Hevc,
    };
    match probe_keyframe(&config, &gpu) {
        Ok(()) => {
            HEVC_PROVEN.store(true, Ordering::Relaxed);
            true
        }
        Err(e) => {
            hevc_failed(&e);
            false
        }
    }
}
fn probe_keyframe(config: &VideoConfig, gpu: &Gpu) -> Result<()> {
    let mut encoder = Encoder::new(config, gpu)?;
    let texture = gpu.texture(
        config.width,
        config.height,
        DXGI_FORMAT_NV12,
        D3D11_BIND_RENDER_TARGET.0 as u32,
        false,
    )?;
    let sample = Encoder::texture_sample(&texture)?;
    for _ in 0..4 {
        if let Some((_, idr)) = encoder.encode(&sample, true)? {
            anyhow::ensure!(idr, "HEVC encoder did not make a keyframe when asked");
            return Ok(());
        }
    }
    bail!("HEVC encoder produced no output")
}

/// Toward speed (0 is fastest, 100 best quality) for Microsoft's CPU
/// encoder, which is the bottleneck of its own pipeline. GPU encoders keep
/// their default: low-latency mode already picks their fast presets.
const QUALITY_VS_SPEED_SOFTWARE: u32 = 20;

pub(super) struct Encoder {
    transform: IMFTransform,
    api: ICodecAPI,
    events: Option<IMFMediaEventGenerator>,
    input_credit: u32,
    output_credit: u32,
    // The manager must outlive the MFT that was given its interface pointer.
    _manager: Option<IMFDXGIDeviceManager>,
    pub hardware: bool,
    pub name: String,
    duration: i64,
    time: i64,
    seq: Sequence,
    activation: Option<IMFActivate>,
    /// Reused CPU output with growth when an MFT underreports its size.
    output_sample: Option<IMFSample>,
    output_capacity: u32,
    kbps: u32,
    fps: u32,
}
impl Encoder {
    pub fn new(config: &VideoConfig, gpu: &Gpu) -> Result<Self> {
        let format = format_of(config.codec);
        let label = label(format);
        for (activation, name) in hardware(format).unwrap_or_default() {
            // SAFETY: Activation is performed on the worker's MTA.
            let transform = unsafe { activation.ActivateObject::<IMFTransform>() };
            let result = transform
                .context("activate hardware encoder")
                .and_then(|t| Self::configure(t, config, Some(gpu), name, true));
            match result {
                Ok(mut encoder) => {
                    encoder.activation = Some(activation);
                    return Ok(encoder);
                }
                Err(e) => {
                    tracing::debug!("hardware {label} MFT rejected configuration: {e:#}");
                    // SAFETY: Releases resources held by this failed activation.
                    unsafe {
                        let _ = activation.ShutdownObject();
                    }
                }
            }
        }
        if format == Format::Hevc {
            bail!("no hardware HEVC encoder accepted the configuration");
        }
        Self::software(config)
    }
    pub fn software(config: &VideoConfig) -> Result<Self> {
        // SAFETY: Called only while the worker owns a COM/MF runtime guard.
        let transform =
            unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
                .context("create Microsoft H.264 encoder")?;
        let name = activations(MFT_ENUM_FLAG_SYNCMFT, Format::H264)
            .unwrap_or_default()
            .into_iter()
            .find_map(|(activation, name)| {
                // SAFETY: This activation's attribute store is owned on this MTA.
                (unsafe { activation.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok()
                    == Some(CLSID_MSH264EncoderMFT))
                .then_some(name)
            })
            .unwrap_or_else(|| "Microsoft H.264 Video Encoder MFT".into());
        Self::configure(transform, config, None, name, false)
    }
    /// Sets a property the encoder may lack.
    fn optional(api: &ICodecAPI, what: &str, key: &GUID, value: VARIANT) {
        // SAFETY: The API belongs to the calling worker's transform.
        if let Err(e) = unsafe { api.SetValue(key, &value) } {
            tracing::debug!("encoder {what} not set: {e}");
        }
    }
    /// Rate control for a mean of `kbps`: CBR, capped at the same rate, with
    /// a buffer of about a frame and a half. A driver's default buffer is
    /// often a second long, and lets a keyframe or a burst of motion run to
    /// many times the average: a burst a link that carries the bitrate
    /// delivers late, and loses to its queue.
    fn rate(api: &ICodecAPI, kbps: u32, fps: u32) {
        let bps = kbps.saturating_mul(1000);
        Self::optional(
            api,
            "peak bit rate",
            &CODECAPI_AVEncCommonMaxBitRate,
            bps.into(),
        );
        Self::optional(
            api,
            "buffer size",
            &CODECAPI_AVEncCommonBufferSize,
            vbv_bits(kbps, fps).into(),
        );
    }
    fn configure(
        transform: IMFTransform,
        config: &VideoConfig,
        gpu: Option<&Gpu>,
        name: String,
        hardware: bool,
    ) -> Result<Self> {
        let format = format_of(config.codec);
        // SAFETY: All transform calls are serialized on its owning worker.
        unsafe {
            let attributes = transform.GetAttributes().context("encoder attributes")?;
            let asynchronous = attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if asynchronous {
                attributes
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .context("unlock asynchronous encoder")?;
            }
            let events = if asynchronous {
                Some(
                    transform
                        .cast::<IMFMediaEventGenerator>()
                        .context("encoder event generator")?,
                )
            } else {
                None
            };
            let manager = if let Some(gpu) = gpu {
                if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                    bail!("hardware MFT does not accept D3D11 textures");
                }
                let (mut token, mut manager) = (0, None);
                mf::MFCreateDXGIDeviceManager(&mut token, &mut manager)
                    .context("create encoder DXGI device manager")?;
                let manager = manager.context("missing DXGI manager")?;
                manager
                    .ResetDevice(&gpu.device, token)
                    .context("set encoder D3D11 device")?;
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .context("give encoder the DXGI manager")?;
                Some(manager)
            } else {
                None
            };
            let api: ICodecAPI = transform.cast().context("encoder ICodecAPI")?;
            api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true))
                .context("encoder low latency")?;
            api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            )
            .context("encoder CBR")?;
            api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(config.bitrate_kbps.saturating_mul(1000)),
            )
            .context("encoder bitrate")?;
            Self::rate(&api, config.bitrate_kbps, config.fps);
            if !hardware {
                // The CPU encoder is the bottleneck of its own pipeline.
                Self::optional(
                    &api,
                    "quality vs speed",
                    &CODECAPI_AVEncCommonQualityVsSpeed,
                    QUALITY_VS_SPEED_SOFTWARE.into(),
                );
            }
            api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32))
                .context("disable B pictures")?;
            if api
                .SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(u32::MAX))
                .is_err()
            {
                let (mut min, mut max, mut step) =
                    (VARIANT::default(), VARIANT::default(), VARIANT::default());
                api.GetParameterRange(&CODECAPI_AVEncMPVGOPSize, &mut min, &mut max, &mut step)
                    .context("encoder GOP range")?;
                let (min, max, step) = (
                    u32::try_from(&min)?,
                    u32::try_from(&max)?,
                    u32::try_from(&step)?.max(1),
                );
                anyhow::ensure!(max >= min, "invalid encoder GOP range");
                let longest = max - (max - min) % step;
                api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(longest))
                    .context("encoder longest supported GOP")?;
            }
            if format == Format::H264 {
                // CABAC saves bits at the same quality; Constrained Baseline
                // has none.
                Self::optional(
                    &api,
                    "CABAC",
                    &CODECAPI_AVEncH264CABACEnable,
                    (config.codec == Codec::H264High).into(),
                );
            }
            let output = media_type(&subtype(format), config)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_kbps.saturating_mul(1000))?;
            if format == Format::Hevc {
                output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
                transform
                    .SetOutputType(0, &output, 0)
                    .context("encoder HEVC output type")?;
            } else {
                let profile = if config.codec == Codec::H264High {
                    eAVEncH264VProfile_High
                } else {
                    eAVEncH264VProfile_ConstrainedBase
                };
                output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32)?;
                if let Err(e) = transform.SetOutputType(0, &output, 0) {
                    if config.codec != Codec::H264Baseline {
                        return Err(e).context("encoder H.264 output type");
                    }
                    output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
                    transform
                        .SetOutputType(0, &output, 0)
                        .context("encoder baseline output type")?;
                }
            }
            transform
                .SetInputType(0, &media_type(&MFVideoFormat_NV12, config)?, 0)
                .context("encoder NV12 input type")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self {
                transform,
                api,
                events,
                input_credit: 0,
                output_credit: 0,
                _manager: manager,
                hardware,
                name: format!("Media Foundation {} ({name})", label(format)),
                duration: 10_000_000 / i64::from(config.fps),
                time: 0,
                seq: Sequence::new(format),
                activation: None,
                output_sample: None,
                output_capacity: 0,
                kbps: config.bitrate_kbps,
                fps: config.fps,
            })
        }
    }
    /// A new mean rate, with the rate buffer kept at a frame and a half of
    /// it. The buffer and cap are best effort; the mean is not.
    pub fn bitrate(&mut self, kbps: u32) -> Result<()> {
        if kbps == self.kbps {
            return Ok(());
        }
        // SAFETY: The API belongs to this worker's transform.
        unsafe {
            self.api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(kbps.saturating_mul(1000)),
            )
        }
        .context("update encoder bitrate")?;
        Self::rate(&self.api, kbps, self.fps);
        self.kbps = kbps;
        Ok(())
    }
    /// Wait for one callback, bounded by the frame deadline.
    fn event(&mut self, deadline: Instant) -> Result<()> {
        let events = self
            .events
            .as_ref()
            .context("missing asynchronous event generator")?;
        let (reply, receive) = mpsc::sync_channel(1);
        let callback: IMFAsyncCallback = EventCallback {
            generator: events.clone(),
            reply,
        }
        .into();
        // SAFETY: MF retains the callback until Invoke; no apartment state crosses it.
        unsafe { events.BeginGetEvent(&callback, None) }.context("encoder BeginGetEvent")?;
        let kind = receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("Media Foundation encoder event timed out")?
            .context("asynchronous encoder event failed")?;
        if kind == METransformNeedInput.0 as u32 {
            self.input_credit = self.input_credit.saturating_add(1);
        }
        if kind == METransformHaveOutput.0 as u32 {
            self.output_credit = self.output_credit.saturating_add(1);
        }
        if kind == MEError.0 as u32 {
            bail!("encoder reported MEError");
        }
        Ok(())
    }
    pub fn encode(
        &mut self,
        sample: &IMFSample,
        keyframe: bool,
    ) -> Result<Option<(Vec<u8>, bool)>> {
        let deadline = Instant::now() + Duration::from_secs(3);
        if self.events.is_some() {
            while self.input_credit == 0 {
                self.event(deadline)?;
            }
            self.input_credit -= 1;
        }
        // SAFETY: The input sample remains referenced by the transform until it
        // finishes. The caller does not reuse a texture until the encoder has
        // returned the frames given after it (see Converter's ring).
        unsafe {
            if keyframe {
                self.api
                    .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32))
                    .context("force encoder IDR")?;
            }
            sample.SetSampleTime(self.time)?;
            sample.SetSampleDuration(self.duration)?;
            self.time += self.duration;
            self.transform
                .ProcessInput(0, sample, 0)
                .context("encoder ProcessInput")?;
        }
        if self.events.is_some() {
            while self.output_credit == 0 {
                self.event(deadline)?;
            }
            self.output_credit -= 1;
        }
        self.output()
    }
    fn output(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        // SAFETY: ABI fields use ManuallyDrop, so release samples and event
        // collections on every HRESULT, including NEED_MORE_INPUT/STREAM_CHANGE.
        unsafe {
            for _ in 0..16 {
                let info = self.transform.GetOutputStreamInfo(0)?;
                let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                    let required = info.cbSize.max(self.output_capacity).max(1);
                    if self.output_sample.is_none() || self.output_capacity < required {
                        let sample = mf::MFCreateSample()?;
                        sample.AddBuffer(&mf::MFCreateAlignedMemoryBuffer(
                            required,
                            info.cbAlignment.saturating_sub(1),
                        )?)?;
                        self.output_sample = Some(sample);
                        self.output_capacity = required;
                    }
                    let sample = self
                        .output_sample
                        .as_ref()
                        .context("missing output sample")?;
                    sample.GetBufferByIndex(0)?.SetCurrentLength(0)?;
                    Some(sample.clone())
                } else {
                    None
                };
                let mut output = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(sample),
                    ..Default::default()
                };
                let mut status = 0;
                let result =
                    self.transform
                        .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
                let sample = ManuallyDrop::take(&mut output.pSample);
                ManuallyDrop::drop(&mut output.pEvents);
                match result {
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                    Err(e) if e.code() == MF_E_BUFFERTOOSMALL => {
                        self.output_capacity = grown_output_capacity(
                            self.output_capacity,
                            self.transform.GetOutputStreamInfo(0)?.cbSize,
                        )?;
                        self.output_sample = None;
                        continue;
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.output_sample = None;
                        let ty = self.transform.GetOutputAvailableType(0, 0)?;
                        self.transform.SetOutputType(0, &ty, 0)?;
                        // The new type may need a bigger sample and has its
                        // own sequence header.
                        self.seq.clear();
                        continue;
                    }
                    Err(e) => return Err(e).context("encoder ProcessOutput"),
                    Ok(()) => {}
                }
                if output.dwStatus == MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32 {
                    return Ok(None);
                }
                let sample = sample.context("encoder returned no output sample")?;
                let buffer = sample.ConvertToContiguousBuffer()?;
                let (mut data, mut length) = (std::ptr::null_mut(), 0);
                buffer.Lock(&mut data, None, Some(&mut length))?;
                // One copy, out of the encoder's buffer into the access unit.
                let result = self
                    .seq
                    .assemble(std::slice::from_raw_parts(data, length as usize), || {
                        sequence_header(&self.transform)
                    });
                buffer.Unlock()?;
                return result.map(Some);
            }
        }
        bail!("encoder exceeded bounded output retries")
    }
    pub fn texture_sample(texture: &ID3D11Texture2D) -> Result<IMFSample> {
        // SAFETY: The surface buffer retains a COM reference to the owned texture.
        unsafe {
            let sample = mf::MFCreateSample()?;
            let buffer = mf::MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            buffer.SetCurrentLength(buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?)?;
            sample.AddBuffer(&buffer)?;
            Ok(sample)
        }
    }
}
/// The parameter sets the encoder keeps in its output type.
fn sequence_header(transform: &IMFTransform) -> Option<Vec<u8>> {
    // SAFETY: The transform belongs to the calling worker; the blob is copied.
    unsafe {
        let ty = transform.GetOutputCurrentType(0).ok()?;
        let size = ty.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut header = vec![0; size as usize];
        ty.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)
            .ok()?;
        Some(header)
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: Teardown occurs on the same MTA, before the MF runtime shuts down.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(activation) = &self.activation {
                let _ = activation.ShutdownObject();
            }
        }
    }
}
fn grown_output_capacity(current: u32, advertised: u32) -> Result<u32> {
    let size = current.saturating_mul(2).max(advertised).max(1);
    anyhow::ensure!(size <= 64 * 1024 * 1024, "encoder output exceeds 64 MiB");
    Ok(size)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_sample_drops_the_surface_padding() -> Result<()> {
        let runtime = match super::super::runtime::Runtime::new(true) {
            Ok(r) => r,
            Err(e) => {
                if super::super::unavailable(&e, "Media Foundation") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        // 4 × 4 picture: 6 rows, each 4 bytes in a 8-byte pitch.
        let mut surface = vec![0xee; 8 * 6];
        for (row, bytes) in surface.chunks_mut(8).enumerate() {
            bytes[..4].fill(row as u8);
        }
        let sample = nv12_sample(&surface, 8, 4, 4)?;
        // SAFETY: The buffer is locked and unlocked on this thread.
        let bytes = unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let (mut data, mut length) = (std::ptr::null_mut(), 0);
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
            buffer.Unlock()?;
            bytes
        };
        assert_eq!(bytes.len(), 24);
        assert_eq!(bytes[..4], [0; 4]);
        assert_eq!(bytes[20..], [5; 4]);
        assert!(nv12_sample(&surface[..20], 8, 4, 4).is_err());
        drop(runtime);
        Ok(())
    }

    #[test]
    fn software_encoder_takes_a_new_bitrate_between_frames() -> Result<()> {
        let _runtime = match super::super::runtime::Runtime::new(true) {
            Ok(r) => r,
            Err(e) => {
                if super::super::unavailable(&e, "Media Foundation") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        let config = VideoConfig {
            width: 160,
            height: 96,
            fps: 30,
            bitrate_kbps: 4000,
            codec: Codec::H264High,
        };
        let mut encoder = match Encoder::software(&config) {
            Ok(e) => e,
            Err(e) => {
                if super::super::unavailable(&e, "software H.264 encoder") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        let picture = vec![100; 160 * 96 * 3 / 2];
        let mut keyframes = 0;
        let mut outputs = 0;
        for i in 0..24 {
            if i == 8 {
                encoder.bitrate(1500)?;
                encoder.bitrate(1500)?;
            }
            if let Some((data, idr)) = encoder.encode(&memory_sample(&picture)?, i == 0)? {
                outputs += 1;
                keyframes += usize::from(idr);
                assert!(!data.is_empty());
            }
        }
        assert!(keyframes >= 1 && outputs >= 12, "{keyframes} {outputs}");
        Ok(())
    }
}

#[cfg(test)]
mod output_tests {
    #[test]
    fn short_output_buffer_grows_even_if_driver_size_is_stale() -> anyhow::Result<()> {
        assert_eq!(super::grown_output_capacity(1024, 8192)?, 8192);
        assert_eq!(super::grown_output_capacity(1024, 1024)?, 2048);
        assert!(super::grown_output_capacity(64 * 1024 * 1024, 1).is_err());
        Ok(())
    }
}
