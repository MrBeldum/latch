//! A float Metal sublayer for HDR video. Egui 0.31 fixes its window to BGRA8.
use super::macos_zero_copy::{CVMetalTextureWrapper, MetalTextureCache};
use latch_stream::{video::NativeFrame, Frame};
use metal::{foreign_types::ForeignType, *};
use objc2::{msg_send, runtime::AnyObject};
use objc2_app_kit::NSApplication;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::atomic::{AtomicU32, Ordering},
};

static PEAK: AtomicU32 = AtomicU32::new(0);
pub fn peak_nits() -> u32 {
    PEAK.load(Ordering::Relaxed)
}

const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct V { float4 pos [[position]]; float2 uv; };
vertex V vs(uint i [[vertex_id]]) {
    float2 p = float2(int(i&1)*4-1,int(i>>1)*4-1);
    return {float4(p,0,1),float2((p.x+1)*0.5,1-(p.y+1)*0.5)};
}
fragment float4 fs(V in [[stage_in]], texture2d<float> y [[texture(0)]], texture2d<float> uv [[texture(1)]], constant float4 &params [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float yy = (y.sample(s,in.uv).r*65535.0/64.0-64.0)/876.0;
    // params.z: chroma siting, in chroma texels right of the centred position.
    float2 c = (uv.sample(s,in.uv+float2(params.z/uv.get_width(),0)).rg*65535.0/64.0-512.0)/896.0;
    float headroom = params.x;
    if(params.y == 0) {
        float3 c709 = saturate(float3(yy+1.5748*c.y,yy-0.1873*c.x-0.4681*c.y,yy+1.8556*c.x));
        float3 lin = select(pow((c709+0.055)/1.055,float3(2.4)),c709/12.92,c709 <= 0.04045);
        return float4(dot(lin,float3(0.822593,0.177534,0)),dot(lin,float3(0.0332,0.966784,0)),dot(lin,float3(0.017085,0.072396,0.910302)),1);
    }
    float3 pq = max(float3(yy+1.4746*c.y,yy-0.164553*c.x-0.571353*c.y,yy+1.8814*c.x),0.0);
    float3 p = pow(pq,float3(32.0/2523.0));
    float3 wide = 10000.0*pow(max(p-3424.0/4096.0,0.0)/(2413.0/128.0-(2392.0/128.0)*p),float3(16384.0/2610.0));
    // Linear BT.2020 -> linear Display P3, relative to 203-nit diffuse white.
    float3 rgb = max(float3(dot(wide,float3(1.343578,-0.282180,-0.061398)),dot(wide,float3(-0.065298,1.075788,-0.010490)),dot(wide,float3(0.002822,-0.019598,1.016777)))/203.0,0.0);
    float peak = max(max(rgb.r,rgb.g),rgb.b);
    float knee = headroom*0.75;
    if(peak > knee) rgb *= (knee+(headroom-knee)*(1-exp(-(peak-knee)/(headroom-knee))))/peak;
    return float4(rgb,1);
}
"#;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceExtendedLinearDisplayP3: *const c_void;
    fn CGColorSpaceCreateWithName(name: *const c_void) -> *const c_void;
    fn CGColorSpaceRelease(space: *const c_void);
}

