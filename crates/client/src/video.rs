//! Drawing decoded frames with wgpu inside an egui rect: two textures (Y and
//! interleaved UV) and a shader that does the BT.709 conversion.

use egui_wgpu::wgpu;
#[cfg(target_os = "macos")]
use egui_wgpu::wgpu::hal;
use latch_stream::{Frame, FrameSlot};
use std::sync::Arc;

#[cfg(target_os = "macos")]
mod macos_zero_copy {
    use metal::foreign_types::ForeignTypeRef;
    use std::ffi::c_void;
    use std::ptr;

    pub enum CVMetalTextureCacheOpaque {}
    pub type CVMetalTextureCacheRef = *mut CVMetalTextureCacheOpaque;

    pub enum CVMetalTextureOpaque {}
    pub type CVMetalTextureRef = *mut CVMetalTextureOpaque;

    type CVReturn = i32;
    type CVOptionFlags = u64;

    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        fn CVMetalTextureCacheCreate(
            allocator: *const c_void,
            cache_attributes: *const c_void,
            metal_device: *mut c_void,
            texture_attributes: *const c_void,
            cache_out: *mut CVMetalTextureCacheRef,
        ) -> CVReturn;

        fn CVMetalTextureCacheCreateTextureFromImage(
            allocator: *const c_void,
            texture_cache: CVMetalTextureCacheRef,
            source_image: *const c_void,
            texture_attributes: *const c_void,
            pixel_format: usize,
            width: usize,
            height: usize,
            plane_index: usize,
            texture_out: *mut CVMetalTextureRef,
        ) -> CVReturn;

        fn CVMetalTextureGetTexture(image: CVMetalTextureRef) -> *mut c_void;

        fn CVMetalTextureCacheFlush(texture_cache: CVMetalTextureCacheRef, options: CVOptionFlags);

        fn CFRelease(cf: *const c_void);
    }

    pub struct MetalTextureCache {
        cache: CVMetalTextureCacheRef,
    }

    unsafe impl Send for MetalTextureCache {}
    unsafe impl Sync for MetalTextureCache {}

    impl MetalTextureCache {
        pub fn new(metal_device: *mut c_void) -> Result<Self, i32> {
            let mut cache: CVMetalTextureCacheRef = ptr::null_mut();
            let status = unsafe {
                CVMetalTextureCacheCreate(
                    ptr::null(),
                    ptr::null(),
                    metal_device,
                    ptr::null(),
                    &mut cache,
                )
            };
            if status != 0 || cache.is_null() {
                Err(status)
            } else {
                Ok(Self { cache })
            }
        }

        pub fn flush(&self) {
            unsafe {
                CVMetalTextureCacheFlush(self.cache, 0);
            }
        }

        pub fn create_texture(
            &self,
            pixel_buffer: *const c_void,
            format: usize,
            width: usize,
            height: usize,
            plane_index: usize,
        ) -> Result<(metal::Texture, CVMetalTextureWrapper), i32> {
            let mut cv_tex: CVMetalTextureRef = ptr::null_mut();
            let status = unsafe {
                CVMetalTextureCacheCreateTextureFromImage(
                    ptr::null(),
                    self.cache,
                    pixel_buffer,
                    ptr::null(),
                    format,
                    width,
                    height,
                    plane_index,
                    &mut cv_tex,
                )
            };
            if status != 0 || cv_tex.is_null() {
                return Err(status);
            }
            let raw_tex = unsafe { CVMetalTextureGetTexture(cv_tex) };
            if raw_tex.is_null() {
                unsafe { CFRelease(cv_tex as *const c_void) };
                return Err(-1);
            }
            let tex = unsafe {
                metal::TextureRef::from_ptr(raw_tex as *mut metal::MTLTexture).to_owned()
            };
            Ok((tex, CVMetalTextureWrapper(cv_tex)))
        }
    }

    impl Drop for MetalTextureCache {
        fn drop(&mut self) {
            if !self.cache.is_null() {
                unsafe { CFRelease(self.cache as *const c_void) };
            }
        }
    }

    pub struct CVMetalTextureWrapper(CVMetalTextureRef);
    unsafe impl Send for CVMetalTextureWrapper {}
    unsafe impl Sync for CVMetalTextureWrapper {}

    impl Drop for CVMetalTextureWrapper {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0 as *const c_void) };
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn warn_native_fallback(reason: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!("mac zero-copy unavailable ({reason}); falling back to CPU copy");
    }
}

