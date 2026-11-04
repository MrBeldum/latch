//! Native HDR10 child swapchain. Egui's SDR surface stays on its existing path.
use anyhow::{Context, Result};
use latch_stream::Frame;
use std::{
    ffi::c_void,
    sync::atomic::{AtomicU32, Ordering},
};
use windows::{
    core::{s, w, Interface},
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::{Fxc::*, *},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        UI::WindowsAndMessaging::*,
    },
};
static PEAK: AtomicU32 = AtomicU32::new(0);
pub fn peak_nits() -> u32 {
    PEAK.load(Ordering::Relaxed)
}
const SHADER: &str = r#"
Texture2D<float> y:register(t0); Texture2D<float2> uv:register(t1); SamplerState samp:register(s0);
struct V { float4 p:SV_Position; float2 uv:TEXCOORD0; };
V vs(uint id:SV_VertexID) { V v; float2 p=float2(int(id&1)*4-1,int(id>>1)*4-1); v.p=float4(p,0,1); v.uv=float2((p.x+1)*0.5,1-(p.y+1)*0.5); return v; }
float4 ps(V v):SV_Target {
    float yy=(y.Sample(samp,v.uv)*65535.0/64.0-64.0)/876.0;
    float2 c=(uv.Sample(samp,v.uv)*65535.0/64.0-512.0)/896.0;
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
    planes: Option<(
        u32,
        u32,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11ShaderResourceView,
        ID3D11ShaderResourceView,
    )>,
    size: (u32, u32),
}
// All native calls run on egui's UI/render thread; callback resources require
// Send/Sync but never access this context concurrently.
unsafe impl Send for Hdr10 {}
unsafe impl Sync for Hdr10 {}
impl Hdr10 {
    pub fn new() -> Result<Self> {
        unsafe {
            let parent = GetActiveWindow();
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
                anyhow::ensure!(hdr, "viewer display is in SDR mode");
                anyhow::ensure!(
                    swap.CheckColorSpaceSupport(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)?
                        & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32
                        != 0,
                    "HDR10 swapchain unsupported"
                );
                swap.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)?;
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
                Ok(Self {
                    device,
                    context,
                    swap,
                    window,
                    vertex: vertex.context("no HDR VS")?,
                    pixel: pixel.context("no HDR PS")?,
                    sampler: sampler.context("no HDR sampler")?,
                    planes: None,
                    size: (1, 1),
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
            anyhow::ensure!(hdr, "viewer display switched to SDR");
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
                self.swap
                    .SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)?;
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
                    std::mem::size_of_val(&meta) as u32,
                    Some((&meta as *const DXGI_HDR_METADATA_HDR10).cast::<c_void>()),
                )?;
            }
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
impl Drop for Hdr10 {
    fn drop(&mut self) {
        unsafe {
            self.context.ClearState();
            let _ = DestroyWindow(self.window);
        }
    }
}
