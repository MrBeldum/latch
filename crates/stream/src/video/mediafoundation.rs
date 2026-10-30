//! Standalone H.264 decoding. COM and MF stay on a dedicated MTA so callers
//! can create and move the decoder from a UI thread with any apartment.
use anyhow::{bail, ensure, Context, Result};
use std::{cell::RefCell, collections::VecDeque, mem::ManuallyDrop, sync::mpsc, thread};
use windows::{
    core::Interface,
    Win32::{
        Foundation::HMODULE,
        Graphics::{Direct3D::*, Direct3D11::*, Dxgi::Common::*},
        Media::MediaFoundation::*,
        System::Com::*,
    },
};

pub struct DecodedNv12 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: usize,
    pub uv: Vec<u8>,
    pub uv_stride: usize,
}
struct Request {
    bytes: Vec<u8>,
    reply: mpsc::SyncSender<Result<(Option<DecodedNv12>, &'static str)>>,
}
pub struct MfDecoder {
    requests: mpsc::Sender<Request>,
    name: &'static str,
}
impl MfDecoder {
    pub fn new() -> Result<Self> {
        let (requests, receive) = mpsc::channel::<Request>();
        let (ready, startup) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("latch-mf-decode".into())
            .spawn(move || {
                let initialized = (|| {
                    let runtime = Runtime::new()?;
                    let decoder = State::new(true).or_else(|_| State::new(false))?;
                    Ok::<_, anyhow::Error>((runtime, decoder))
                })();
                let (_runtime, mut decoder) = match initialized {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let name = if decoder.gpu.is_some() {
                    "Media Foundation (DXVA)"
                } else {
                    "Media Foundation (software)"
                };
                if ready.send(Ok(name)).is_err() {
                    return;
                }
                for request in receive {
                    let result = decoder
                        .decode(&request.bytes)
                        .map(|frame| (frame, decoder.name()))
                        .context("decode Media Foundation H.264 access unit");
                    let _ = request.reply.send(result);
                }
            })
            .context("spawn Media Foundation decoder worker")?;
        Ok(Self {
            requests,
            name: startup
                .recv()
                .context("decoder worker stopped during startup")??,
        })
    }
    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<DecodedNv12>> {
        ensure!(
            annexb.len() <= 64 * 1024 * 1024,
            "H.264 access unit exceeds 64 MiB"
        );
        let (reply, receive) = mpsc::sync_channel(1);
        self.requests
            .send(Request {
                bytes: annexb.to_vec(),
                reply,
            })
            .context("decoder worker stopped")?;
        let (frame, name) = receive.recv().context("decoder worker stopped")??;
        self.name = name;
        Ok(frame)
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
}
struct Runtime {
    mf: bool,
}
impl Runtime {
    fn new() -> Result<Self> {
        // SAFETY: This guard never leaves its worker and balances both APIs.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.context("decoder COM MTA")?;
        let mut runtime = Self { mf: false };
        // SAFETY: COM is initialized; failure drops the guard and uninitializes it.
        unsafe { mf::MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .context("start decoder Media Foundation")?;
        runtime.mf = true;
        Ok(runtime)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: Decoder interfaces are destroyed before the runtime, on this thread.
        unsafe {
            if self.mf {
                let _ = mf::MFShutdown();
            }
            CoUninitialize();
        }
    }
}
struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    staging: RefCell<Option<ID3D11Texture2D>>,
}
impl Gpu {
    fn new(transform: &IMFTransform) -> Result<Self> {
        let (mut device, mut context, mut manager, mut token) = (None, None, None, 0);
        // SAFETY: All interfaces are owned by the decoder worker. The device's
        // immediate context is protected because the MFT may use driver threads.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("create DXVA D3D11 device")?;
            let device = device.context("missing decoder D3D11 device")?;
            let context: ID3D11DeviceContext = context.context("missing decoder D3D11 context")?;
            let _ = context
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
            ensure!(
                transform
                    .GetAttributes()?
                    .GetUINT32(&MF_SA_D3D11_AWARE)
                    .unwrap_or(0)
                    != 0,
                "decoder is not D3D11 aware"
            );
            mf::MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("missing decoder DXGI manager")?;
            manager.ResetDevice(&device, token)?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("configure decoder DXVA manager")?;
            Ok(Self {
                device,
                context,
                _manager: manager,
                staging: RefCell::new(None),
            })
        }
    }
    fn read(&self, buffer: &IMFDXGIBuffer, layout: Layout) -> Result<DecodedNv12> {
        let mut resource = std::ptr::null_mut();
        // SAFETY: GetResource writes an owned texture interface reference. It is
        // adopted immediately, and its subresource is copied before releasing it.
        unsafe {
            buffer
                .GetResource(&ID3D11Texture2D::IID, &mut resource)
                .context("get DXVA output texture")?;
            ensure!(!resource.is_null(), "DXVA returned no texture");
            let texture = ID3D11Texture2D::from_raw(resource);
            let subresource = buffer.GetSubresourceIndex()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            ensure!(
                desc.Format == DXGI_FORMAT_NV12
                    && layout.coded_width <= desc.Width
                    && layout.coded_height <= desc.Height,
                "invalid DXVA NV12 texture dimensions"
            );
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut cached = self.staging.borrow_mut();
            let matches = cached.as_ref().is_some_and(|texture| {
                let mut old = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut old);
                (old.Width, old.Height, old.Format) == (desc.Width, desc.Height, desc.Format)
            });
            if !matches {
                let mut staging = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut staging))
                    .context("allocate DXVA staging texture")?;
                *cached = staging;
            }
            let staging = cached.as_ref().context("missing decoder staging texture")?;
            self.context
                .CopySubresourceRegion(staging, 0, 0, 0, 0, &texture, subresource, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("map DXVA output")?;
            let bytes = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                mapped.RowPitch as usize * desc.Height as usize * 3 / 2,
            );
            let result = layout.copy(bytes, mapped.RowPitch as usize, desc.Height as usize);
            self.context.Unmap(staging, 0);
            result
        }
    }
}
#[derive(Clone, Copy)]
struct Layout {
    coded_width: u32,
    coded_height: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    stride: usize,
}
impl Layout {
    fn from_type(ty: &IMFMediaType) -> Result<Self> {
        // SAFETY: Attribute reads are owned values; the aperture blob is read
        // unaligned into its Win32 value type only after checking its byte size.
        unsafe {
            let size = ty
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .context("decoder frame size")?;
            let (coded_width, coded_height) = ((size >> 32) as u32, size as u32);
            ensure!(
                coded_width > 0
                    && coded_height > 0
                    && coded_width <= 16384
                    && coded_height <= 16384,
                "invalid decoded dimensions"
            );
            let mut layout = Self {
                coded_width,
                coded_height,
                left: 0,
                top: 0,
                width: coded_width,
                height: coded_height,
                stride: ty
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .unwrap_or(coded_width.div_ceil(2) * 2) as usize,
            };
            for key in [MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_GEOMETRIC_APERTURE] {
                if let Ok(size) = ty.GetBlobSize(&key) {
                    ensure!(
                        size as usize == std::mem::size_of::<MFVideoArea>(),
                        "invalid decoder aperture size"
                    );
                    let mut blob = vec![0; size as usize];
                    ty.GetBlob(&key, &mut blob, None)?;
                    let area = blob.as_ptr().cast::<MFVideoArea>().read_unaligned();
                    ensure!(
                        area.OffsetX.value >= 0
                            && area.OffsetX.fract == 0
                            && area.OffsetY.value >= 0
                            && area.OffsetY.fract == 0
                            && area.Area.cx > 0
                            && area.Area.cy > 0,
                        "invalid decoder aperture"
                    );
                    layout.left = area.OffsetX.value as u32;
                    layout.top = area.OffsetY.value as u32;
                    layout.width = area.Area.cx as u32;
                    layout.height = area.Area.cy as u32;
                    break;
                }
            }
            ensure!(
                layout.left + layout.width <= coded_width
                    && layout.top + layout.height <= coded_height
                    && layout.left.is_multiple_of(2)
                    && layout.top.is_multiple_of(2),
                "decoder crop exceeds NV12 bounds"
            );
            Ok(layout)
        }
    }
    fn copy(self, bytes: &[u8], stride: usize, surface_height: usize) -> Result<DecodedNv12> {
        let (left, top, width, height) = (
            self.left as usize,
            self.top as usize,
            self.width as usize,
            self.height as usize,
        );
        let uv_width = width.div_ceil(2) * 2;
        ensure!(
            stride >= left + uv_width && surface_height >= top + height,
            "invalid NV12 pitch or height"
        );
        let mut y = Vec::with_capacity(width * height);
        let mut uv = Vec::with_capacity(uv_width * height.div_ceil(2));
        for row in top..top + height {
            let start = row
                .checked_mul(stride)
                .and_then(|n| n.checked_add(left))
                .context("NV12 Y offset overflow")?;
            y.extend_from_slice(
                bytes
                    .get(start..start + width)
                    .context("truncated NV12 Y plane")?,
            );
        }
        for row in top / 2..top / 2 + height.div_ceil(2) {
            let start = (surface_height + row)
                .checked_mul(stride)
                .and_then(|n| n.checked_add(left))
                .context("NV12 UV offset overflow")?;
            uv.extend_from_slice(
                bytes
                    .get(start..start + uv_width)
                    .context("truncated NV12 UV plane")?,
            );
        }
        Ok(DecodedNv12 {
            width: self.width,
            height: self.height,
            y,
            y_stride: width,
            uv,
            uv_stride: uv_width,
        })
    }
}
struct State {
    transform: IMFTransform,
    gpu: Option<Gpu>,
    output_type: Option<IMFMediaType>,
    queue: VecDeque<DecodedNv12>,
    time: i64,
    dxva_used: bool,
}
impl State {
    fn name(&self) -> &'static str {
        if self.dxva_used {
            "Media Foundation (DXVA)"
        } else {
            "Media Foundation (software)"
        }
    }
    fn new(dxva: bool) -> Result<Self> {
        // SAFETY: This runs only in the initialized decoder MTA.
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .context("create Microsoft H.264 decoder")?;
            // The H.264 decoder exposes this as a UINT32 transform attribute,
            // not the VT_BOOL encoder property on ICodecAPI. Set it before types.
            transform
                .GetAttributes()
                .context("decoder attributes")?
                .SetUINT32(&CODECAPI_AVLowLatencyMode, 1)
                .context("enable decoder low-latency mode")?;
            let gpu = if dxva {
                Some(Gpu::new(&transform)?)
            } else {
                None
            };
            let input = mf::MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform
                .SetInputType(0, &input, 0)
                .context("decoder H.264 input type")?;
            let mut state = Self {
                transform,
                gpu,
                output_type: None,
                queue: VecDeque::new(),
                time: 0,
                dxva_used: dxva,
            };
            state.output_type()?;
            state
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("start decoder streaming")?;
            state
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("start decoder input stream")?;
            Ok(state)
        }
    }
    fn output_type(&mut self) -> Result<()> {
        // SAFETY: Type negotiation is serialized with ProcessInput/Output.
        unsafe {
            for index in 0..256 {
                let ty = match self.transform.GetOutputAvailableType(0, index) {
                    Ok(ty) => ty,
                    Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                    Err(e) => return Err(e).context("enumerate decoder output types"),
                };
                if ty.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    match self.transform.SetOutputType(0, &ty, 0) {
                        Err(e) if e.code() == MF_E_UNSUPPORTED_D3D_TYPE && self.gpu.is_some() => {
                            self.transform
                                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0)
                                .context("disable unsupported decoder DXVA mode")?;
                            self.gpu = None;
                            self.dxva_used = false;
                            self.transform
                                .SetOutputType(0, &ty, 0)
                                .context("set software decoder NV12 type")?;
                        }
                        result => result.context("set decoder NV12 output type")?,
                    }
                    self.output_type = Some(ty);
                    return Ok(());
                }
            }
        }
        bail!("decoder offers no NV12 output")
    }
    fn decode(&mut self, annexb: &[u8]) -> Result<Option<DecodedNv12>> {
        if self.output_type.is_some() {
            self.drain()?;
        }
        if !annexb.is_empty() {
            let length = u32::try_from(annexb.len())?;
            // SAFETY: The input is copied into a locked buffer before being given
            // to the transform, which retains its own reference if necessary.
            unsafe {
                let sample = mf::MFCreateSample()?;
                let buffer = mf::MFCreateMemoryBuffer(length)?;
                let mut data = std::ptr::null_mut();
                buffer.Lock(&mut data, None, None)?;
                std::ptr::copy_nonoverlapping(annexb.as_ptr(), data, annexb.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(length)?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(self.time)?;
                sample.SetSampleDuration(166667)?;
                self.time += 166667;
                match self.transform.ProcessInput(0, &sample, 0) {
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        self.drain()?;
                        self.transform
                            .ProcessInput(0, &sample, 0)
                            .context("decoder retry ProcessInput")?;
                    }
                    result => result.context("decoder ProcessInput")?,
                }
            }
        }
        self.drain()?;
        Ok(self.queue.pop_front())
    }
    fn drain(&mut self) -> Result<()> {
        if self.output_type.is_none() {
            match self.output_type() {
                Err(e)
                    if e.downcast_ref::<windows::core::Error>()
                        .is_some_and(|e| e.code() == MF_E_TRANSFORM_TYPE_NOT_SET) =>
                {
                    return Ok(())
                }
                other => other?,
            }
        }
        // SAFETY: ManuallyDrop output fields are released on every HRESULT. All
        // output buffers are copied before the transform can recycle its sample.
        unsafe {
            for _ in 0..64 {
                let info = self.transform.GetOutputStreamInfo(0)?;
                let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                    let sample = mf::MFCreateSample()?;
                    sample.AddBuffer(&mf::MFCreateAlignedMemoryBuffer(
                        info.cbSize.max(1),
                        info.cbAlignment.saturating_sub(1),
                    )?)?;
                    Some(sample)
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
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.output_type()?;
                        continue;
                    }
                    result => result.context("decoder ProcessOutput")?,
                }
                let sample = sample.context("decoder returned no output sample")?;
                let layout = Layout::from_type(
                    self.output_type
                        .as_ref()
                        .context("decoder has no output type")?,
                )?;
                let buffer = sample.GetBufferByIndex(0)?;
                let picture = if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
                    self.dxva_used = true;
                    self.gpu
                        .as_ref()
                        .context("DXVA sample without a D3D11 device")?
                        .read(&dxgi, layout)?
                } else if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer2>() {
                    self.dxva_used = false;
                    let (mut scan, mut start, mut pitch, mut length) =
                        (std::ptr::null_mut(), std::ptr::null_mut(), 0, 0);
                    buffer2d.Lock2DSize(
                        MF2DBuffer_LockFlags_Read,
                        &mut scan,
                        &mut pitch,
                        &mut start,
                        &mut length,
                    )?;
                    let result = (|| {
                        ensure!(
                            pitch > 0 && scan as usize >= start as usize,
                            "invalid NV12 2D buffer"
                        );
                        let offset = scan as usize - start as usize;
                        let all = std::slice::from_raw_parts(start, length as usize);
                        layout.copy(
                            all.get(offset..).context("invalid NV12 scanline")?,
                            pitch as usize,
                            layout.coded_height as usize,
                        )
                    })();
                    buffer2d.Unlock2D()?;
                    result?
                } else {
                    self.dxva_used = false;
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let (mut data, mut length) = (std::ptr::null_mut(), 0);
                    buffer.Lock(&mut data, None, Some(&mut length))?;
                    let result = layout.copy(
                        std::slice::from_raw_parts(data, length as usize),
                        layout.stride,
                        layout.coded_height as usize,
                    );
                    buffer.Unlock()?;
                    result?
                };
                ensure!(
                    self.queue.len() < 64,
                    "decoder output queue exceeded 64 pictures"
                );
                self.queue.push_back(picture);
            }
        }
        bail!("decoder produced too many outputs without needing input")
    }
}
impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: The MFT is stopped before its manager/device and MF runtime drop.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if self.gpu.is_some() {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crop_preserves_visible_planes_and_ignores_padding() -> Result<()> {
        let layout = Layout {
            coded_width: 8,
            coded_height: 6,
            left: 2,
            top: 2,
            width: 4,
            height: 2,
            stride: 12,
        };
        let bytes: Vec<u8> = (0..108).collect();
        let frame = layout.copy(&bytes, 12, 6)?;
        assert_eq!(frame.y, [26, 27, 28, 29, 38, 39, 40, 41]);
        assert_eq!(frame.uv, [86, 87, 88, 89]);
        assert_eq!(
            (frame.width, frame.height, frame.y_stride, frame.uv_stride),
            (4, 2, 4, 4)
        );
        assert!(layout.copy(&bytes[..80], 12, 6).is_err());
        Ok(())
    }
}