#[cfg(target_os = "macos")]
struct InFlightFrame {
    frame: Frame,
    _cv_textures: (
        macos_zero_copy::CVMetalTextureWrapper,
        macos_zero_copy::CVMetalTextureWrapper,
    ),
    _wgpu_textures: (wgpu::Texture, wgpu::Texture),
    bind_group: wgpu::BindGroup,
}

const SHADER: &str = r#"
struct Uniforms {
    full_range: u32,
    srgb_target: u32,
    _pad0: u32,
    _pad1: u32,
};
@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // One triangle that covers the whole viewport (the callback rect).
    let x = f32(i32(i & 1u) * 4 - 1);
    let y = f32(i32(i >> 1u) * 4 - 1);
    var out: VsOut;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(y_tex, samp, in.uv).r;
    let c = textureSample(uv_tex, samp, in.uv).rg;
    var yy: f32;
    var cb: f32;
    var cr: f32;
    if (u.full_range == 1u) {
        yy = y;
        cb = c.x - 0.5;
        cr = c.y - 0.5;
    } else {
        yy = (y - 16.0 / 255.0) * (255.0 / 219.0);
        cb = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
        cr = (c.y - 128.0 / 255.0) * (255.0 / 224.0);
    }
    let r = yy + 1.5748 * cr;
    let g = yy - 0.1873 * cb - 0.4681 * cr;
    let b = yy + 1.8556 * cb;
    var rgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
    if (u.srgb_target == 1u) {
        rgb = to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Uniforms {
    full_range: u32,
    srgb_target: u32,
    _pad: [u32; 2],
}

impl Uniforms {
    fn bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&self.full_range.to_ne_bytes());
        b[4..8].copy_from_slice(&self.srgb_target.to_ne_bytes());
        b
    }
}

