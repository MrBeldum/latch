//! The cursor uses a pixel shader compiled by the system d3dcompiler_47.dll.
//! This preserves XOR cursors without reading the desktop back to the CPU.
use super::display;
use crate::{
    input::DisplayRect,
    software::{Bgra, CpuCapture},
};
use anyhow::{bail, Context, Result};
use std::{
    mem::ManuallyDrop,
    thread,
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
    /// Runs `use_rows` on the first `rows` rows of `texture` as the CPU sees
    /// them, with their pitch. `staging` keeps the CPU-readable copy between
    /// calls; it is replaced when the texture's size or format changes.
    pub fn read<R>(
        &self,
        staging: &mut Option<ID3D11Texture2D>,
        texture: &ID3D11Texture2D,
        rows: usize,
        use_rows: impl FnOnce(&[u8], usize) -> R,
    ) -> Result<R> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: The texture and output descriptor are valid.
        unsafe {
            texture.GetDesc(&mut desc);
        }
        let mut have = D3D11_TEXTURE2D_DESC::default();
        if let Some(staging) = staging {
            // SAFETY: As above.
            unsafe { staging.GetDesc(&mut have) };
        }
        let stale = staging.is_none()
            || (have.Width, have.Height, have.Format) != (desc.Width, desc.Height, desc.Format);
        if stale {
            *staging = Some(self.texture(desc.Width, desc.Height, desc.Format, 0, true)?);
        }
        let staging = staging.as_ref().context("missing staging texture")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: CopyResource is ordered before Map. Map is paired with Unmap;
        // the row pitch describes all accessible bytes handed to `use_rows`.
        unsafe {
            self.context.CopyResource(staging, texture);
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("map staging texture")?;
            let result = use_rows(
                std::slice::from_raw_parts(
                    mapped.pData.cast::<u8>(),
                    mapped.RowPitch as usize * rows,
                ),
                mapped.RowPitch as usize,
            );
            self.context.Unmap(staging, 0);
            Ok(result)
        }
    }
}
struct CursorRenderer {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    shape: Option<ID3D11ShaderResourceView>,
    /// Views of the textures last drawn from and to, made once: a view holds
    /// its texture, so one cannot be mistaken for a new texture at the
    /// same address, and a new texture (after a resize) gets new views.
    source: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    target: Option<(ID3D11Texture2D, ID3D11RenderTargetView)>,
}
const SHADER: &str = r#"
Texture2D<float4> desktop : register(t0);
Texture2D<uint> shape : register(t1);
cbuffer C : register(b0) { int2 origin; uint2 size; uint kind; uint rotation; uint2 rawsize; uint hdr; float white; uint output_pq; uint padding; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    return float4(id == 2 ? 3 : -1, id == 1 ? 3 : -1, 0, 1);
}
float3 pq(float3 lin) {
    float3 n = pow(clamp(lin,0,1000)/10000.0, 2610.0/16384.0);
    return pow((3424.0/4096.0 + (2413.0/128.0)*n)/(1+(2392.0/128.0)*n),2523.0/32.0);
}
float3 wide(float3 rgb) {
    return mul(float3x3(0.627404,0.329283,0.043313,0.069097,0.919540,0.011362,0.016391,0.088013,0.895595),rgb);
}
// latch_proto::color::hdr_to_sdr: BT.709 gamut toward the colour's own
// luminance, then a highlight shoulder on max(R,G,B) up to `peak`.
float3 hdr_to_sdr(float3 c, float peak) {
    float y = dot(c, float3(0.2126,0.7152,0.0722));
    if(y <= 0) return 0;
    float lo = min(min(c.r,c.g),c.b);
    if(lo < 0) c = y + (c - y)*(y/(y - lo));
    float m = max(max(c.r,c.g),c.b);
    if(m <= 0.75) return c;
    float x = (m - 0.75)/0.25, top = max((peak - 0.75)/0.25, 1.0);
    float g = min(x*(1 + x/(top*top))/(1 + x), 1.0);
    return saturate(c*((0.75 + 0.25*g)/m));
}
float3 linearize(float3 c) { return float3(c.r <= 0.04045 ? c.r/12.92 : pow((c.r+0.055)/1.055,2.4),c.g <= 0.04045 ? c.g/12.92 : pow((c.g+0.055)/1.055,2.4),c.b <= 0.04045 ? c.b/12.92 : pow((c.b+0.055)/1.055,2.4)); }
float4 ps(float4 p : SV_Position) : SV_Target {
    int2 pos = int2(p.xy); int2 src = pos;
    if(rotation == 2) src = int2(pos.y, rawsize.y - 1 - pos.x);
    if(rotation == 3) src = int2(rawsize.x - 1 - pos.x, rawsize.y - 1 - pos.y);
    if(rotation == 4) src = int2(rawsize.x - 1 - pos.y, pos.x);
    float4 bg = desktop.Load(int3(src,0));
    if(output_pq != 0) {
        bg.rgb = pq(wide(hdr != 0 ? bg.rgb*80.0 : linearize(bg.rgb)*white*80.0));
    } else if(hdr != 0) {
        // scRGB is linear BT.709, with SDR white given by the display's white
        // level. Keep highlights up to the 1000-nit HDR ceiling apart and in
        // hue rather than clipping each channel, then apply the sRGB transfer
        // before cursor blending.
        float3 lin = hdr_to_sdr(bg.rgb / white, 1000.0/80.0/white);
        bg.rgb = float3(lin.r <= 0.0031308 ? 12.92*lin.r : 1.055*pow(lin.r,1.0/2.4)-0.055,
                        lin.g <= 0.0031308 ? 12.92*lin.g : 1.055*pow(lin.g,1.0/2.4)-0.055,
                        lin.b <= 0.0031308 ? 12.92*lin.b : 1.055*pow(lin.b,1.0/2.4)-0.055);
    }
    int2 c = pos - origin;
    if(kind == 0 || any(c < 0) || any(c >= int2(size))) return bg;
    uint v = shape.Load(int3(c,0));
    uint3 rgb = uint3((v >> 16) & 255, (v >> 8) & 255, v & 255);
    uint a = v >> 24;
    if(output_pq != 0) {
        float3 cursor = pq(wide(linearize(float3(rgb)/255.0)*white*80.0));
        if(kind == 2) return float4(cursor*(a/255.0)+bg.rgb*(1-a/255.0),1);
        if(kind == 4) return a == 255 ? float4(abs(bg.rgb-cursor),1) : float4(cursor,1);
        uint3 screen = uint3(round(saturate(bg.rgb)*255));
        return float4(float3((screen & a) ^ rgb)/255.0,1);
    }
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
                        ByteWidth: 48,
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
            source: None,
            target: None,
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
        &mut self,
        gpu: &Gpu,
        source: &ID3D11Texture2D,
        target: &ID3D11Texture2D,
        cursor: &DXGI_OUTDUPL_POINTER_POSITION,
        shape: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        color: (DXGI_MODE_ROTATION, f32, bool),
    ) -> Result<()> {
        let (rotation, white, output_pq) = color;
        let (mut raw, mut dst) = (
            D3D11_TEXTURE2D_DESC::default(),
            D3D11_TEXTURE2D_DESC::default(),
        );
        // SAFETY: Views reference owned textures and constants contain exactly 48 bytes.
        unsafe {
            source.GetDesc(&mut raw);
            target.GetDesc(&mut dst);
            if self
                .source
                .as_ref()
                .is_none_or(|(texture, _)| texture.as_raw() != source.as_raw())
            {
                let mut view = None;
                gpu.device
                    .CreateShaderResourceView(source, None, Some(&mut view))
                    .context("desktop shader view")?;
                self.source = Some((source.clone(), view.context("missing desktop view")?));
            }
            if self
                .target
                .as_ref()
                .is_none_or(|(texture, _)| texture.as_raw() != target.as_raw())
            {
                let mut view = None;
                gpu.device
                    .CreateRenderTargetView(target, None, Some(&mut view))
                    .context("cursor render target")?;
                self.target = Some((target.clone(), view.context("missing cursor target")?));
            }
            let srv = self.source.as_ref().map(|(_, view)| view.clone());
            let rtv = self.target.as_ref().map(|(_, view)| view.clone());
            let constants: [u32; 12] = [
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
                u32::from(raw.Format == DXGI_FORMAT_R16G16B16A16_FLOAT),
                white.to_bits(),
                u32::from(output_pq),
                0,
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
    pub hdr_output: bool,
    ten_bit: bool,
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
    pub captured: Instant,
    cpu_seeded: bool,
    white: f32,
    retry: ReopenBackoff,
    staging: Option<ID3D11Texture2D>,
}
struct ReopenBackoff {
    started: Instant,
    at: Instant,
    delay: Duration,
}
impl ReopenBackoff {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            at: Instant::now(),
            delay: Duration::from_millis(50),
        }
    }
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= Duration::from_secs(5)
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
    // SAFETY: the device belongs to this output. Once Output5 is available,
    // never fall back on a failed acquisition to legacy BGRA conversion: that
    // can silently turn an Advanced Color desktop black.
    unsafe {
        match output.output.cast::<IDXGIOutput5>() {
            Ok(output) => output.DuplicateOutput1(
                &gpu.device,
                0,
                &[
                    DXGI_FORMAT_R16G16B16A16_FLOAT,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                ],
            ),
            Err(_) => output
                .output
                .cast::<IDXGIOutput1>()?
                .DuplicateOutput(&gpu.device),
        }
    }
    .context("duplicate desktop output")
}
// SAFETY: D3D11/DXGI interfaces are free threaded and the immediate context has
// multithread protection. Capture is used exclusively by its video worker; no
// apartment-bound MF interfaces or COM initialization guard travel with it.
unsafe impl Send for Capture {}
impl Capture {
    #[cfg(test)]
    pub fn new() -> Result<Self> {
        Self::new_color(Default::default())
    }
    pub fn new_color(color: latch_proto::color::ColorDescription) -> Result<Self> {
        let _dpi = display::DpiAwareness::physical();
        let output = display::select()?;
        let ten_bit = color.bit_depth == 10;
        let hdr_output = color.is_hdr()
            && unsafe {
                output
                    .output
                    .cast::<IDXGIOutput6>()
                    .and_then(|o| o.GetDesc1())
                    .is_ok_and(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
            };
        let gpu = Gpu::new(&output.adapter)?;
        let duplication = duplicate(&output, &gpu)?;
        // SAFETY: The duplication owns its descriptor.
        let desc = unsafe { duplication.GetDesc() };
        let display = desktop_rect(&desc, output.desc.DesktopCoordinates);
        let source = gpu.texture(
            desc.ModeDesc.Width,
            desc.ModeDesc.Height,
            desc.ModeDesc.Format,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let frame = gpu.texture(
            display.width,
            display.height,
            if ten_bit {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let renderer = CursorRenderer::new(&gpu)?;
        Ok(Self {
            hdr_output,
            ten_bit,
            gpu,
            duplication: Some(duplication),
            source,
            frame,
            renderer,
            pointer: Default::default(),
            shape: Default::default(),
            rotation: desc.Rotation,
            display,
            has_frame: false,
            captured: Instant::now(),
            cpu_seeded: false,
            white: display::sdr_white_level(&output.desc.DeviceName),
            retry: ReopenBackoff::new(),
            staging: None,
        })
    }
    /// See [`Gpu::read`]; the staging texture is kept for the next call.
    pub fn read<R>(
        &mut self,
        texture: &ID3D11Texture2D,
        rows: usize,
        use_rows: impl FnOnce(&[u8], usize) -> R,
    ) -> Result<R> {
        self.gpu.read(&mut self.staging, texture, rows, use_rows)
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
            let mut color = latch_proto::color::ColorDescription {
                bit_depth: if self.ten_bit { 10 } else { 8 },
                ..Default::default()
            };
            if self.hdr_output {
                color.transfer = 16;
            }
            let hdr_output = self.hdr_output;
            *self = Self::new_color(color)?;
            self.hdr_output = hdr_output;
            return Ok(());
        }
        let duplication = duplicate(&output, &self.gpu)?;
        // SAFETY: duplication owns its descriptor.
        let desc = unsafe { duplication.GetDesc() };
        let source = self.gpu.texture(
            desc.ModeDesc.Width,
            desc.ModeDesc.Height,
            desc.ModeDesc.Format,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let display = desktop_rect(&desc, output.desc.DesktopCoordinates);
        let frame = self.gpu.texture(
            display.width,
            display.height,
            if self.ten_bit {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        self.white = display::sdr_white_level(&output.desc.DeviceName);
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
        Ok(())
    }
    pub fn grab(&mut self, timeout: Duration) -> Result<bool> {
        if self.duplication.is_none() {
            anyhow::ensure!(
                !self.retry.expired(Instant::now()),
                "desktop duplication unavailable for five seconds after access loss"
            );
            let now = Instant::now();
            if now < self.retry.at {
                // Nothing to wait on: sleep out the caller's wait (or the
                // backoff) instead of having it poll in a loop.
                thread::sleep(timeout.min(self.retry.at - now));
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
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Release the old interface before DuplicateOutput: a process can
                // own only one duplication per output. Mark the copy unavailable
                // so repeated reopen failures cannot stream a stale desktop forever.
                self.duplication = None;
                self.has_frame = false;
                self.cpu_seeded = false;
                self.retry = ReopenBackoff::new();
                self.retry.defer(Instant::now());
                return Ok(false);
            }
            Err(e) => return Err(e).context("acquire desktop frame"),
            Ok(()) => {}
        }
        self.captured = Instant::now();
        let mut shape_bytes = None;
        let mut composite = true;
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
            if (desc.Width, desc.Height, desc.Format)
                != (source.Width, source.Height, source.Format)
            {
                self.source = self.gpu.texture(
                    desc.Width,
                    desc.Height,
                    desc.Format,
                    D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    false,
                )?;
                let (width, height) = oriented_size(desc.Width, desc.Height, self.rotation);
                self.frame = self.gpu.texture(
                    width,
                    height,
                    if self.ten_bit {
                        DXGI_FORMAT_R10G10B10A2_UNORM
                    } else {
                        DXGI_FORMAT_B8G8R8A8_UNORM
                    },
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    false,
                )?;
                self.display.width = width;
                self.display.height = height;
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
            composite = self.ten_bit
                || self.pointer.Visible.as_bool()
                || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
                || !matches!(
                    self.rotation,
                    DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED
                );
            // SAFETY: the selected destination has the same size/format. A
            // plain SDR desktop needs only its retained copy, no cursor pass.
            unsafe {
                self.gpu
                    .context
                    .CopyResource(if composite { &self.source } else { &self.frame }, &texture);
            }
            Ok(())
        })();
        // SAFETY: each successful acquisition is released, including copy errors.
        let released = unsafe {
            self.duplication
                .as_ref()
                .context("missing duplication")?
                .ReleaseFrame()
        }
        .context("release desktop frame");
        copied?;
        released?;
        anyhow::ensure!(
            !info.ProtectedContentMaskedOut.as_bool(),
            "Windows masked protected display content"
        );
        if let Some(bytes) = shape_bytes {
            self.renderer.shape(&self.gpu, &self.shape, &bytes)?;
        }
        if composite {
            self.renderer.draw(
                &self.gpu,
                &self.source,
                &self.frame,
                &self.pointer,
                &self.shape,
                (self.rotation, self.white, self.hdr_output),
            )?;
        }
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
        // Fill the caller's buffer in place: a 1080p frame is 8 MB.
        let mut data = std::mem::take(&mut frame.data);
        let texture = self.frame.clone();
        let stride = self.read(&texture, self.display.height as usize, |bytes, stride| {
            data.clear();
            data.extend_from_slice(bytes);
            stride
        })?;
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
        "DXGI Desktop Duplication".into()
    }
}

/// NV12 textures the converter rotates through. The encoder is handed one
/// per frame and, for a hardware MFT, holds it only until it has returned
/// that frame's bitstream (`Encoder::encode` waits for it), or for a
/// synchronous MFT that buffers its first inputs, a frame or two longer;
/// the converter does not write a texture again until three others have
/// been written since.
pub(super) const RING: usize = 4;

struct Slot {
    texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
}

/// A converted frame: the ring slot it is in and its texture.
pub(super) struct Converted {
    pub slot: usize,
    pub texture: ID3D11Texture2D,
}

pub(super) struct Converter {
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    slots: Vec<Slot>,
    next: usize,
    /// The view of the last input, made once. It holds its texture, so the
    /// texture cannot be freed and another allocated at the same address
    /// while this is cached; a new texture (after a resize) gets a new view.
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}
impl Converter {
    #[cfg(test)]
    pub fn new(gpu: &Gpu, input: (u32, u32), output: (u32, u32), fps: u32) -> Result<Self> {
        Self::with_color(gpu, input, output, fps, Default::default())
    }
    pub fn with_color(
        gpu: &Gpu,
        input: (u32, u32),
        output: (u32, u32),
        fps: u32,
        color: latch_proto::color::ColorDescription,
    ) -> Result<Self> {
        let format = if color.bit_depth == 10 {
            DXGI_FORMAT_P010
        } else {
            DXGI_FORMAT_NV12
        };
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
                enumerator.CheckVideoProcessorFormat(format)?
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
            if color.is_hdr() {
                context
                    .cast::<ID3D11VideoContext1>()
                    .context("HDR colour conversion requires D3D11.1")?;
            }
            if let Ok(context1) = context.cast::<ID3D11VideoContext1>() {
                context1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    if color.is_hdr() {
                        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                    } else {
                        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
                    },
                );
                context1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    if color.is_hdr() {
                        DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020
                    } else {
                        DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709
                    },
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
            let mut slots = Vec::with_capacity(RING);
            for _ in 0..RING {
                let texture = gpu.texture(
                    output.0,
                    output.1,
                    format,
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    false,
                )?;
                let mut view = None;
                video
                    .CreateVideoProcessorOutputView(
                        &texture,
                        &enumerator,
                        &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                            },
                        },
                        Some(&mut view),
                    )
                    .context("video processor output view")?;
                slots.push(Slot {
                    texture,
                    view: view.context("missing processor output view")?,
                });
            }
            Ok(Self {
                processor,
                enumerator,
                video,
                context,
                slots,
                next: 0,
                input: None,
            })
        }
    }
    pub fn convert(&mut self, input: &ID3D11Texture2D) -> Result<Converted> {
        // SAFETY: Views and textures belong to the same video device. ManuallyDrop
        // fields in the Win32 ABI structs are explicitly released after the blit.
        unsafe {
            if self
                .input
                .as_ref()
                .is_none_or(|(texture, _)| texture.as_raw() != input.as_raw())
            {
                let mut view = None;
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
                        Some(&mut view),
                    )
                    .context("video processor input view")?;
                self.input = Some((input.clone(), view.context("missing processor input view")?));
            }
            let slot = self.next;
            self.next = (self.next + 1) % RING;
            let view = self.input.as_ref().map(|(_, view)| view.clone());
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(view),
                ..Default::default()
            };
            let result = self
                .context
                .VideoProcessorBlt(
                    &self.processor,
                    &self.slots[slot].view,
                    0,
                    std::slice::from_ref(&stream),
                )
                .context("convert BGRA to NV12");
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            Ok(Converted {
                slot,
                texture: self.slots[slot].texture.clone(),
            })
        }
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
        assert!(!retry.expired(retry.started + Duration::from_secs(4)));
        assert!(retry.expired(retry.started + Duration::from_secs(5)));
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
    /// IEEE half from f32 for the normal, finite values a test uses.
    fn half(v: f32) -> u16 {
        if v == 0.0 {
            return 0;
        }
        let bits = v.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        let mantissa = ((bits & 0x7f_ffff) + 0x1000) >> 13;
        sign | (((exponent as u16) << 10) + mantissa as u16)
    }
    /// An HDR (Advanced Color) desktop captured for an SDR stream: neutral
    /// 203/400/600/1000-nit patches stay apart, saturated and wide-gamut
    /// colours keep their hue, SDR content below the knee is unchanged and
    /// nothing goes black. Matches latch_proto's reference within a code.
    #[test]
    fn fp16_sc_rgb_hdr_desktop_keeps_highlights_in_sdr() -> anyhow::Result<()> {
        use super::*;
        use latch_proto::color::{bt2020_to_bt709, hdr_to_sdr, srgb_oetf};
        // This tests shader output, not capture or video acceleration. WARP
        // runs the shader even on CI adapters without D3D11 video support.
        let (mut device, mut context) = (None, None);
        // SAFETY: Valid output pointers; WARP requires no explicit adapter.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let gpu = Gpu {
            device: device.context("WARP returned no device")?,
            context: context.context("WARP returned no context")?,
        };
        let white_nits = 203.0f64;
        let nits = |n: f64| n / 80.0;
        let mut patches: Vec<[f64; 3]> = [203.0, 400.0, 600.0, 1000.0, 100.0, 0.0]
            .iter()
            .map(|&n| [nits(n); 3])
            .collect();
        patches.extend([
            [nits(1000.0), 0.0, 0.0],
            [nits(1000.0), nits(500.0), 0.0],
            [nits(40.0), nits(80.0), nits(600.0)],
            bt2020_to_bt709([0.0, nits(1000.0), 0.0]),
        ]);
        let width = patches.len() as u32;
        let source = gpu.texture(
            width,
            1,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let target = gpu.texture(
            width,
            1,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let pixels: Vec<u16> = patches
            .iter()
            .flat_map(|p| {
                [
                    half(p[0] as f32),
                    half(p[1] as f32),
                    half(p[2] as f32),
                    0x3c00,
                ]
            })
            .collect();
        // Half floats round: compare against what the shader actually reads.
        let read: Vec<[f64; 3]> = pixels
            .chunks(4)
            .map(|p| {
                [0, 1, 2].map(|i| {
                    let h = p[i];
                    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
                    let e = i32::from((h >> 10) & 31);
                    let m = f64::from(h & 1023) / 1024.0;
                    if e == 0 {
                        sign * m * 2f64.powi(-14)
                    } else {
                        sign * (1.0 + m) * 2f64.powi(e - 15)
                    }
                })
            })
            .collect();
        // SAFETY: Upload covers the complete one-row texture.
        unsafe {
            gpu.context
                .UpdateSubresource(&source, 0, None, pixels.as_ptr().cast(), width * 8, 0);
        }
        let white = (white_nits / 80.0) as f32;
        CursorRenderer::new(&gpu)?.draw(
            &gpu,
            &source,
            &target,
            &Default::default(),
            &Default::default(),
            (DXGI_MODE_ROTATION_IDENTITY, white, false),
        )?;
        let out = gpu.read(&mut None, &target, 1, |bytes, _| bytes.to_vec())?;
        let bgr: Vec<[i32; 3]> = out[..width as usize * 4]
            .chunks(4)
            .map(|p| [i32::from(p[2]), i32::from(p[1]), i32::from(p[0])])
            .collect();
        for (i, rgb) in read.iter().enumerate() {
            let expected = hdr_to_sdr(rgb.map(|v| v / f64::from(white)), 1000.0 / white_nits)
                .map(|v| (255.0 * srgb_oetf(v)).round() as i32);
            assert!(
                (0..3).all(|c| (bgr[i][c] - expected[c]).abs() <= 1),
                "patch {i} {rgb:?}: shader {:?}, reference {expected:?}",
                bgr[i]
            );
        }
        eprintln!("HDR desktop → SDR codes: {bgr:?}");
        let neutral: Vec<i32> = bgr[..4].iter().map(|p| p[0]).collect();
        assert!(neutral.windows(2).all(|w| w[0] < w[1]), "{neutral:?}");
        assert_eq!(bgr[5], [0, 0, 0], "black stays black");
        assert!(
            bgr[4][0] > 150,
            "100-nit grey is not darkened: {:?}",
            bgr[4]
        );
        assert!(
            bgr[6..].iter().all(|p| p.iter().any(|&c| c > 128)),
            "{bgr:?}"
        );
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

#[cfg(test)]
mod timing_tests {
    use super::*;
    /// Capture updates without conversion, encoding, or a pacer. Run with
    /// desktop animation to distinguish DXGI/DWM cadence from encode limits.
    #[test]
    #[ignore]
    fn hardware_capture_rate() -> Result<()> {
        let _runtime = super::super::Runtime::new(false)?;
        for (width, height, fps) in [(1920, 1080, 240), (2560, 1440, 240), (3840, 2160, 120)] {
            let previous = display::prepare(width, height, fps)?;
            let result = (|| -> Result<()> {
                let mut capture = Capture::new()?;
                let start = Instant::now();
                let mut frames = 0;
                while start.elapsed() < Duration::from_secs(3) {
                    frames += usize::from(capture.grab(Duration::from_millis(100))?);
                }
                eprintln!(
                    "DXGI capture only {width}x{height}@{fps}: {:.1} updates/s",
                    frames as f64 / start.elapsed().as_secs_f64()
                );
                Ok(())
            })();
            if let Some(previous) = previous {
                display::restore(&previous)?;
            }
            result?;
        }
        Ok(())
    }
    #[test]
    #[ignore]
    fn hardware_conversion_timing() -> Result<()> {
        let _runtime = super::super::Runtime::new(false)?;
        let output = display::select()?;
        let gpu = Gpu::new(&output.adapter)?;
        // SAFETY: queries and textures belong to this device; GetData sizes
        // match SDK types. Disjoint timestamps are discarded, waits bounded.
        unsafe {
            for (width, height) in [(1920, 1080), (2560, 1440), (3840, 2160)] {
                let input = gpu.texture(
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    false,
                )?;
                let mut converter = Converter::new(&gpu, (width, height), (width, height), 240)?;
                let query = |kind| -> Result<ID3D11Query> {
                    let mut query = None;
                    gpu.device.CreateQuery(
                        &D3D11_QUERY_DESC {
                            Query: kind,
                            MiscFlags: 0,
                        },
                        Some(&mut query),
                    )?;
                    query.context("missing timing query")
                };
                let disjoint = query(D3D11_QUERY_TIMESTAMP_DISJOINT)?;
                let before = query(D3D11_QUERY_TIMESTAMP)?;
                let after = query(D3D11_QUERY_TIMESTAMP)?;
                let mut times = Vec::new();
                for i in 0..40 {
                    gpu.context.Begin(&disjoint);
                    gpu.context.End(&before);
                    converter.convert(&input)?;
                    gpu.context.End(&after);
                    gpu.context.End(&disjoint);
                    gpu.context.Flush();
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let read = |query: &ID3D11Query,
                                data: *mut std::ffi::c_void,
                                size: u32|
                     -> Result<()> {
                        loop {
                            // Preserve S_FALSE: the generated Result<()> wrapper
                            // considers it success while query data is not ready.
                            let status = (gpu.context.vtable().GetData)(
                                gpu.context.as_raw(),
                                query.as_raw(),
                                data,
                                size,
                                0,
                            );
                            if status == S_OK {
                                return Ok(());
                            }
                            status.ok()?;
                            anyhow::ensure!(Instant::now() < deadline, "GPU timestamps timed out");
                            thread::yield_now();
                        }
                    };
                    let mut timing = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
                    read(
                        &disjoint,
                        (&mut timing as *mut D3D11_QUERY_DATA_TIMESTAMP_DISJOINT).cast(),
                        std::mem::size_of_val(&timing) as u32,
                    )?;
                    let (mut start, mut end) = (0u64, 0u64);
                    read(&before, (&mut start as *mut u64).cast(), 8)?;
                    read(&after, (&mut end as *mut u64).cast(), 8)?;
                    if i >= 10 && !timing.Disjoint.as_bool() {
                        times.push((end - start) as f64 * 1000.0 / timing.Frequency as f64);
                    }
                }
                anyhow::ensure!(!times.is_empty(), "all timestamps disjoint");
                times.sort_by(f64::total_cmp);
                eprintln!(
                    "GPU BGRA→NV12 {width}x{height}: p50={:.3}ms p95={:.3}ms",
                    times[times.len() / 2],
                    times[times.len() * 95 / 100]
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod hdr_probe {
    use super::*;
    use crate::video::{Codec, VideoConfig};

    #[test]
    #[ignore = "requires a hardware HEVC Main10 encoder; writes HDR probe artifacts"]
    fn scrgb_pq_p010_hardware_roundtrip() -> Result<()> {
        let _runtime = super::super::runtime::Runtime::new(true)?;
        let output = display::select()?;
        let gpu = Gpu::new(&output.adapter)?;
        let color = latch_proto::color::ColorDescription::negotiate(
            latch_proto::color::ColorCapabilities {
                hdr: true,
                ten_bit: true,
                peak_nits: 1000,
            },
            true,
            true,
        );
        let config = VideoConfig {
            color,
            codec: Codec::Hevc,
            width: 960,
            height: 540,
            fps: 60,
            bitrate_kbps: 20000,
        };
        let source = gpu.texture(
            960,
            540,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            false,
        )?;
        let target = gpu.texture(
            960,
            540,
            DXGI_FORMAT_R10G10B10A2_UNORM,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            false,
        )?;
        let pixels: Vec<u16> = (0..960 * 540)
            .flat_map(|i| {
                let v = [0x3000, 0x3d00, 0x4a40][(i % 960) / 320]; // 10, 100, 1000 cd/m² in scRGB.
                [v, v, v, 0x3c00]
            })
            .collect();
        // SAFETY: tightly packed complete FP16 image; the system shader reads it.
        unsafe {
            gpu.context
                .UpdateSubresource(&source, 0, None, pixels.as_ptr().cast(), 960 * 8, 0);
        }
        let mut renderer = CursorRenderer::new(&gpu)?;
        renderer.draw(
            &gpu,
            &source,
            &target,
            &Default::default(),
            &Default::default(),
            (DXGI_MODE_ROTATION_IDENTITY, 1.0, true),
        )?;
        let mut converter = Converter::with_color(&gpu, (960, 540), (960, 540), 60, color)?;
        let picture = converter.convert(&target)?;
        gpu.read(&mut None, &picture.texture, 540 * 3 / 2, |bytes, stride| {
            for (x, nits) in [(160, 10.0), (480, 100.0), (800, 1000.0)] {
                let offset = 270 * stride + x * 2;
                let code = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) >> 6;
                let expected = 64.0 + 876.0 * latch_proto::color::pq_oetf(nits);
                assert!(
                    (f64::from(code) - expected).abs() < 4.0,
                    "FP16 -> PQ P010: {nits} nits -> {code}, expected {expected}"
                );
            }
        })?;
        let dir = std::env::var("LATCH_HDR_PROBE_DIR").unwrap_or_else(|_| ".".into());
        for mode in ["sdr8", "sdr10", "hdr"] {
            let color = latch_proto::color::ColorDescription::negotiate(
                latch_proto::color::ColorCapabilities {
                    ten_bit: mode != "sdr8",
                    hdr: mode == "hdr",
                    peak_nits: 1000,
                },
                true,
                mode == "hdr",
            );
            let config = VideoConfig {
                color,
                ..config.clone()
            };
            let rgb = gpu.texture(
                960,
                540,
                if color.bit_depth == 10 {
                    DXGI_FORMAT_R10G10B10A2_UNORM
                } else {
                    DXGI_FORMAT_B8G8R8A8_UNORM
                },
                D3D11_BIND_RENDER_TARGET.0 as u32,
                false,
            )?;
            renderer.draw(
                &gpu,
                &source,
                &rgb,
                &Default::default(),
                &Default::default(),
                (DXGI_MODE_ROTATION_IDENTITY, 1.0, color.is_hdr()),
            )?;
            let mut converter = Converter::with_color(&gpu, (960, 540), (960, 540), 60, color)?;
            let picture = converter.convert(&rgb)?;
            for mf in [false, true] {
                let name = if mf { "mf" } else { "amf" };
                let mut stream = color
                    .hdr
                    .map(latch_proto::color::hevc_hdr_sei)
                    .unwrap_or_default();
                let mut times = Vec::new();
                if mf {
                    let mut encoder = super::super::encode::Encoder::new(&config, &gpu)?;
                    let sample = super::super::encode::Encoder::texture_sample(&picture.texture)?;
                    for i in 0..60 {
                        let at = Instant::now();
                        if let Some((bytes, _)) = encoder.encode(&sample, i == 0)? {
                            stream.extend(bytes);
                        }
                        times.push(at.elapsed().as_secs_f64() * 1000.0);
                    }
                } else {
                    let mut encoder = super::super::amf::Encoder::new(&config, &gpu)?;
                    for i in 0..60 {
                        let at = Instant::now();
                        let (bytes, _) = encoder.encode(&picture.texture, i == 0)?;
                        times.push(at.elapsed().as_secs_f64() * 1000.0);
                        stream.extend(bytes);
                    }
                }
                times.sort_by(f64::total_cmp);
                eprintln!(
                "COLOR {mode} {name}: frames=60 bytes={} encode_p50_ms={:.3} encode_p95_ms={:.3}",
                stream.len(),
                times[30],
                times[57]
            );
                std::fs::write(
                    std::path::Path::new(&dir).join(format!("{mode}-{name}.hevc")),
                    stream,
                )?;
            }
        }
        Ok(())
    }
}
