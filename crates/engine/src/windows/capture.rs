//! The cursor uses a pixel shader compiled by the system d3dcompiler_47.dll.
//! This preserves XOR cursors without reading the desktop back to the CPU.
use super::{display, gdi::Gdi};
use crate::{
    input::DisplayRect,
    software::{Bgra, CpuCapture},
};
use anyhow::{bail, Context, Result};
use std::{
    mem::ManuallyDrop,
    time::{Duration, Instant},
};
use windows::{
    core::{s, Interface},
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::{Fxc::*, *},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
};

#[derive(Clone)]
pub(super) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}
impl Gpu {
    pub fn new(adapter: &IDXGIAdapter1) -> Result<Self> {
        let (mut device, mut context) = (None, None);
        // SAFETY: Output pointers are valid; an explicit adapter requires UNKNOWN.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .context("create capture D3D11 device")?;
        let gpu = Self {
            device: device.context("D3D11 returned no device")?,
            context: context.context("D3D11 returned no context")?,
        };
        // SAFETY: MF may call the immediate context from driver threads.
        unsafe {
            let _ = gpu
                .context
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
        }
        Ok(gpu)
    }
    pub fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        bind: u32,
        staging: bool,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: if staging {
                D3D11_USAGE_STAGING
            } else {
                D3D11_USAGE_DEFAULT
            },
            BindFlags: if staging { 0 } else { bind },
            CPUAccessFlags: if staging {
                D3D11_CPU_ACCESS_READ.0 as u32
            } else {
                0
            },
            ..Default::default()
        };
        let mut texture = None;
        // SAFETY: Descriptor and output pointer are valid for this call.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("allocate D3D11 texture")?;
        texture.context("D3D11 returned no texture")
    }
    pub fn read(&self, texture: &ID3D11Texture2D, rows: usize) -> Result<(Vec<u8>, usize)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: The texture and output descriptor are valid.
        unsafe {
            texture.GetDesc(&mut desc);
        }
        let staging = self.texture(desc.Width, desc.Height, desc.Format, 0, true)?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: CopyResource is ordered before Map. Map is paired with Unmap;
        // the row pitch describes all accessible bytes copied below.
        unsafe {
            self.context.CopyResource(&staging, texture);
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("map staging texture")?;
            let bytes = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                mapped.RowPitch as usize * rows,
            )
            .to_vec();
            self.context.Unmap(&staging, 0);
            Ok((bytes, mapped.RowPitch as usize))
        }
    }
}
struct CursorRenderer {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    shape: Option<ID3D11ShaderResourceView>,
}
const SHADER: &str = r#"
Texture2D<float4> desktop : register(t0);
Texture2D<uint> shape : register(t1);
cbuffer C : register(b0) { int2 origin; uint2 size; uint kind; uint rotation; uint2 rawsize; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    return float4(id == 2 ? 3 : -1, id == 1 ? 3 : -1, 0, 1);
}
float4 ps(float4 p : SV_Position) : SV_Target {
    int2 pos = int2(p.xy); int2 src = pos;
    if(rotation == 2) src = int2(pos.y, rawsize.y - 1 - pos.x);
    if(rotation == 3) src = int2(rawsize.x - 1 - pos.x, rawsize.y - 1 - pos.y);
    if(rotation == 4) src = int2(rawsize.x - 1 - pos.y, pos.x);
    float4 bg = desktop.Load(int3(src,0));
    int2 c = pos - origin;
    if(kind == 0 || any(c < 0) || any(c >= int2(size))) return bg;
    uint v = shape.Load(int3(c,0));
    uint3 rgb = uint3((v >> 16) & 255, (v >> 8) & 255, v & 255);
    uint a = v >> 24;
    if(kind == 2) return float4(float3(rgb)*(a/65025.0) + bg.rgb*(1-a/255.0), 1);
    uint3 screen = uint3(round(saturate(bg.rgb)*255));
    if(kind == 4) rgb = a == 255 ? screen ^ rgb : rgb;
    if(kind == 1) rgb = (screen & a) ^ rgb;
    return float4(float3(rgb)/255.0, 1);
}
"#;
fn compile_shader(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Result<ID3DBlob> {
    let (mut code, mut errors) = (None, None);
    // SAFETY: Shader source and entry names live through compilation;
    // compilation uses only the Windows system compiler, no native build.
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let detail = errors
            .map(|e| {
                // SAFETY: The compiler blob owns BufferSize readable bytes.
                String::from_utf8_lossy(unsafe {
                    std::slice::from_raw_parts(e.GetBufferPointer().cast::<u8>(), e.GetBufferSize())
                })
                .into_owned()
            })
            .unwrap_or_default();
        return Err(e).with_context(|| format!("compile cursor shader: {detail}"));
    }
    code.context("compiler returned no shader")
}

