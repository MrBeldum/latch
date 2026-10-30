use super::capture::Gpu;
use super::mf;
use crate::video::{Codec, VideoConfig};
use anyhow::{bail, Context, Result};
use std::{
    mem::ManuallyDrop,
    sync::mpsc,
    time::{Duration, Instant},
};
use windows::{
    core::{implement, Interface, GUID, VARIANT},
    Win32::{Graphics::Direct3D11::ID3D11Texture2D, Media::MediaFoundation::*, System::Com::*},
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
fn activations(flags: MFT_ENUM_FLAG) -> Result<Vec<(IMFActivate, String)>> {
    let (mut array, mut count) = (std::ptr::null_mut(), 0);
    let filter = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
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
        .context("enumerate H.264 encoders")?;
        let mut out = Vec::new();
        for i in 0..count as usize {
            if let Some(activate) = array.add(i).read() {
                let name = (|| -> windows::core::Result<String> {
                    let length = activate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute)?;
                    let mut text = vec![0; length as usize + 1];
                    activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut text, None)?;
                    Ok(String::from_utf16_lossy(&text[..length as usize]))
                })()
                .unwrap_or_else(|_| "hardware H.264 encoder".into());
                out.push((activate, name));
            }
        }
        CoTaskMemFree(Some(array.cast()));
        Ok(out)
    }
}
fn hardware() -> Result<Vec<(IMFActivate, String)>> {
    activations(MFT_ENUM_FLAG_HARDWARE)
}
pub(super) fn available() -> bool {
    if hardware().is_ok_and(|v| !v.is_empty()) {
        return true;
    }
    // SAFETY: This probe runs in an initialized MTA with MF started.
    unsafe {
        CoCreateInstance::<_, IMFTransform>(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
            .is_ok()
    }
}
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
    sequence: Vec<u8>,
    activation: Option<IMFActivate>,
    output_sample: Option<IMFSample>,
    output_capacity: u32,
}
impl Encoder {
    pub fn new(config: &VideoConfig, gpu: &Gpu) -> Result<Self> {
        for (activation, name) in hardware().unwrap_or_default() {
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
                    tracing::debug!("hardware H.264 MFT rejected configuration: {e:#}");
                    // SAFETY: Releases resources held by this failed activation.
                    unsafe {
                        let _ = activation.ShutdownObject();
                    }
                }
            }
        }
        Self::software(config)
    }
    pub fn software(config: &VideoConfig) -> Result<Self> {
        // SAFETY: Called only while the worker owns a COM/MF runtime guard.
        let transform =
            unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
                .context("create Microsoft H.264 encoder")?;
        let name = activations(MFT_ENUM_FLAG_SYNCMFT)
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
    fn configure(
        transform: IMFTransform,
        config: &VideoConfig,
        gpu: Option<&Gpu>,
        name: String,
        hardware: bool,
    ) -> Result<Self> {
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
            if config.codec == Codec::H264Baseline {
                let _ = api.SetValue(&CODECAPI_AVEncH264CABACEnable, &VARIANT::from(false));
            }
            let output = media_type(&MFVideoFormat_H264, config)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_kbps.saturating_mul(1000))?;
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
                name: format!("Media Foundation H.264 ({name})"),
                duration: 10_000_000 / i64::from(config.fps),
                time: 0,
                sequence: Vec::new(),
                activation: None,
                output_sample: None,
                output_capacity: 0,
            })
        }
    }
    pub fn bitrate(&self, kbps: u32) -> Result<()> {
        // SAFETY: The API belongs to this worker's transform.
        unsafe {
            self.api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(kbps.saturating_mul(1000)),
            )
        }
        .context("update encoder bitrate")
    }
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
        // finishes. Each GPU frame has its own texture, so it cannot be overwritten.
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
                let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
                buffer.Unlock()?;
                let output_type = self.transform.GetOutputCurrentType(0)?;
                if let Ok(size) = output_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                    let mut header = vec![0; size as usize];
                    if output_type
                        .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)
                        .is_ok()
                    {
                        self.sequence = normalize(&header)?;
                    }
                }
                let mut bytes = normalize(&bytes)?;
                for kind in [7, 8] {
                    if let Some(nal) = nals(&bytes)
                        .into_iter()
                        .find(|nal| nal.first().is_some_and(|v| v & 31 == kind))
                    {
                        let mut headers = Vec::new();
                        for old in nals(&self.sequence) {
                            if old.first().is_some_and(|v| v & 31 != kind) {
                                headers.extend_from_slice(&[0, 0, 0, 1]);
                                headers.extend_from_slice(old);
                            }
                        }
                        headers.extend_from_slice(&[0, 0, 0, 1]);
                        headers.extend_from_slice(nal);
                        self.sequence = headers;
                    }
                }
                let idr = nal_types(&bytes).contains(&5);
                if idr {
                    let mut access_unit = Vec::new();
                    for kind in [7, 8] {
                        let header = nals(&self.sequence)
                            .into_iter()
                            .find(|nal| nal.first().is_some_and(|v| v & 31 == kind))
                            .context("encoder IDR has no parameter sets or sequence header")?;
                        access_unit.extend_from_slice(&[0, 0, 0, 1]);
                        access_unit.extend_from_slice(header);
                    }
                    for nal in nals(&bytes) {
                        if nal.first().is_some_and(|v| !matches!(v & 31, 7 | 8)) {
                            access_unit.extend_from_slice(&[0, 0, 0, 1]);
                            access_unit.extend_from_slice(nal);
                        }
                    }
                    bytes = access_unit;
                }
                return Ok(Some((bytes, idr)));
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
fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            starts.push((i, i + 4));
            i += 4;
        } else if data[i..].starts_with(&[0, 0, 1]) {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &(_, start))| &data[start..starts.get(i + 1).map_or(data.len(), |v| v.0)])
        .collect()
}
fn nal_types(data: &[u8]) -> Vec<u8> {
    nals(data)
        .iter()
        .filter_map(|v| v.first().map(|v| v & 31))
        .collect()
}
fn normalize(data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() || data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Ok(data.to_vec());
    }
    let mut out = Vec::new();
    if data[0] == 1 && data.len() >= 7 {
        // AVCDecoderConfigurationRecord, used by some hardware sequence headers.
        let mut at = 6;
        let sps_count = data[5] & 31;
        for count in 0..2 {
            let n = if count == 0 {
                sps_count
            } else {
                let n = *data.get(at).context("truncated avcC PPS count")?;
                at += 1;
                n
            };
            for _ in 0..n {
                let bytes = data.get(at..at + 2).context("truncated avcC length")?;
                let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                at += 2;
                let nal = data.get(at..at + length).context("truncated avcC NAL")?;
                at += length;
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(nal);
            }
        }
    } else {
        let mut at = 0;
        while at < data.len() {
            let bytes = data.get(at..at + 4).context("truncated H.264 NAL length")?;
            let length = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
            at += 4;
            if length == 0 {
                bail!("empty length-prefixed H.264 NAL");
            }
            let nal = data.get(at..at + length).context("truncated H.264 NAL")?;
            at += length;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn annex_b_parameter_sets_and_length_prefixes() -> Result<()> {
        let header = [1, 66, 0, 31, 255, 225, 0, 2, 0x67, 42, 1, 0, 2, 0x68, 7];
        assert_eq!(nal_types(&normalize(&header)?), [7, 8]);
        assert_eq!(normalize(&[0, 0, 0, 2, 0x65, 3])?, [0, 0, 0, 1, 0x65, 3]);
        assert!(normalize(&[0, 0, 0, 9, 0x65]).is_err());
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