type Pending = (
    CommandBuffer,
    NativeFrame,
    CVMetalTextureWrapper,
    CVMetalTextureWrapper,
);
/// The video layer sits *below* egui's own Metal layer. While it shows, egui's
/// layer is made translucent and the video callback clears its rect to
/// transparent, so CoreAnimation composites controls, stats and toasts over
/// the HDR picture at SDR white instead of the picture being tone-mapped
/// whenever something covers it.
pub(super) struct Edr {
    layer: MetalLayer,
    queue: CommandQueue,
    pipeline: RenderPipelineState,
    cache: MetalTextureCache,
    pending: VecDeque<Pending>,
    /// Tests render into a texture at a given headroom instead of a window.
    #[cfg(test)]
    offscreen: Option<(Texture, f32)>,
}
impl Edr {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let screen: *mut AnyObject = msg_send![objc2::class!(NSScreen), mainScreen];
            if !screen.is_null() {
                let potential: f64 = msg_send![
                    screen,
                    maximumPotentialExtendedDynamicRangeColorComponentValue
                ];
                PEAK.store(
                    if potential > 1.0 {
                        (potential * 203.0) as u32
                    } else {
                        0
                    },
                    Ordering::Relaxed,
                );
            }
        }
        let device = Device::system_default().ok_or("no Metal device")?;
        let pipeline = pipeline(&device)?;
        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::RGBA16Float);
        layer.set_wants_extended_dynamic_range_content(true);
        layer.set_display_sync_enabled(false);
        layer.set_maximum_drawable_count(2);
        layer.set_opaque(true);
        // SAFETY: CALayer and CGColorSpace calls on the window's main thread.
        unsafe {
            let object = &*(layer.as_ptr() as *const AnyObject);
            let space = CGColorSpaceCreateWithName(kCGColorSpaceExtendedLinearDisplayP3);
            if space.is_null() {
                return Err("no EDR colour space".into());
            }
            let _: () = msg_send![object,setColorspace: space];
            CGColorSpaceRelease(space);
            let _: () = msg_send![object,setHidden: true];
        }
        let cache = MetalTextureCache::new(device.as_ptr().cast())
            .map_err(|e| format!("Metal texture cache: {e}"))?;
        Ok(Self {
            layer,
            queue: device.new_command_queue(),
            pipeline,
            cache,
            pending: VecDeque::new(),
            #[cfg(test)]
            offscreen: None,
        })
    }
    /// A presenter that draws into an RGBA16Float texture, for tests.
    #[cfg(test)]
    pub fn offscreen(width: u64, height: u64, headroom: f32) -> Result<Self, String> {
        let device = Device::system_default().ok_or("no Metal device")?;
        let desc = TextureDescriptor::new();
        desc.set_pixel_format(MTLPixelFormat::RGBA16Float);
        desc.set_width(width);
        desc.set_height(height);
        desc.set_storage_mode(MTLStorageMode::Shared);
        desc.set_usage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
        Ok(Self {
            layer: MetalLayer::new(),
            queue: device.new_command_queue(),
            pipeline: pipeline(&device)?,
            cache: MetalTextureCache::new(device.as_ptr().cast())
                .map_err(|e| format!("Metal texture cache: {e}"))?,
            pending: VecDeque::new(),
            offscreen: Some((device.new_texture(&desc), headroom)),
        })
    }
    /// The offscreen picture as linear extended-range RGBA.
    #[cfg(test)]
    pub fn read(&mut self) -> Vec<[f32; 4]> {
        for p in self.pending.drain(..) {
            p.0.wait_until_completed();
        }
        let (texture, _) = self.offscreen.as_ref().expect("offscreen presenter");
        let (w, h) = (texture.width(), texture.height());
        let mut half = vec![0u16; (w * h * 4) as usize];
        texture.get_bytes(
            half.as_mut_ptr().cast(),
            w * 8,
            MTLRegion::new_2d(0, 0, w, h),
            0,
        );
        half.chunks(4)
            .map(|p| [0, 1, 2, 3].map(|i| f16_to_f32(p[i])))
            .collect()
    }
    fn set_visible(&self, visible: bool) {
        // SAFETY: CALayer property changes on the window's main thread; the
        // root's sublayers are retained by the root while we look at them.
        unsafe {
            let object = &*(self.layer.as_ptr() as *const AnyObject);
            let _: () = msg_send![object,setHidden: !visible];
            let root: *mut AnyObject = msg_send![object, superlayer];
            if root.is_null() {
                return;
            }
            // wgpu configures egui's layer opaque; egui's pixels are
            // premultiplied, so translucency only matters inside the hole.
            let sublayers: *mut AnyObject = msg_send![root, sublayers];
            let count: usize = if sublayers.is_null() {
                0
            } else {
                msg_send![sublayers, count]
            };
            for i in 0..count {
                let layer: *mut AnyObject = msg_send![sublayers, objectAtIndex: i];
                let metal: bool = msg_send![layer, isKindOfClass: objc2::class!(CAMetalLayer)];
                if metal && !std::ptr::eq(layer, object) {
                    let _: () = msg_send![layer,setOpaque: !visible];
                }
            }
        }
    }
    pub fn show(&self) {
        self.set_visible(true);
    }
    pub fn hide(&self) {
        self.set_visible(false);
    }
    pub fn draw(
        &mut self,
        frame: &Frame,
        rect: egui::Rect,
        pixels_per_point: f32,
    ) -> Result<(), String> {
        while self.pending.front().is_some_and(|p| {
            matches!(
                p.0.status(),
                MTLCommandBufferStatus::Completed | MTLCommandBufferStatus::Error
            )
        }) {
            self.pending.pop_front();
        }
        #[cfg(test)]
        if let Some((texture, headroom)) = self.offscreen.clone() {
            let _ = (rect, pixels_per_point);
            let cmd = self.encode(frame, &texture, headroom)?;
            cmd.commit();
            cmd.wait_until_completed();
            return Ok(());
        }
        let mtm = MainThreadMarker::new().ok_or("EDR requires main thread")?;
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: borrowed AppKit objects remain owned by the main-thread window.
        let (headroom, bounds) = unsafe {
            let app = &*(&*app as *const NSApplication as *const AnyObject);
            let window: *mut AnyObject = msg_send![app, mainWindow];
            if window.is_null() {
                return Err("no HDR window".into());
            }
            let view: *mut AnyObject = msg_send![window, contentView];
            let root: *mut AnyObject = msg_send![view, layer];
            let screen: *mut AnyObject = msg_send![window, screen];
            if root.is_null() || screen.is_null() {
                return Err("no window layer/screen".into());
            }
            let potential: f64 = msg_send![
                screen,
                maximumPotentialExtendedDynamicRangeColorComponentValue
            ];
            let headroom: f64 = msg_send![screen, maximumExtendedDynamicRangeColorComponentValue];
            PEAK.store(
                if potential > 1.0 {
                    (potential * 203.0) as u32
                } else {
                    0
                },
                Ordering::Relaxed,
            );
            let object = &*(self.layer.as_ptr() as *const AnyObject);
            let parent: *mut AnyObject = msg_send![object, superlayer];
            if parent != root {
                // Below wgpu's sublayer, which egui draws the controls into.
                let _: () = msg_send![root,insertSublayer: object,atIndex: 0u32];
            }
            let bounds: NSRect = msg_send![view, bounds];
            let flipped: bool = msg_send![root, isGeometryFlipped];
            let y = if flipped {
                f64::from(rect.top())
            } else {
                bounds.size.height - f64::from(rect.bottom())
            };
            let area = NSRect::new(
                NSPoint::new(f64::from(rect.left()), y),
                NSSize::new(f64::from(rect.width()), f64::from(rect.height())),
            );
            let _: () = msg_send![object,setFrame: area];
            (headroom.max(1.0) as f32, area)
        };
        self.show();
        self.layer.set_contents_scale(f64::from(pixels_per_point));
        let mut size = self.layer.drawable_size();
        size.width = bounds.size.width * f64::from(pixels_per_point);
        size.height = bounds.size.height * f64::from(pixels_per_point);
        self.layer.set_drawable_size(size);
        if self.pending.len() >= 2 {
            return Ok(());
        }
        let drawable = self
            .layer
            .next_drawable()
            .ok_or("no EDR drawable")?
            .to_owned();
        let cmd = self.encode(frame, drawable.texture(), headroom)?;
        cmd.present_drawable(&drawable);
        cmd.commit();
        Ok(())
    }
    /// Draw `frame` into `target`; the returned buffer still has to be committed.
    fn encode(
        &mut self,
        frame: &Frame,
        target: &TextureRef,
        headroom: f32,
    ) -> Result<CommandBuffer, String> {
        let native = frame.native.as_ref().ok_or("HDR frame has no IOSurface")?;
        let (y, cy) = self
            .cache
            .create_texture(
                native.pixel_buffer(),
                MTLPixelFormat::R16Unorm as usize,
                frame.width as usize,
                frame.height as usize,
                0,
            )
            .map_err(|e| format!("HDR Y plane: {e}"))?;
        let (uv, cu) = self
            .cache
            .create_texture(
                native.pixel_buffer(),
                MTLPixelFormat::RG16Unorm as usize,
                frame.width.div_ceil(2) as usize,
                frame.height.div_ceil(2) as usize,
                1,
            )
            .map_err(|e| format!("HDR UV plane: {e}"))?;
        let params = [
            headroom,
            if frame.color.is_hdr() { 1.0 } else { 0.0 },
            frame.color.chroma.texel_offset(),
            0.0,
        ];
        let cmd = render(&self.queue, &self.pipeline, &y, &uv, target, params);
        self.pending
            .push_back((cmd.clone(), native.clone(), cy, cu));
        Ok(cmd)
    }
}
fn pipeline(device: &Device) -> Result<RenderPipelineState, String> {
    let library = device.new_library_with_source(SHADER, &CompileOptions::new())?;
    let desc = RenderPipelineDescriptor::new();
    let vertex = library.get_function("vs", None)?;
    desc.set_vertex_function(Some(&vertex));
    let fragment = library.get_function("fs", None)?;
    desc.set_fragment_function(Some(&fragment));
    desc.color_attachments()
        .object_at(0)
        .unwrap()
        .set_pixel_format(MTLPixelFormat::RGBA16Float);
    device.new_render_pipeline_state(&desc)
}
fn render(
    queue: &CommandQueueRef,
    pipeline: &RenderPipelineStateRef,
    y: &TextureRef,
    uv: &TextureRef,
    target: &TextureRef,
    params: [f32; 4],
) -> CommandBuffer {
    let pass = RenderPassDescriptor::new();
    let attachment = pass.color_attachments().object_at(0).unwrap();
    attachment.set_texture(Some(target));
    attachment.set_load_action(MTLLoadAction::DontCare);
    attachment.set_store_action(MTLStoreAction::Store);
    let cmd = queue.new_command_buffer().to_owned();
    let enc = cmd.new_render_command_encoder(pass);
    enc.set_render_pipeline_state(pipeline);
    enc.set_fragment_texture(0, Some(y));
    enc.set_fragment_texture(1, Some(uv));
    enc.set_fragment_bytes(0, 16, params.as_ptr().cast());
    enc.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
    enc.end_encoding();
    cmd
}
#[cfg(test)]
fn f16_to_f32(half: u16) -> f32 {
    let sign = if half & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((half >> 10) & 31);
    let mantissa = f32::from(half & 1023) / 1024.0;
    sign * if exponent == 0 {
        mantissa * 2f32.powi(-14)
    } else {
        (1.0 + mantissa) * 2f32.powi(exponent - 15)
    }
}
impl Drop for Edr {
    fn drop(&mut self) {
        self.hide();
        // Teardown only: retain IOSurfaces until their GPU readers finish.
        for p in &self.pending {
            p.0.wait_until_completed();
        }
        unsafe {
            let _: () = msg_send![
                &*(self.layer.as_ptr() as *const AnyObject),
                removeFromSuperlayer
            ];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Render ten-bit planes (codes, not P010 words) through the shader.
    fn shade(
        (w, h): (u64, u64),
        luma: &[u16],
        chroma: &[[u16; 2]],
        params: [f32; 4],
    ) -> Vec<[f32; 4]> {
        let device = Device::system_default().unwrap();
        let pipeline = pipeline(&device).unwrap();
        let texture = |format, w, h| {
            let desc = TextureDescriptor::new();
            desc.set_pixel_format(format);
            desc.set_width(w);
            desc.set_height(h);
            desc.set_storage_mode(MTLStorageMode::Shared);
            desc.set_usage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
            device.new_texture(&desc)
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y = texture(MTLPixelFormat::R16Unorm, w, h);
        let uv = texture(MTLPixelFormat::RG16Unorm, cw, ch);
        let words: Vec<u16> = luma.iter().map(|c| c << 6).collect();
        y.replace_region(
            MTLRegion::new_2d(0, 0, w, h),
            0,
            words.as_ptr().cast(),
            w * 2,
        );
        let words: Vec<u16> = chroma.iter().flat_map(|c| [c[0] << 6, c[1] << 6]).collect();
        uv.replace_region(
            MTLRegion::new_2d(0, 0, cw, ch),
            0,
            words.as_ptr().cast(),
            cw * 4,
        );
        let output = texture(MTLPixelFormat::RGBA16Float, w, h);
        let queue = device.new_command_queue();
        let cmd = render(&queue, &pipeline, &y, &uv, &output, params);
        cmd.commit();
        cmd.wait_until_completed();
        assert_eq!(cmd.status(), MTLCommandBufferStatus::Completed);
        let mut half = vec![0u16; (w * h * 4) as usize];
        output.get_bytes(
            half.as_mut_ptr().cast(),
            w * 8,
            MTLRegion::new_2d(0, 0, w, h),
            0,
        );
        half.chunks(4)
            .map(|p| [0, 1, 2, 3].map(|i| f16_to_f32(p[i])))
            .collect()
    }

    #[test]
    #[ignore = "requires Metal"]
    fn native_edr_shader_preserves_hdr_headroom() {
        let code = (64.0 + 876.0 * latch_proto::color::pq_oetf(1000.0)).round() as u16;
        let out = shade((2, 2), &[code; 4], &[[512, 512]], [4.0, 1.0, 0.25, 0.0]);
        // 1000 nits through a four-times-SDR shoulder is 3.85 EDR units.
        assert!(
            out.iter().all(|p| (p[0] - 3.85).abs() < 0.05),
            "native EDR output {out:?}"
        );
    }

    /// A coloured edge in left-sited chroma: chroma sample k sits on luma
    /// column 2k, so the luma column there takes exactly the new colour and
    /// the one before it the midpoint. Centred sampling puts it a quarter of
    /// a chroma texel (half a pixel) to the left.
    #[test]
    #[ignore = "requires Metal"]
    fn native_edr_shader_honours_left_chroma_siting() {
        let (w, h) = (32u64, 4u64);
        let (a, b) = ([400, 650], [650, 400]);
        let chroma: Vec<[u16; 2]> = (0..w / 2 * h / 2)
            .map(|i| if i % (w / 2) < 8 { a } else { b })
            .collect();
        let luma = vec![500; (w * h) as usize];
        for hdr in [0.0, 1.0] {
            let left = shade((w, h), &luma, &chroma, [4.0, hdr, 0.25, 0.0]);
            let centred = shade((w, h), &luma, &chroma, [4.0, hdr, 0.0, 0.0]);
            let flat = |c| shade((2, 2), &[500; 4], &[c], [4.0, hdr, 0.25, 0.0])[0];
            let (pa, pb) = (flat(a), flat(b));
            let near =
                |p: [f32; 4], q: [f32; 4], tol: f32| (0..3).all(|i| (p[i] - q[i]).abs() <= tol);
            let row = w as usize; // second luma row, first chroma row
            assert!(
                near(left[row + 16], pb, 2e-3),
                "{:?} vs {pb:?}",
                left[row + 16]
            );
            assert!(
                near(left[row + 14], pa, 2e-3),
                "{:?} vs {pa:?}",
                left[row + 14]
            );
            let mid = shade((2, 2), &[500; 4], &[[525, 525]], [4.0, hdr, 0.25, 0.0])[0];
            assert!(
                near(left[row + 15], mid, 2e-3),
                "{:?} vs {mid:?}",
                left[row + 15]
            );
            // The test can tell the two sitings apart.
            assert!(
                !near(centred[row + 16], pb, 2e-2),
                "centred {:?}",
                centred[row + 16]
            );
        }
    }
}