impl CursorRenderer {
    fn new(gpu: &Gpu) -> Result<Self> {
        let vs = compile_shader(s!("vs"), s!("vs_4_0"))?;
        let ps = compile_shader(s!("ps"), s!("ps_4_0"))?;
        let (mut vertex, mut pixel, mut constants) = (None, None, None);
        // SAFETY: Blobs own valid shader bytecode; all output slots are valid.
        unsafe {
            gpu.device
                .CreateVertexShader(
                    std::slice::from_raw_parts(vs.GetBufferPointer().cast(), vs.GetBufferSize()),
                    None,
                    Some(&mut vertex),
                )
                .context("create cursor vertex shader")?;
            gpu.device
                .CreatePixelShader(
                    std::slice::from_raw_parts(ps.GetBufferPointer().cast(), ps.GetBufferSize()),
                    None,
                    Some(&mut pixel),
                )
                .context("create cursor pixel shader")?;
            gpu.device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: 32,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut constants),
                )
                .context("create cursor constants")?;
        }
        Ok(Self {
            vertex: vertex.context("missing vertex shader")?,
            pixel: pixel.context("missing pixel shader")?,
            constants: constants.context("missing cursor buffer")?,
            shape: None,
        })
    }
    fn shape(
        &mut self,
        gpu: &Gpu,
        info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        data: &[u8],
    ) -> Result<()> {
        let mono = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
        let height = if mono { info.Height / 2 } else { info.Height };
        if info.Width == 0 || height == 0 || info.Pitch as usize * info.Height as usize > data.len()
        {
            bail!("invalid DXGI pointer shape");
        }
        let mut pixels = Vec::with_capacity(info.Width as usize * height as usize);
        for y in 0..height as usize {
            for x in 0..info.Width as usize {
                let at = y * info.Pitch as usize;
                let pixel = if mono {
                    let and = (data[at + x / 8] >> (7 - x % 8)) & 1;
                    let xor = (data[at + height as usize * info.Pitch as usize + x / 8]
                        >> (7 - x % 8))
                        & 1;
                    (if and != 0 { 0xff000000 } else { 0 })
                        | (if xor != 0 { 0x00ffffff } else { 0 })
                } else {
                    let at = at + x * 4;
                    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
                };
                pixels.push(pixel);
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: info.Width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R32_UINT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: info.Width * 4,
            ..Default::default()
        };
        let mut texture = None;
        let mut view = None;
        // SAFETY: Initial data covers every row. CreateTexture2D copies it.
        unsafe {
            gpu.device
                .CreateTexture2D(&desc, Some(&initial), Some(&mut texture))
                .context("upload DXGI cursor")?;
            gpu.device
                .CreateShaderResourceView(
                    &texture.context("missing cursor texture")?,
                    None,
                    Some(&mut view),
                )
                .context("create cursor view")?;
        }
        self.shape = view;
        Ok(())
    }
    fn draw(
        &self,
        gpu: &Gpu,
        source: &ID3D11Texture2D,
        target: &ID3D11Texture2D,
        cursor: &DXGI_OUTDUPL_POINTER_POSITION,
        shape: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        rotation: DXGI_MODE_ROTATION,
    ) -> Result<()> {
        let (mut srv, mut rtv) = (None, None);
        let (mut raw, mut dst) = (
            D3D11_TEXTURE2D_DESC::default(),
            D3D11_TEXTURE2D_DESC::default(),
        );
        // SAFETY: Views reference owned textures and constants contain exactly 32 bytes.
        unsafe {
            source.GetDesc(&mut raw);
            target.GetDesc(&mut dst);
            gpu.device
                .CreateShaderResourceView(source, None, Some(&mut srv))
                .context("desktop shader view")?;
            gpu.device
                .CreateRenderTargetView(target, None, Some(&mut rtv))
                .context("cursor render target")?;
            let constants: [u32; 8] = [
                cursor.Position.x as u32,
                cursor.Position.y as u32,
                shape.Width,
                if shape.Type == 1 {
                    shape.Height / 2
                } else {
                    shape.Height
                },
                if cursor.Visible.as_bool() && self.shape.is_some() {
                    shape.Type
                } else {
                    0
                },
                rotation.0 as u32,
                raw.Width,
                raw.Height,
            ];
            gpu.context.UpdateSubresource(
                &self.constants,
                0,
                None,
                constants.as_ptr().cast(),
                0,
                0,
            );
            gpu.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            gpu.context.VSSetShader(&self.vertex, None);
            gpu.context.PSSetShader(&self.pixel, None);
            gpu.context
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            gpu.context
                .PSSetShaderResources(0, Some(&[srv, self.shape.clone()]));
            gpu.context.OMSetRenderTargets(Some(&[rtv]), None);
            gpu.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: dst.Width as f32,
                Height: dst.Height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            gpu.context.Draw(3, 0);
            gpu.context.PSSetShaderResources(0, Some(&[None, None]));
            gpu.context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}
pub(super) struct Capture {
    pub gpu: Gpu,
    duplication: Option<IDXGIOutputDuplication>,
    source: ID3D11Texture2D,
    pub frame: ID3D11Texture2D,
    renderer: CursorRenderer,
    pointer: DXGI_OUTDUPL_POINTER_POSITION,
    shape: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    rotation: DXGI_MODE_ROTATION,
    pub display: DisplayRect,
    pub has_frame: bool,
    cpu_seeded: bool,
    retry: ReopenBackoff,
    waiting_since: Option<Instant>,
    output_name: [u16; 32],
    gdi: Option<Gdi>,
}
struct ReopenBackoff {
    at: Instant,
    delay: Duration,
}
impl ReopenBackoff {
    fn new() -> Self {
        Self {
            at: Instant::now(),
            delay: Duration::from_millis(50),
        }
    }
    fn defer(&mut self, now: Instant) {
        self.at = now + self.delay;
        self.delay = (self.delay * 2).min(Duration::from_secs(1));
    }
}
fn oriented_size(width: u32, height: u32, rotation: DXGI_MODE_ROTATION) -> (u32, u32) {
    if matches!(
        rotation,
        DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
    ) {
        (height, width)
    } else {
        (width, height)
    }
}
fn desktop_rect(desc: &DXGI_OUTDUPL_DESC, coordinates: RECT) -> DisplayRect {
    let (width, height) = oriented_size(desc.ModeDesc.Width, desc.ModeDesc.Height, desc.Rotation);
    DisplayRect {
        x: coordinates.left,
        y: coordinates.top,
        width,
        height,
    }
}
fn duplicate(output: &display::Output, gpu: &Gpu) -> Result<IDXGIOutputDuplication> {
    // SAFETY: the device belongs to the selected output's adapter.
    unsafe {
        output
            .output
            .cast::<IDXGIOutput5>()
            .and_then(|o| o.DuplicateOutput1(&gpu.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]))
            .or_else(|_| {
                output
                    .output
                    .cast::<IDXGIOutput1>()?
                    .DuplicateOutput(&gpu.device)
            })
    }
    .context("duplicate desktop output")
}
// SAFETY: D3D11/DXGI interfaces are free threaded and the immediate context has
// multithread protection. Capture is used exclusively by its video worker; no
// apartment-bound MF interfaces or COM initialization guard travel with it.
// GDI DCs, when needed, are created, used and dropped on that same worker.
unsafe impl Send for Capture {}
impl Capture {
    pub fn new() -> Result<Self> {
        let _dpi = display::DpiAwareness::physical();
        let output = display::select()?;
        let gpu = Gpu::new(&output.adapter)?;
        let duplication = duplicate(&output, &gpu)
            .map_err(|e| {
                tracing::warn!("Desktop Duplication unavailable; using GDI capture: {e:#}")
            })
            .ok();
        let gdi = if duplication.is_none() {
            Some(Gdi::new(output.desc.DeviceName)?)
        } else {
            None
        };
        // SAFETY: The duplication owns its descriptor.
        let desc = duplication
            .as_ref()
            .map(|d| unsafe { d.GetDesc() })
            .unwrap_or_else(|| {
                let display = gdi.as_ref().expect("capture backend").display;
                DXGI_OUTDUPL_DESC {
                    ModeDesc: DXGI_MODE_DESC {
                        Width: display.width,
                        Height: display.height,
                        ..Default::default()
                    },
                    Rotation: DXGI_MODE_ROTATION_IDENTITY,
                    ..Default::default()
                }
            });
        let display = desktop_rect(&desc, output.desc.DesktopCoordinates);
        let source = gpu.texture(
            desc.ModeDesc.Width,
            desc.ModeDesc.Height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let frame = gpu.texture(
            display.width,
            display.height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let renderer = CursorRenderer::new(&gpu)?;
        Ok(Self {
            gpu,
            duplication,
            source,
            frame,
            renderer,
            pointer: Default::default(),
            shape: Default::default(),
            rotation: desc.Rotation,
            display,
            has_frame: false,
            cpu_seeded: false,
            retry: ReopenBackoff::new(),
            waiting_since: None,
            output_name: output.desc.DeviceName,
            gdi,
        })
    }
    fn reopen(&mut self) -> Result<()> {
        let _dpi = display::DpiAwareness::physical();
        let output = display::select()?;
        // Reuse the device and cursor shaders while the adapter is unchanged.
        // SAFETY: both descriptors are copied from live DXGI interfaces.
        let (old, new) = unsafe {
            (
                self.gpu
                    .device
                    .cast::<IDXGIDevice>()?
                    .GetAdapter()?
                    .GetDesc()?
                    .AdapterLuid,
                output.adapter.GetDesc1()?.AdapterLuid,
            )
        };
        if old.HighPart != new.HighPart || old.LowPart != new.LowPart {
            *self = Self::new()?;
            return Ok(());
        }
        let duplication = duplicate(&output, &self.gpu)?;
        // SAFETY: duplication owns its descriptor.
        let desc = unsafe { duplication.GetDesc() };
        let source = self.gpu.texture(
            desc.ModeDesc.Width,
            desc.ModeDesc.Height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let display = desktop_rect(&desc, output.desc.DesktopCoordinates);
        let frame = self.gpu.texture(
            display.width,
            display.height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        self.source = source;
        self.frame = frame;
        self.display = display;
        self.rotation = desc.Rotation;
        self.pointer = Default::default();
        self.shape = Default::default();
        self.renderer.shape = None;
        self.has_frame = false;
        self.cpu_seeded = false;
        self.duplication = Some(duplication);
        self.retry = ReopenBackoff::new();
        self.waiting_since = None;
        self.output_name = output.desc.DeviceName;
        Ok(())
    }
    fn grab_gdi(&mut self) -> Result<bool> {
        let capture = self.gdi.as_mut().context("missing GDI capture")?;
        let (display, bytes) = capture.grab()?;
        if self.display != display {
            self.display = display;
            self.frame = self.gpu.texture(
                self.display.width,
                self.display.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_RENDER_TARGET.0 as u32,
                false,
            )?;
        }
        // SAFETY: The top-down bitmap covers every row of this BGRA texture;
        // UpdateSubresource copies the pixels before the bitmap is reused.
        unsafe {
            self.gpu.context.UpdateSubresource(
                &self.frame,
                0,
                None,
                bytes.as_ptr().cast(),
                self.display.width * 4,
                0,
            );
        }
        self.has_frame = true;
        Ok(true)
    }
    pub fn grab(&mut self, timeout: Duration) -> Result<bool> {
        if self.gdi.is_some() {
            return self.grab_gdi();
        }
        if self.duplication.is_none() {
            if Instant::now() < self.retry.at {
                return Ok(false);
            }
            if let Err(e) = self.reopen() {
                tracing::debug!("desktop duplication still unavailable: {e:#}");
                self.retry.defer(Instant::now());
                return Ok(false);
            }
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: Owned duplication and valid output slots; acquisition is always
        // paired with ReleaseFrame before rendering or error propagation.
        let acquired = unsafe {
            self.duplication
                .as_ref()
                .context("missing desktop duplication")?
                .AcquireNextFrame(
                    timeout.as_millis().min(u32::MAX as u128) as u32,
                    &mut info,
                    &mut resource,
                )
        };
        match acquired {
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                if !self.has_frame
                    && self
                        .waiting_since
                        .get_or_insert_with(Instant::now)
                        .elapsed()
                        >= Duration::from_secs(1)
                {
                    tracing::warn!(
                        "Desktop Duplication produced no initial frame; using GDI capture"
                    );
                    self.duplication = None;
                    self.gdi = Some(Gdi::new(self.output_name)?);
                    return self.grab_gdi();
                }
                return Ok(false);
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Release the old interface before DuplicateOutput: a process can
                // own only one duplication per output. Retain the last safe copy
                // while a secure desktop temporarily prevents reopening.
                self.duplication = None;
                self.retry.defer(Instant::now());
                return Ok(false);
            }
            Err(e) => return Err(e).context("acquire desktop frame"),
            Ok(()) => {}
        }
        let mut shape_bytes = None;
        let copied = (|| -> Result<()> {
            let texture: ID3D11Texture2D = resource.context("DXGI returned no frame")?.cast()?;
            // Use the acquired texture as the final authority on physical size;
            // DesktopCoordinates can be virtualised for DPI-unaware processes.
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            let mut source = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: both descriptors are copied from live owned textures.
            unsafe {
                texture.GetDesc(&mut desc);
                self.source.GetDesc(&mut source);
            }
            if (desc.Width, desc.Height) != (source.Width, source.Height) {
                self.source = self.gpu.texture(
                    desc.Width,
                    desc.Height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    false,
                )?;
                let (width, height) = oriented_size(desc.Width, desc.Height, self.rotation);
                self.frame = self.gpu.texture(
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    false,
                )?;
                self.display.width = width;
                self.display.height = height;
            }
            // SAFETY: these textures have identical dimensions and BGRA format.
            unsafe {
                self.gpu.context.CopyResource(&self.source, &texture);
            }
            if info.LastMouseUpdateTime != 0 {
                self.pointer = info.PointerPosition;
            }
            if info.PointerShapeBufferSize > 0 {
                let mut bytes = vec![0; info.PointerShapeBufferSize as usize];
                let mut required = 0;
                // SAFETY: The shape buffer is sized from this acquired frame.
                unsafe {
                    self.duplication
                        .as_ref()
                        .context("missing desktop duplication")?
                        .GetFramePointerShape(
                            bytes.len() as u32,
                            bytes.as_mut_ptr().cast(),
                            &mut required,
                            &mut self.shape,
                        )
                }
                .context("read DXGI pointer shape")?;
                shape_bytes = Some(bytes);
            }
            Ok(())
        })();
        // SAFETY: AcquireNextFrame succeeded; no borrowed frame escapes this call.
        let released = unsafe {
            self.duplication
                .as_ref()
                .context("missing desktop duplication")?
                .ReleaseFrame()
        }
        .context("release desktop frame");
        copied?;
        released?;
        if let Some(bytes) = shape_bytes {
            self.renderer.shape(&self.gpu, &self.shape, &bytes)?;
        }
        self.renderer.draw(
            &self.gpu,
            &self.source,
            &self.frame,
            &self.pointer,
            &self.shape,
            self.rotation,
        )?;
        self.has_frame = true;
        Ok(true)
    }
}
impl CpuCapture for Capture {
    fn size(&self) -> (u32, u32) {
        (self.display.width, self.display.height)
    }
    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> Result<bool> {
        let timeout = if !self.cpu_seeded && self.has_frame {
            Duration::ZERO
        } else {
            timeout
        };
        let changed = self.grab(timeout)?;
        if !changed && (self.cpu_seeded || !self.has_frame) {
            return Ok(false);
        }
        let (data, stride) = self.gpu.read(&self.frame, self.display.height as usize)?;
        self.cpu_seeded = true;
        *frame = Bgra {
            width: self.display.width,
            height: self.display.height,
            stride,
            data,
        };
        Ok(true)
    }
    fn display(&self) -> DisplayRect {
        self.display
    }
    fn name(&self) -> String {
        if self.gdi.is_some() {
            "Windows GDI".into()
        } else {
            "DXGI Desktop Duplication".into()
        }
    }
}

pub(super) struct Converter {
    gpu: Gpu,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    width: u32,
    height: u32,
}
impl Converter {
    pub fn new(gpu: &Gpu, input: (u32, u32), output: (u32, u32), fps: u32) -> Result<Self> {
        // SAFETY: D3D video interfaces refer to the same device/context.
        unsafe {
            let video: ID3D11VideoDevice = gpu.device.cast().context("D3D11 video device")?;
            let context: ID3D11VideoContext = gpu.context.cast()?;
            let rate = DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            };
            let enumerator = video
                .CreateVideoProcessorEnumerator(&D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                    InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    InputFrameRate: rate,
                    InputWidth: input.0,
                    InputHeight: input.1,
                    OutputFrameRate: rate,
                    OutputWidth: output.0,
                    OutputHeight: output.1,
                    Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                })
                .context("enumerate video processor")?;
            anyhow::ensure!(
                enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?
                    & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32
                    != 0,
                "video processor cannot read BGRA"
            );
            anyhow::ensure!(
                enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?
                    & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32
                    != 0,
                "video processor cannot produce NV12"
            );
            let processor = video
                .CreateVideoProcessor(&enumerator, 0)
                .context("create BGRA to NV12 processor")?;
            // Bitfield: YCbCr_Matrix=1 (709), nominal range=2 (16..235).
            context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (2 << 4),
                },
            );
            // RGB input is full range; the matrix bit selects 709 conversion.
            context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (1 << 4),
                },
            );
            if let Ok(context1) = context.cast::<ID3D11VideoContext1>() {
                context1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                context1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            }
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let rect = RECT {
                left: 0,
                top: 0,
                right: output.0 as i32,
                bottom: output.1 as i32,
            };
            let fitted = DisplayRect {
                width: input.0,
                height: input.1,
                ..Default::default()
            }
            .fitted(output.0, output.1);
            let content = RECT {
                left: fitted.x,
                top: fitted.y,
                right: fitted.x + fitted.width as i32,
                bottom: fitted.y + fitted.height as i32,
            };
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&content));
            context.VideoProcessorSetOutputBackgroundColor(
                &processor,
                true,
                &D3D11_VIDEO_COLOR {
                    Anonymous: D3D11_VIDEO_COLOR_0 {
                        YCbCr: D3D11_VIDEO_COLOR_YCbCrA {
                            Y: 0.0,
                            Cb: 0.5,
                            Cr: 0.5,
                            A: 1.0,
                        },
                    },
                },
            );
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect));
            Ok(Self {
                gpu: gpu.clone(),
                enumerator,
                processor,
                video,
                context,
                width: output.0,
                height: output.1,
            })
        }
    }
    pub fn convert(&self, input: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let texture = self.gpu.texture(
            self.width,
            self.height,
            DXGI_FORMAT_NV12,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let (mut iv, mut ov) = (None, None);
        // SAFETY: Views and textures belong to the same video device. ManuallyDrop
        // fields in the Win32 ABI structs are explicitly released after the blit.
        unsafe {
            self.video
                .CreateVideoProcessorInputView(
                    input,
                    &self.enumerator,
                    &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        FourCC: 0,
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPIV {
                                MipSlice: 0,
                                ArraySlice: 0,
                            },
                        },
                    },
                    Some(&mut iv),
                )
                .context("video processor input view")?;
            self.video
                .CreateVideoProcessorOutputView(
                    &texture,
                    &self.enumerator,
                    &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    },
                    Some(&mut ov),
                )
                .context("video processor output view")?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(iv),
                ..Default::default()
            };
            let result = self
                .context
                .VideoProcessorBlt(
                    &self.processor,
                    &ov.context("missing processor output view")?,
                    0,
                    std::slice::from_ref(&stream),
                )
                .context("convert BGRA to NV12");
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
        }
        Ok(texture)
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn physical_texture_size_and_reopen_backoff() {
        use super::*;
        assert_eq!(
            oriented_size(1920, 1080, DXGI_MODE_ROTATION_IDENTITY),
            (1920, 1080)
        );
        assert_eq!(
            oriented_size(1920, 1080, DXGI_MODE_ROTATION_ROTATE90),
            (1080, 1920)
        );
        assert_eq!(
            oriented_size(1920, 1080, DXGI_MODE_ROTATION_ROTATE270),
            (1080, 1920)
        );
        let desc = DXGI_OUTDUPL_DESC {
            ModeDesc: DXGI_MODE_DESC {
                Width: 1920,
                Height: 1080,
                ..Default::default()
            },
            Rotation: DXGI_MODE_ROTATION_IDENTITY,
            ..Default::default()
        };
        // A DPI-unaware output rectangle must never shrink the physical frame.
        let display = desktop_rect(
            &desc,
            RECT {
                left: -1280,
                top: 20,
                right: 0,
                bottom: 740,
            },
        );
        assert_eq!(
            display,
            DisplayRect {
                x: -1280,
                y: 20,
                width: 1920,
                height: 1080
            }
        );
        let mut retry = ReopenBackoff::new();
        let now = Instant::now();
        for millis in [50, 100, 200, 400, 800, 1000, 1000] {
            retry.defer(now);
            assert_eq!(retry.at.duration_since(now), Duration::from_millis(millis));
        }
    }
    #[test]
    fn cursor_shaders_compile_without_a_gpu() -> anyhow::Result<()> {
        super::compile_shader(windows::core::s!("vs"), windows::core::s!("vs_4_0"))?;
        super::compile_shader(windows::core::s!("ps"), windows::core::s!("ps_4_0"))?;
        Ok(())
    }
    #[test]
    fn desktop_duplication_frame_when_interactive() -> anyhow::Result<()> {
        let _runtime = match super::super::runtime::Runtime::new(false) {
            Ok(r) => r,
            Err(e) => {
                if super::super::unavailable(&e, "COM") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        let mut capture = match super::Capture::new() {
            Ok(c) => c,
            Err(e) => {
                if super::super::unavailable(&e, "Desktop Duplication") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        match capture.grab(std::time::Duration::from_secs(1)) {
            Ok(true) => {
                assert!(capture.display.width > 0);
            }
            Ok(false) => eprintln!("SKIP desktop produced no frame"),
            Err(e) => {
                if !super::super::unavailable(&e, "interactive desktop") {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}