mod mf {
    //! Resolve MF lazily so Windows Server without the Media Foundation feature
    //! can still start Latch and report an unavailable codec instead of failing in
    //! the process loader. The system DLL is retained for the process lifetime.
    #![allow(non_snake_case)]
    use std::{ffi::c_void, sync::OnceLock};
    use windows::{
        core::{s, w, Error, Interface, HRESULT, PCSTR},
        Win32::{
            Foundation::{E_POINTER, HANDLE, HMODULE},
            Media::MediaFoundation::*,
            System::LibraryLoader::*,
        },
    };
    fn resolve(name: PCSTR) -> windows::core::Result<unsafe extern "system" fn() -> isize> {
        static MODULE: OnceLock<Result<usize, HRESULT>> = OnceLock::new();
        let module = MODULE
            .get_or_init(|| {
                // SAFETY: Load only the system copy; retain its reference while cached
                // function pointers and any MF COM objects can still exist.
                unsafe {
                    LoadLibraryExW(
                        w!("mfplat.dll"),
                        HANDLE::default(),
                        LOAD_LIBRARY_SEARCH_SYSTEM32,
                    )
                }
                .map(|m| m.0 as usize)
                .map_err(|e| e.code())
            })
            .as_ref()
            .map_err(|code| Error::from_hresult(*code))?;
        // SAFETY: The cached module retains its loader reference permanently.
        unsafe { GetProcAddress(HMODULE(*module as *mut c_void), name) }
            .ok_or_else(Error::from_win32)
    }
    macro_rules! bind {
        ($name:literal, $signature:ty) => {{
            static FUNCTION: OnceLock<Result<$signature, HRESULT>> = OnceLock::new();
            *FUNCTION
                .get_or_init(|| {
                    let pointer = resolve(s!($name)).map_err(|e| e.code())?;
                    // SAFETY: Each signature below is the exact Windows SDK ABI for
                    // the named export; resolve retains the module that owns it.
                    Ok(unsafe {
                        std::mem::transmute::<unsafe extern "system" fn() -> isize, $signature>(
                            pointer,
                        )
                    })
                })
                .as_ref()
                .map_err(|code| Error::from_hresult(*code))?
        }};
    }
    unsafe fn created<T: Interface>(
        call: impl FnOnce(*mut *mut c_void) -> HRESULT,
    ) -> windows::core::Result<T> {
        let mut pointer = std::ptr::null_mut();
        call(&mut pointer).ok()?;
        if pointer.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        // SAFETY: Successful COM creation transfers one owned reference.
        Ok(unsafe { T::from_raw(pointer) })
    }
    pub unsafe fn MFStartup(version: u32, flags: u32) -> windows::core::Result<()> {
        // SAFETY: Exact SDK signature; caller balances startup with shutdown.
        let function = bind!("MFStartup", unsafe extern "system" fn(u32, u32) -> HRESULT);
        // SAFETY: Caller owns the matching startup/shutdown scope.
        unsafe { function(version, flags).ok() }
    }
    pub unsafe fn MFShutdown() -> windows::core::Result<()> {
        // SAFETY: Caller owns a successful MF startup reference.
        let function = bind!("MFShutdown", unsafe extern "system" fn() -> HRESULT);
        // SAFETY: Caller owns a successful startup reference.
        unsafe { function().ok() }
    }
    pub unsafe fn MFCreateMediaType() -> windows::core::Result<IMFMediaType> {
        let function = bind!(
            "MFCreateMediaType",
            unsafe extern "system" fn(*mut *mut c_void) -> HRESULT
        );
        // SAFETY: Exact SDK creation signature, adopted as an owned interface.
        unsafe { created(|out| function(out)) }
    }
    pub unsafe fn MFCreateSample() -> windows::core::Result<IMFSample> {
        let function = bind!(
            "MFCreateSample",
            unsafe extern "system" fn(*mut *mut c_void) -> HRESULT
        );
        // SAFETY: Exact SDK creation signature, adopted as an owned interface.
        unsafe { created(|out| function(out)) }
    }
    pub unsafe fn MFCreateMemoryBuffer(length: u32) -> windows::core::Result<IMFMediaBuffer> {
        let function = bind!(
            "MFCreateMemoryBuffer",
            unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT
        );
        // SAFETY: Exact SDK creation signature, adopted as an owned interface.
        unsafe { created(|out| function(length, out)) }
    }
    pub unsafe fn MFCreateAlignedMemoryBuffer(
        length: u32,
        alignment: u32,
    ) -> windows::core::Result<IMFMediaBuffer> {
        let function = bind!(
            "MFCreateAlignedMemoryBuffer",
            unsafe extern "system" fn(u32, u32, *mut *mut c_void) -> HRESULT
        );
        // SAFETY: Exact SDK creation signature, adopted as an owned interface.
        unsafe { created(|out| function(length, alignment, out)) }
    }
    pub unsafe fn MFCreateDXGIDeviceManager(
        token: *mut u32,
        manager: *mut Option<IMFDXGIDeviceManager>,
    ) -> windows::core::Result<()> {
        // SAFETY: Caller supplies writable SDK output slots.
        let function = bind!(
            "MFCreateDXGIDeviceManager",
            unsafe extern "system" fn(*mut u32, *mut Option<IMFDXGIDeviceManager>) -> HRESULT
        );
        // SAFETY: Caller supplies the SDK output pointers and optional filters.
        unsafe { function(token, manager).ok() }
    }
}
