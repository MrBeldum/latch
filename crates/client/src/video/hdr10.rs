//! Native HDR10 child swapchain. Egui's SDR surface stays on its existing path.
//! A child window covers its parent's surface, so the child is cut around
//! whatever egui draws over the picture (toolbar, stats, toasts, dialogs):
//! there egui shows its controls over its own copy of the picture, and
//! everywhere else the picture stays HDR whatever the controls do.
use anyhow::{Context, Result};
use latch_stream::Frame;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::{
    core::{s, w, Interface},
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::{Fxc::*, *},
            Direct3D11::*,
            Dxgi::{Common::*, *},
            Gdi::{CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, HRGN, RGN_DIFF},
        },
        UI::WindowsAndMessaging::*,
    },
};
static PEAK: AtomicU32 = AtomicU32::new(0);
pub fn peak_nits() -> u32 {
    PEAK.load(Ordering::Relaxed)
}
const SHADER: &str = r#"
cbuffer C:register(b0) { uint hdr; float chroma; uint2 pad; };
Texture2D<float> y:register(t0); Texture2D<float2> uv:register(t1); SamplerState samp:register(s0);
struct V { float4 p:SV_Position; float2 uv:TEXCOORD0; };
// Clockwise, D3D's front face: the default rasterizer culls the other.
V vs(uint id:SV_VertexID) { V v; float2 p=float2(int(id>>1)*4-1,int(id&1)*4-1); v.p=float4(p,0,1); v.uv=float2((p.x+1)*0.5,1-(p.y+1)*0.5); return v; }
float4 ps(V v):SV_Target {
    float yy=(y.Sample(samp,v.uv)*65535.0/64.0-64.0)/876.0;
    // chroma: siting, in chroma texels right of the centred position.
    float cw, ch; uv.GetDimensions(cw, ch);
    float2 c=(uv.Sample(samp,v.uv+float2(chroma/cw,0))*65535.0/64.0-512.0)/896.0;
    if(hdr == 0) return float4(saturate(float3(yy+1.5748*c.y,yy-0.1873*c.x-0.4681*c.y,yy+1.8556*c.x)),1);
    return float4(saturate(float3(yy+1.4746*c.y,yy-0.164553*c.x-0.571353*c.y,yy+1.8814*c.x)),1);
}
"#;
fn shader(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Result<ID3DBlob> {
    let (mut code, mut errors) = (None, None);
    unsafe {
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
        )?;
    }
    code.context("HDR shader compiler returned no code")
}
pub(super) struct Hdr10 {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap: IDXGISwapChain3,
    window: HWND,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    planes: Option<(
        u32,
        u32,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11ShaderResourceView,
        ID3D11ShaderResourceView,
    )>,
    size: (u32, u32),
    /// The window size and holes of the current window region.
    clipped: ((u32, u32), Vec<RECT>),
}
// All native calls run on egui's UI/render thread; callback resources require
// Send/Sync but never access this context concurrently.
unsafe impl Send for Hdr10 {}
unsafe impl Sync for Hdr10 {}
impl Hdr10 {
    pub fn new(parent: HWND) -> Result<Self> {
        unsafe {
            anyhow::ensure!(!parent.is_invalid(), "no active viewer window");
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("no HDR device")?;
            let context = context.context("no HDR context")?;
            let dxgi: IDXGIDevice = device.cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let window = CreateWindowExW(
                WS_EX_NOPARENTNOTIFY | WS_EX_TRANSPARENT,
                w!("STATIC"),
                None,
                WS_CHILD | WS_DISABLED,
                0,
                0,
                1,
                1,
                parent,
                None,
                None,
                None,
            )?;
            let attempt = (|| -> Result<Self> {
                let swap: IDXGISwapChain3 = factory
                    .CreateSwapChainForHwnd(
                        &device,
                        window,
                        &DXGI_SWAP_CHAIN_DESC1 {
                            Width: 1,
                            Height: 1,
                            Format: DXGI_FORMAT_R10G10B10A2_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                            BufferCount: 2,
                            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                            ..Default::default()
                        },
                        None,
                        None,
                    )?
                    .cast()?;
                let _ = factory.MakeWindowAssociation(parent, DXGI_MWA_NO_ALT_ENTER);
                let output: IDXGIOutput6 = swap.GetContainingOutput()?.cast()?;
                let d = output.GetDesc1()?;
                let hdr = d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                PEAK.store(
                    if hdr {
                        d.MaxLuminance.max(1.0) as u32
                    } else {
                        0
                    },
                    Ordering::Relaxed,
                );
                let space = if hdr {
                    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                } else {
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
                };
                anyhow::ensure!(
                    swap.CheckColorSpaceSupport(space)?
                        & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32
                        != 0,
                    "HDR10 swapchain unsupported"
                );
                swap.SetColorSpace1(space)?;
                let mut vertex = None;
                let mut pixel = None;
                let mut sampler = None;
                let vs = shader(s!("vs"), s!("vs_5_0"))?;
                let ps = shader(s!("ps"), s!("ps_5_0"))?;
                device.CreateVertexShader(
                    std::slice::from_raw_parts(vs.GetBufferPointer().cast(), vs.GetBufferSize()),
                    None,
                    Some(&mut vertex),
                )?;
                device.CreatePixelShader(
                    std::slice::from_raw_parts(ps.GetBufferPointer().cast(), ps.GetBufferSize()),
                    None,
                    Some(&mut pixel),
                )?;
                device.CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    },
                    Some(&mut sampler),
                )?;
                let mut constants = None;
                device.CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: 16,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut constants),
                )?;
                Ok(Self {
                    constants: constants.context("no HDR constants")?,
                    device,
                    context,
                    swap,
                    window,
                    vertex: vertex.context("no HDR VS")?,
                    pixel: pixel.context("no HDR PS")?,
                    sampler: sampler.context("no HDR sampler")?,
                    planes: None,
                    size: (1, 1),
                    clipped: ((1, 1), Vec::new()),
                })
            })();
            if attempt.is_err() {
                let _ = DestroyWindow(window);
            }
            attempt
        }
    }
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.window, SW_HIDE);
        }
    }
    pub fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.window, SW_SHOWNA);
        }
    }
    /// Cut `holes` (points; parts of `rect`, the video) out of the window.
    pub fn clip(&mut self, holes: &[egui::Rect], rect: egui::Rect, scale: f32) -> Result<()> {
        let size = (
            (rect.width() * scale).round().max(1.0) as u32,
            (rect.height() * scale).round().max(1.0) as u32,
        );
        let rects = holes_in(holes, rect, scale);
        if self.clipped.0 == size && self.clipped.1 == rects {
            return Ok(());
        }
        // SAFETY: regions are created here; SetWindowRgn owns one on success
        // and the others are deleted. The window is this presenter's child.
        unsafe {
            if rects.is_empty() {
                anyhow::ensure!(
                    SetWindowRgn(self.window, HRGN::default(), true) != 0,
                    "SetWindowRgn"
                );
            } else {
                let region = CreateRectRgn(0, 0, size.0 as i32, size.1 as i32);
                for r in &rects {
                    let hole = CreateRectRgn(r.left, r.top, r.right, r.bottom);
                    CombineRgn(region, region, hole, RGN_DIFF);
                    let _ = DeleteObject(hole);
                }
                if SetWindowRgn(self.window, region, true) == 0 {
                    let _ = DeleteObject(region);
                    anyhow::bail!("SetWindowRgn");
                }
            }
        }
        self.clipped = (size, rects);
        Ok(())
    }
    fn texture(
        &self,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
    ) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        unsafe {
            let mut texture = None;
            let mut view = None;
            self.device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut texture),
            )?;
            let texture = texture.context("no HDR upload texture")?;
            self.device
                .CreateShaderResourceView(&texture, None, Some(&mut view))?;
            Ok((texture, view.context("no HDR upload view")?))
        }
    }
    pub fn draw(&mut self, frame: &Frame, rect: egui::Rect, scale: f32) -> Result<()> {
        unsafe {
            let d = self
                .swap
                .GetContainingOutput()?
                .cast::<IDXGIOutput6>()?
                .GetDesc1()?;
            let hdr = d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
            PEAK.store(
                if hdr {
                    d.MaxLuminance.max(1.0) as u32
                } else {
                    0
                },
                Ordering::Relaxed,
            );
            anyhow::ensure!(
                hdr || !frame.color.is_hdr(),
                "viewer display switched to SDR"
            );
            let space = if frame.color.is_hdr() {
                DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
            } else {
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
            };
            let size = (
                (rect.width() * scale).round().max(1.0) as u32,
                (rect.height() * scale).round().max(1.0) as u32,
            );
            SetWindowPos(
                self.window,
                None,
                (rect.left() * scale).round() as i32,
                (rect.top() * scale).round() as i32,
                size.0 as i32,
                size.1 as i32,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )?;
            if self.size != size {
                self.context.OMSetRenderTargets(None, None);
                self.swap.ResizeBuffers(
                    2,
                    size.0,
                    size.1,
                    DXGI_FORMAT_R10G10B10A2_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )?;
                self.swap.SetColorSpace1(space)?;
                self.size = size;
            }
            if self
                .planes
                .as_ref()
                .is_none_or(|p| (p.0, p.1) != (frame.width, frame.height))
            {
                let (y, yv) = self.texture(frame.width, frame.height, DXGI_FORMAT_R16_UNORM)?;
                let (uv, uvv) = self.texture(
                    frame.width.div_ceil(2),
                    frame.height.div_ceil(2),
                    DXGI_FORMAT_R16G16_UNORM,
                )?;
                self.planes = Some((frame.width, frame.height, y, uv, yv, uvv));
            }
            let p = self.planes.as_ref().unwrap();
            frame.with_planes(|y, ys, uv, uvs| {
                self.context
                    .UpdateSubresource(&p.2, 0, None, y.as_ptr().cast(), ys as u32, 0);
                self.context
                    .UpdateSubresource(&p.3, 0, None, uv.as_ptr().cast(), uvs as u32, 0);
            });
            if let Some(m) = frame.color.hdr {
                let meta = DXGI_HDR_METADATA_HDR10 {
                    RedPrimary: m.primaries[0],
                    GreenPrimary: m.primaries[1],
                    BluePrimary: m.primaries[2],
                    WhitePoint: m.white_point,
                    MaxMasteringLuminance: m.max_luminance / 10000,
                    MinMasteringLuminance: m.min_luminance,
                    MaxContentLightLevel: m.max_cll,
                    MaxFrameAverageLightLevel: m.max_fall,
                };
                self.swap.cast::<IDXGISwapChain4>()?.SetHDRMetaData(
                    DXGI_HDR_METADATA_TYPE_HDR10,
                    Some(std::slice::from_raw_parts(
                        (&meta as *const DXGI_HDR_METADATA_HDR10).cast::<u8>(),
                        std::mem::size_of_val(&meta),
                    )),
                )?;
            }
            self.swap.SetColorSpace1(space)?;
            let constants = [
                u32::from(frame.color.is_hdr()),
                frame.color.chroma.texel_offset().to_bits(),
                0,
                0,
            ];
            self.context.UpdateSubresource(
                &self.constants,
                0,
                None,
                constants.as_ptr().cast(),
                0,
                0,
            );
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            let buffer: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            let mut target = None;
            self.device
                .CreateRenderTargetView(&buffer, None, Some(&mut target))?;
            self.context.OMSetRenderTargets(Some(&[target]), None);
            self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: size.0 as f32,
                Height: size.1 as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShader(&self.pixel, None);
            self.context
                .PSSetShaderResources(0, Some(&[Some(p.4.clone()), Some(p.5.clone())]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.Draw(3, 0);
            self.context.OMSetRenderTargets(None, None);
            self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
            self.show();
            Ok(())
        }
    }
}
/// Overlay rects in window pixels, rounded outward so no control is covered.
fn holes_in(holes: &[egui::Rect], rect: egui::Rect, scale: f32) -> Vec<RECT> {
    holes
        .iter()
        .map(|h| RECT {
            left: ((h.left() - rect.left()) * scale).floor() as i32,
            top: ((h.top() - rect.top()) * scale).floor() as i32,
            right: ((h.right() - rect.left()) * scale).ceil() as i32,
            bottom: ((h.bottom() - rect.top()) * scale).ceil() as i32,
        })
        .collect()
}
impl Drop for Hdr10 {
    fn drop(&mut self) {
        unsafe {
            self.context.ClearState();
            let _ = DestroyWindow(self.window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hdr10_shaders_compile_with_the_system_compiler() {
        shader(s!("vs"), s!("vs_5_0")).unwrap();
        shader(s!("ps"), s!("ps_5_0")).unwrap();
    }

    /// Render ten-bit planes (codes) through the presenter's shaders on WARP.
    fn shade(
        (w, h): (u32, u32),
        luma: &[u16],
        chroma: &[[u16; 2]],
        constants: [u32; 4],
    ) -> Result<Vec<[f32; 4]>> {
        unsafe {
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.context("WARP device")?;
            let context: ID3D11DeviceContext = context.context("WARP context")?;
            let texture = |w, h, format, bind: u32, usage, cpu: u32| -> Result<ID3D11Texture2D> {
                let mut t = None;
                device.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: format,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: usage,
                        BindFlags: bind,
                        CPUAccessFlags: cpu,
                        ..Default::default()
                    },
                    None,
                    Some(&mut t),
                )?;
                t.context("texture")
            };
            let srv = D3D11_BIND_SHADER_RESOURCE.0 as u32;
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let y = texture(w, h, DXGI_FORMAT_R16_UNORM, srv, D3D11_USAGE_DEFAULT, 0)?;
            let uv = texture(
                cw,
                ch,
                DXGI_FORMAT_R16G16_UNORM,
                srv,
                D3D11_USAGE_DEFAULT,
                0,
            )?;
            let words: Vec<u16> = luma.iter().map(|c| c << 6).collect();
            context.UpdateSubresource(&y, 0, None, words.as_ptr().cast(), w * 2, 0);
            let words: Vec<u16> = chroma.iter().flat_map(|c| [c[0] << 6, c[1] << 6]).collect();
            context.UpdateSubresource(&uv, 0, None, words.as_ptr().cast(), cw * 4, 0);
            let target = texture(
                w,
                h,
                DXGI_FORMAT_R32G32B32A32_FLOAT,
                D3D11_BIND_RENDER_TARGET.0 as u32,
                D3D11_USAGE_DEFAULT,
                0,
            )?;
            let staging = texture(
                w,
                h,
                DXGI_FORMAT_R32G32B32A32_FLOAT,
                0,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_READ.0 as u32,
            )?;
            let (mut yv, mut uvv, mut rtv) = (None, None, None);
            device.CreateShaderResourceView(&y, None, Some(&mut yv))?;
            device.CreateShaderResourceView(&uv, None, Some(&mut uvv))?;
            device.CreateRenderTargetView(&target, None, Some(&mut rtv))?;
            let vs = shader(s!("vs"), s!("vs_5_0"))?;
            let ps = shader(s!("ps"), s!("ps_5_0"))?;
            let (mut vertex, mut pixel, mut sampler, mut buffer) = (None, None, None, None);
            device.CreateVertexShader(
                std::slice::from_raw_parts(vs.GetBufferPointer().cast(), vs.GetBufferSize()),
                None,
                Some(&mut vertex),
            )?;
            device.CreatePixelShader(
                std::slice::from_raw_parts(ps.GetBufferPointer().cast(), ps.GetBufferSize()),
                None,
                Some(&mut pixel),
            )?;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                Some(&D3D11_SUBRESOURCE_DATA {
                    pSysMem: constants.as_ptr().cast(),
                    ..Default::default()
                }),
                Some(&mut buffer),
            )?;
            context.OMSetRenderTargets(Some(&[rtv]), None);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: w as f32,
                Height: h as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(vertex.as_ref(), None);
            context.PSSetShader(pixel.as_ref(), None);
            context.PSSetConstantBuffers(0, Some(&[buffer]));
            context.PSSetShaderResources(0, Some(&[yv, uvv]));
            context.PSSetSamplers(0, Some(&[sampler]));
            context.Draw(3, 0);
            context.CopyResource(&staging, &target);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let mut out = Vec::new();
            for row in 0..h as usize {
                let line = mapped
                    .pData
                    .cast::<u8>()
                    .add(row * mapped.RowPitch as usize);
                let line = std::slice::from_raw_parts(line.cast::<[f32; 4]>(), w as usize);
                out.extend_from_slice(line);
            }
            context.Unmap(&staging, 0);
            Ok(out)
        }
    }

    /// As the Mac EDR test: left-sited chroma sample k sits on luma column
    /// 2k, so that column takes exactly the new colour.
    #[test]
    fn hdr10_shader_honours_left_chroma_siting() -> Result<()> {
        let (w, h) = (32u32, 4u32);
        let (a, b) = ([400, 650], [650, 400]);
        let chroma: Vec<[u16; 2]> = (0..w / 2 * h / 2)
            .map(|i| if i % (w / 2) < 8 { a } else { b })
            .collect();
        let luma = vec![500; (w * h) as usize];
        let left = 0.25f32.to_bits();
        for hdr in [0, 1] {
            let sited = shade((w, h), &luma, &chroma, [hdr, left, 0, 0])?;
            let centred = shade((w, h), &luma, &chroma, [hdr, 0, 0, 0])?;
            let flat = |c| shade((2, 2), &[500; 4], &[c], [hdr, left, 0, 0]).map(|p| p[0]);
            let (pa, pb, mid) = (flat(a)?, flat(b)?, flat([525, 525])?);
            let near =
                |p: [f32; 4], q: [f32; 4], tol: f32| (0..3).all(|i| (p[i] - q[i]).abs() <= tol);
            let row = w as usize;
            assert!(
                near(sited[row + 16], pb, 2e-3),
                "{:?} vs {pb:?}",
                sited[row + 16]
            );
            assert!(
                near(sited[row + 14], pa, 2e-3),
                "{:?} vs {pa:?}",
                sited[row + 14]
            );
            assert!(
                near(sited[row + 15], mid, 2e-3),
                "{:?} vs {mid:?}",
                sited[row + 15]
            );
            assert!(
                !near(centred[row + 16], pb, 2e-2),
                "centred {:?}",
                centred[row + 16]
            );
        }
        Ok(())
    }
}
