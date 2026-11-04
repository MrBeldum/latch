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
fragment float4 fs(V in [[stage_in]], texture2d<float> y [[texture(0)]], texture2d<float> uv [[texture(1)]], constant float &headroom [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float yy = (y.sample(s,in.uv).r*65535.0/64.0-64.0)/876.0;
    float2 c = (uv.sample(s,in.uv).rg*65535.0/64.0-512.0)/896.0;
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
pub(super) struct Edr {
    layer: MetalLayer,
    queue: CommandQueue,
    pipeline: RenderPipelineState,
    cache: MetalTextureCache,
    pending: VecDeque<Pending>,
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
        let pipeline = device.new_render_pipeline_state(&desc)?;
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
        })
    }
    pub fn show(&self) {
        unsafe {
            let _: () = msg_send![&*(self.layer.as_ptr() as *const AnyObject),setHidden: false];
        }
    }
    pub fn hide(&self) {
        unsafe {
            let _: () = msg_send![&*(self.layer.as_ptr() as *const AnyObject),setHidden: true];
        }
    }
    pub fn draw(
        &mut self,
        frame: &Frame,
        rect: egui::Rect,
        pixels_per_point: f32,
    ) -> Result<(), String> {
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
                let _: () = msg_send![root,addSublayer: object];
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
            let _: () = msg_send![object,setHidden: false];
            (headroom.max(1.0) as f32, area)
        };
        self.layer.set_contents_scale(f64::from(pixels_per_point));
        let mut size = self.layer.drawable_size();
        size.width = bounds.size.width * f64::from(pixels_per_point);
        size.height = bounds.size.height * f64::from(pixels_per_point);
        self.layer.set_drawable_size(size);
        while self.pending.front().is_some_and(|p| {
            matches!(
                p.0.status(),
                MTLCommandBufferStatus::Completed | MTLCommandBufferStatus::Error
            )
        }) {
            self.pending.pop_front();
        }
        if self.pending.len() >= 2 {
            return Ok(());
        }
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
        let drawable = self.layer.next_drawable().ok_or("no EDR drawable")?;
        let pass = RenderPassDescriptor::new();
        let target = pass.color_attachments().object_at(0).unwrap();
        target.set_texture(Some(drawable.texture()));
        target.set_load_action(MTLLoadAction::DontCare);
        target.set_store_action(MTLStoreAction::Store);
        let cmd = self.queue.new_command_buffer();
        let enc = cmd.new_render_command_encoder(pass);
        enc.set_render_pipeline_state(&self.pipeline);
        enc.set_fragment_texture(0, Some(&y));
        enc.set_fragment_texture(1, Some(&uv));
        enc.set_fragment_bytes(0, 4, (&headroom as *const f32).cast());
        enc.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
        enc.end_encoding();
        cmd.present_drawable(drawable);
        cmd.commit();
        self.pending
            .push_back((cmd.to_owned(), native.clone(), cy, cu));
        Ok(())
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