struct Planes {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

///// Lives in egui-wgpu's callback resources for the life of the window.
pub struct Resources {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    srgb_target: bool,
    planes: Option<Planes>,
    #[cfg(target_os = "macos")]
    metal_cache: Option<macos_zero_copy::MetalTextureCache>,
    #[cfg(target_os = "macos")]
    in_flight_ring: std::collections::VecDeque<InFlightFrame>,
    #[cfg(target_os = "macos")]
    frame_counter: u64,
}

impl Resources {
    pub fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("latch video"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("latch video"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("latch video"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("latch video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("latch video"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("latch video"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            sampler,
            uniforms,
            srgb_target: target.is_srgb(),
            planes: None,
            #[cfg(target_os = "macos")]
            metal_cache: None,
            #[cfg(target_os = "macos")]
            in_flight_ring: std::collections::VecDeque::with_capacity(4),
            #[cfg(target_os = "macos")]
            frame_counter: 0,
        }
    }

    fn bind_group(&self) -> Option<&wgpu::BindGroup> {
        #[cfg(target_os = "macos")]
        if let Some(in_flight) = self.in_flight_ring.back() {
            return Some(&in_flight.bind_group);
        }
        self.planes.as_ref().map(|p| &p.bind_group)
    }

    fn planes(&mut self, device: &wgpu::Device, width: u32, height: u32) -> &Planes {
        let stale = self
            .planes
            .as_ref()
            .is_none_or(|p| p.width != width || p.height != height);
        if stale {
            let make = |w, h, format, label| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let y = make(width, height, wgpu::TextureFormat::R8Unorm, "latch y");
            let uv = make(
                width.div_ceil(2),
                height.div_ceil(2),
                wgpu::TextureFormat::Rg8Unorm,
                "latch uv",
            );
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("latch video"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            &y.create_view(&Default::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(
                            &uv.create_view(&Default::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.uniforms.as_entire_binding(),
                    },
                ],
            });
            self.planes = Some(Planes {
                width,
                height,
                y,
                uv,
                bind_group,
            });
        }
        self.planes.as_ref().unwrap()
    }

    /// Upload the newest frame, if there is one.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, slot: &FrameSlot) {
        let Some(mut frame) = slot.take() else { return };
        #[cfg(target_os = "macos")]
        if frame.native.is_some() {
            match self.upload_native(device, queue, frame, slot) {
                Ok(()) => return,
                Err((mut f, reason)) => {
                    warn_native_fallback(&reason);
                    f.ensure_cpu_planes();
                    frame = f;
                }
            }
        }
        self.upload_cpu(device, queue, frame, slot);
    }

    #[cfg(target_os = "macos")]
    fn upload_native(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: Frame,
        slot: &FrameSlot,
    ) -> Result<(), (Frame, String)> {
        let native = match &frame.native {
            Some(n) => n,
            None => return Err((frame, "frame has no native buffer".into())),
        };
        let pb = native.pixel_buffer();
        if pb.is_null() {
            return Err((frame, "native pixel buffer is null".into()));
        }

        let mut mtl_dev_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let hal_ok = unsafe {
            device.as_hal::<hal::api::Metal, _, _>(|hal_dev| {
                if let Some(hal_dev) = hal_dev {
                    use metal::foreign_types::ForeignType;
                    let dev_lock = hal_dev.raw_device().lock();
                    mtl_dev_ptr = dev_lock.as_ptr() as *mut std::ffi::c_void;
                    true
                } else {
                    false
                }
            })
        };
        if !hal_ok || mtl_dev_ptr.is_null() {
            return Err((frame, "device is not Metal".into()));
        }

        if self.metal_cache.is_none() {
            match macos_zero_copy::MetalTextureCache::new(mtl_dev_ptr) {
                Ok(cache) => self.metal_cache = Some(cache),
                Err(e) => {
                    return Err((frame, format!("CVMetalTextureCacheCreate failed ({e})")));
                }
            }
        }
        let cache = self.metal_cache.as_ref().unwrap();

        let (mtl_y, cv_y) = match cache.create_texture(
            pb,
            metal::MTLPixelFormat::R8Unorm as usize,
            frame.width as usize,
            frame.height as usize,
            0,
        ) {
            Ok(pair) => pair,
            Err(e) => {
                return Err((
                    frame,
                    format!("createTextureFromImage plane 0 failed ({e})"),
                ));
            }
        };

        let uv_w = frame.width.div_ceil(2);
        let uv_h = frame.height.div_ceil(2);
        let (mtl_uv, cv_uv) = match cache.create_texture(
            pb,
            metal::MTLPixelFormat::RG8Unorm as usize,
            uv_w as usize,
            uv_h as usize,
            1,
        ) {
            Ok(pair) => pair,
            Err(e) => {
                return Err((
                    frame,
                    format!("createTextureFromImage plane 1 failed ({e})"),
                ));
            }
        };

        let hal_tex_y = unsafe {
            hal::metal::Device::texture_from_raw(
                mtl_y,
                wgpu::TextureFormat::R8Unorm,
                metal::MTLTextureType::D2,
                1,
                1,
                hal::CopyExtent {
                    width: frame.width,
                    height: frame.height,
                    depth: 1,
                },
            )
        };
        let desc_y = wgpu::TextureDescriptor {
            label: Some("latch y zero-copy"),
            size: wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let y = unsafe { device.create_texture_from_hal::<hal::api::Metal>(hal_tex_y, &desc_y) };

        let hal_tex_uv = unsafe {
            hal::metal::Device::texture_from_raw(
                mtl_uv,
                wgpu::TextureFormat::Rg8Unorm,
                metal::MTLTextureType::D2,
                1,
                1,
                hal::CopyExtent {
                    width: uv_w,
                    height: uv_h,
                    depth: 1,
                },
            )
        };
        let desc_uv = wgpu::TextureDescriptor {
            label: Some("latch uv zero-copy"),
            size: wgpu::Extent3d {
                width: uv_w,
                height: uv_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let uv = unsafe { device.create_texture_from_hal::<hal::api::Metal>(hal_tex_uv, &desc_uv) };

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("latch video zero-copy"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &y.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(
                        &uv.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.uniforms.as_entire_binding(),
                },
            ],
        });

        let u = Uniforms {
            full_range: frame.full_range as u32,
            srgb_target: self.srgb_target as u32,
            _pad: [0; 2],
        };
        queue.write_buffer(&self.uniforms, 0, &u.bytes());

        self.in_flight_ring.push_back(InFlightFrame {
            frame,
            _cv_textures: (cv_y, cv_uv),
            _wgpu_textures: (y, uv),
            bind_group,
        });

        if self.in_flight_ring.len() > 3 {
            if let Some(mut old) = self.in_flight_ring.pop_front() {
                old.frame.native = None;
                slot.recycle(old.frame);
            }
        }

        self.frame_counter = self.frame_counter.wrapping_add(1);
        if self.frame_counter.is_multiple_of(60) {
            if let Some(cache) = &self.metal_cache {
                cache.flush();
            }
        }

        Ok(())
    }

    fn upload_cpu(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: Frame,
        slot: &FrameSlot,
    ) {
        #[cfg(target_os = "macos")]
        while let Some(mut old) = self.in_flight_ring.pop_front() {
            old.frame.native = None;
            slot.recycle(old.frame);
        }
        let srgb = self.srgb_target;
        let planes = self.planes(device, frame.width, frame.height);
        let write = |tex: &wgpu::Texture, data: &[u8], stride: usize, w: u32, h: u32| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        write(
            &planes.y,
            &frame.y,
            frame.y_stride,
            frame.width,
            frame.height,
        );
        write(
            &planes.uv,
            &frame.uv,
            frame.uv_stride,
            frame.width.div_ceil(2),
            frame.height.div_ceil(2),
        );
        let u = Uniforms {
            full_range: frame.full_range as u32,
            srgb_target: srgb as u32,
            _pad: [0; 2],
        };
        queue.write_buffer(&self.uniforms, 0, &u.bytes());
        slot.recycle(frame);
    }
}

/// The paint callback egui runs for the video rect.
pub struct Paint {
    pub frames: Arc<FrameSlot>,
}

impl egui_wgpu::CallbackTrait for Paint {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(r) = resources.get_mut::<Resources>() {
            r.upload(device, queue, &self.frames);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(r) = resources.get::<Resources>() else {
            return;
        };
        let Some(bind_group) = r.bind_group() else {
            return;
        };
        pass.set_pipeline(&r.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// Install the shared GPU resources; call once when the window is created.
pub fn install(render_state: &egui_wgpu::RenderState) {
    let res = Resources::new(&render_state.device, render_state.target_format);
    render_state.renderer.write().callback_resources.insert(res);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a GPU"]
    fn decoded_frame_reaches_the_render_target() {
        struct TestApp(Arc<FrameSlot>);
        impl eframe::App for TestApp {
            fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(egui::Color32::BLACK))
                    .show(ctx, |ui| {
                        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                            ui.max_rect(),
                            Paint {
                                frames: self.0.clone(),
                            },
                        ));
                    });
            }
        }
        let frames = Arc::new(FrameSlot::default());
        let encoded = include_bytes!("../../stream/tests/fixtures/gray-bars.h264");
        let mut decoder = latch_stream::video::new_decoder(latch_stream::Codec::H264High).unwrap();
        let frame = decoder
            .decode(encoded, true, None)
            .unwrap()
            .expect("decoded frame");
        #[cfg(target_os = "macos")]
        assert!(
            frame.native.is_some(),
            "decoded frame must have a native CVPixelBuffer"
        );
        frames.publish(frame);
        let mut harness = egui_kittest::Harness::builder()
            .wgpu()
            .with_size(egui::vec2(128.0, 128.0))
            .build_eframe(move |cc| {
                install(cc.wgpu_render_state.as_ref().unwrap());
                TestApp(frames)
            });
        harness.run_steps(2);
        for _ in 0..2 {
            let image = harness.render().unwrap();
            let top = image.get_pixel(64, 32);
            let bottom = image.get_pixel(64, 96);
            assert!(top.0[..3].iter().all(|&c| c >= 250), "white half: {top:?}");
            assert!(
                bottom.0[..3].iter().all(|&c| c <= 5),
                "black half: {bottom:?}"
            );
        }
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn cpu_frame_reaches_the_render_target() {
        struct TestApp(Arc<FrameSlot>);
        impl eframe::App for TestApp {
            fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(egui::Color32::BLACK))
                    .show(ctx, |ui| {
                        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                            ui.max_rect(),
                            Paint {
                                frames: self.0.clone(),
                            },
                        ));
                    });
            }
        }
        let frames = Arc::new(FrameSlot::default());
        frames.publish(latch_stream::Frame {
            width: 64,
            height: 64,
            y: (0..64)
                .flat_map(|row| vec![if row < 32 { 235 } else { 16 }; 64])
                .collect(),
            y_stride: 64,
            uv: vec![128; 64 * 32],
            uv_stride: 64,
            full_range: false,
            ..Default::default()
        });
        let mut harness = egui_kittest::Harness::builder()
            .wgpu()
            .with_size(egui::vec2(128.0, 128.0))
            .build_eframe(move |cc| {
                install(cc.wgpu_render_state.as_ref().unwrap());
                TestApp(frames)
            });
        harness.run_steps(2);
        for _ in 0..2 {
            let image = harness.render().unwrap();
            let top = image.get_pixel(64, 32);
            let bottom = image.get_pixel(64, 96);
            assert!(top.0[..3].iter().all(|&c| c >= 250), "white half: {top:?}");
            assert!(
                bottom.0[..3].iter().all(|&c| c <= 5),
                "black half: {bottom:?}"
            );
        }
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn bench_upload_and_render_3024x1964() {
        struct BenchApp(Arc<FrameSlot>);
        impl eframe::App for BenchApp {
            fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(egui::Color32::BLACK))
                    .show(ctx, |ui| {
                        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                            ui.max_rect(),
                            Paint {
                                frames: self.0.clone(),
                            },
                        ));
                    });
            }
        }
        let frames = Arc::new(FrameSlot::default());
        let width = 3024;
        let height = 1964;

        #[cfg(target_os = "macos")]
        let make_frame = || {
            #[link(name = "CoreVideo", kind = "framework")]
            extern "C" {}
            #[link(name = "CoreFoundation", kind = "framework")]
            extern "C" {
                static kCFTypeDictionaryKeyCallBacks: std::ffi::c_void;
                static kCFTypeDictionaryValueCallBacks: std::ffi::c_void;
                static kCVPixelBufferIOSurfacePropertiesKey: *const std::ffi::c_void;
                static kCVPixelBufferMetalCompatibilityKey: *const std::ffi::c_void;
                static kCFBooleanTrue: *const std::ffi::c_void;
                fn CFDictionaryCreate(
                    allocator: *const std::ffi::c_void,
                    keys: *const *const std::ffi::c_void,
                    values: *const *const std::ffi::c_void,
                    num: isize,
                    key_callbacks: *const std::ffi::c_void,
                    value_callbacks: *const std::ffi::c_void,
                ) -> *const std::ffi::c_void;
                fn CFRelease(cf: *const std::ffi::c_void);
                fn CVPixelBufferCreate(
                    allocator: *const std::ffi::c_void,
                    width: usize,
                    height: usize,
                    pixel_format_type: u32,
                    pixel_buffer_attributes: *const std::ffi::c_void,
                    pixel_buffer_out: *mut *const std::ffi::c_void,
                ) -> i32;
                fn CVPixelBufferGetBytesPerRowOfPlane(
                    pb: *const std::ffi::c_void,
                    plane: usize,
                ) -> usize;
            }
            unsafe {
                let empty_dict = CFDictionaryCreate(
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                let keys = [
                    kCVPixelBufferIOSurfacePropertiesKey,
                    kCVPixelBufferMetalCompatibilityKey,
                ];
                let values = [empty_dict, kCFBooleanTrue];
                let attrs = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    2,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                let mut pb = std::ptr::null();
                let status = CVPixelBufferCreate(
                    std::ptr::null(),
                    width,
                    height,
                    0x3432_3076, // '420v'
                    attrs,
                    &mut pb,
                );
                CFRelease(attrs);
                CFRelease(empty_dict);
                assert_eq!(status, 0, "CVPixelBufferCreate failed");
                let ys = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
                let uvs = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
                Frame {
                    width: width as u32,
                    height: height as u32,
                    y_stride: ys,
                    uv_stride: uvs,
                    native: Some(latch_stream::video::NativeFrame::new(pb)),
                    ..Default::default()
                }
            }
        };

        #[cfg(not(target_os = "macos"))]
        let make_frame = || Frame {
            width: width as u32,
            height: height as u32,
            y: vec![16; width * height],
            y_stride: width,
            uv: vec![128; (width / 2) * (height / 2) * 2],
            uv_stride: width,
            full_range: false,
        };

        frames.publish(make_frame());
        let frames_clone = frames.clone();
        let mut harness = egui_kittest::Harness::builder()
            .wgpu()
            .with_size(egui::vec2(width as f32, height as f32))
            .build_eframe(move |cc| {
                install(cc.wgpu_render_state.as_ref().unwrap());
                BenchApp(frames_clone)
            });

        // Warmup: 10 frames
        for _ in 0..10 {
            frames.publish(make_frame());
            harness.run_steps(1);
            let _ = harness.render().unwrap();
        }

        // Run: 50 frames
        let start = std::time::Instant::now();
        for _ in 0..50 {
            frames.publish(make_frame());
            harness.run_steps(1);
            let _ = harness.render().unwrap();
        }
        let elapsed = start.elapsed();
        let per_frame = elapsed.as_secs_f64() * 1000.0 / 50.0;
        eprintln!(
            "3024x1964 upload and render: 50 frames in {elapsed:?} ({per_frame:.2} ms/frame)"
        );
    }
}
