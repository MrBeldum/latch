//! Runtime-only AMF HEVC path. ABI: AMF 1.4 core/{Factory,Context,Data,
//! Buffer,Variant}.h and components/{Component,VideoEncoderHEVC}.h. No SDK
//! library or native build is linked. All objects stay on the video worker.
use super::capture::Gpu;
use crate::{
    bitstream::{Format, Sequence},
    video::VideoConfig,
};
use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::c_void,
    ptr::{null_mut, NonNull},
    sync::OnceLock,
    time::{Duration, Instant},
};
use windows::{
    core::{s, w, Interface},
    Win32::{Foundation::HANDLE, Graphics::Direct3D11::ID3D11Texture2D, System::LibraryLoader::*},
};

type Ptr = *mut c_void;
type RcFn = unsafe extern "system" fn(Ptr) -> i32;
type OutFn = unsafe extern "system" fn(Ptr, *mut Ptr) -> i32;
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Variant {
    kind: u32,
    padding: u32,
    data: [u64; 2],
}
impl Variant {
    fn int(value: i64) -> Self {
        Self {
            kind: 2,
            data: [value as u64, 0],
            ..Self::default()
        }
    }
    fn boolean(value: bool) -> Self {
        Self {
            kind: 1,
            data: [u64::from(value), 0],
            ..Self::default()
        }
    }
    fn pair(kind: u32, a: u32, b: u32) -> Self {
        Self {
            kind,
            data: [u64::from(a) | (u64::from(b) << 32), 0],
            ..Self::default()
        }
    }
}
#[repr(C)]
struct Properties {
    acquire: RcFn,
    release: RcFn,
    query: usize,
    set: unsafe extern "system" fn(Ptr, *const u16, Variant) -> i32,
    get: unsafe extern "system" fn(Ptr, *const u16, *mut Variant) -> i32,
    rest: [usize; 8],
}
#[repr(C)]
struct ContextTable {
    properties: Properties,
    terminate: RcFn,
    dx9: [usize; 4],
    init_dx11: unsafe extern "system" fn(Ptr, Ptr, i32) -> i32,
    rest: [usize; 30],
    wrap_dx11: unsafe extern "system" fn(Ptr, Ptr, *mut Ptr, Ptr) -> i32,
}
#[repr(C)]
struct ComponentTable {
    properties: Properties,
    extended: [usize; 4],
    init: unsafe extern "system" fn(Ptr, i32, i32, i32) -> i32,
    reinit: usize,
    terminate: RcFn,
    drain: usize,
    flush: usize,
    submit: unsafe extern "system" fn(Ptr, Ptr) -> i32,
    output: OutFn,
}
#[repr(C)]
struct DataTable {
    properties: Properties,
    data: [usize; 6],
    set_pts: unsafe extern "system" fn(Ptr, i64),
    get_pts: usize,
    set_duration: unsafe extern "system" fn(Ptr, i64),
}
#[repr(C)]
struct BufferTable {
    properties: Properties,
    data: [usize; 10],
    set_size: usize,
    size: unsafe extern "system" fn(Ptr) -> usize,
    native: unsafe extern "system" fn(Ptr) -> Ptr,
}
#[repr(C)]
struct FactoryTable {
    context: OutFn,
    component: unsafe extern "system" fn(Ptr, Ptr, *const u16, *mut Ptr) -> i32,
}
struct Object(NonNull<c_void>);
impl Object {
    fn new(ptr: Ptr) -> Result<Self> {
        Ok(Self(
            NonNull::new(ptr).context("AMF returned null interface")?,
        ))
    }
    fn ptr(&self) -> Ptr {
        self.0.as_ptr()
    }
    unsafe fn table<T>(&self) -> &T {
        // SAFETY: callers select the exact table for the interface's creation API.
        unsafe { &**self.ptr().cast::<*const T>() }
    }
    fn set(&self, name: &str, value: Variant) -> Result<()> {
        let name16: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: all reference-counted AMF objects inherit AMFPropertyStorage.
        check(
            unsafe { (self.table::<Properties>().set)(self.ptr(), name16.as_ptr(), value) },
            name,
        )
    }
}
impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: Object adopts one reference from an AMF creation call.
        unsafe {
            (self.table::<Properties>().release)(self.ptr());
        }
    }
}
fn check(code: i32, action: &str) -> Result<()> {
    ensure!(code == 0, "AMF {action}: result {code}");
    Ok(())
}
fn factory() -> Result<Ptr> {
    static FACTORY: OnceLock<Result<usize, String>> = OnceLock::new();
    let ptr = FACTORY
        .get_or_init(|| {
            (|| -> Result<usize> {
                // SAFETY: retain the system driver runtime for all AMF object lifetimes.
                unsafe {
                    let module = LoadLibraryExW(
                        w!("amfrt64.dll"),
                        HANDLE::default(),
                        LOAD_LIBRARY_SEARCH_SYSTEM32,
                    )?;
                    let init = GetProcAddress(module, s!("AMFInit")).context("AMFInit export")?;
                    let init: unsafe extern "C" fn(u64, *mut Ptr) -> i32 =
                        std::mem::transmute(init);
                    let mut factory = null_mut();
                    check(
                        init((1 << 48) | (4 << 32) | (30 << 16), &mut factory),
                        "initialize 1.4.30",
                    )?;
                    ensure!(!factory.is_null(), "AMF returned null factory");
                    Ok(factory as usize)
                }
            })()
            .map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(*ptr as Ptr)
}
pub(super) struct Encoder {
    component: Object,
    context: Object,
    // Retain the device while AMF holds its native pointer.
    _gpu: Gpu,
    seq: Sequence,
    fps: u32,
    time: i64,
}
impl Encoder {
    pub fn new(config: &VideoConfig, gpu: &Gpu) -> Result<Self> {
        // SAFETY: the factory is a process singleton; owned outputs have their
        // documented vtable. AMF_DX11_1=111 and AMF_SURFACE_NV12=1 are SDK enums.
        unsafe {
            let factory = factory()?;
            let table = &**factory.cast::<*const FactoryTable>();
            let mut context = null_mut();
            check((table.context)(factory, &mut context), "create context")?;
            let context = Object::new(context)?;
            check(
                (context.table::<ContextTable>().init_dx11)(
                    context.ptr(),
                    gpu.device.as_raw(),
                    111,
                ),
                "initialize DX11",
            )?;
            let mut component = null_mut();
            check(
                (table.component)(
                    factory,
                    context.ptr(),
                    w!("AMFVideoEncoderHW_HEVC").as_ptr(),
                    &mut component,
                ),
                "create HEVC",
            )?;
            let component = Object::new(component)?;
            component.set("HevcUsage", Variant::int(1))?; // ultra low latency
            component.set("HevcQualityPreset", Variant::int(10))?; // speed
            component.set("HevcProfile", Variant::int(1))?;
            component.set(
                "HevcFrameSize",
                Variant::pair(5, config.width, config.height),
            )?;
            component.set("HevcFrameRate", Variant::pair(7, config.fps, 1))?;
            component.set("HevcRateControlMethod", Variant::int(3))?;
            component.set("HevcFillerDataEnable", Variant::boolean(true))?;
            component.set("HevcEnforceHRD", Variant::boolean(true))?; // CBR
            component.set("HevcGOPSize", Variant::int(0))?; // infinite
            component.set("HevcRateControlSkipFrameEnable", Variant::boolean(false))?;
            for (name, value) in [
                ("LowLatencyInternal", Variant::boolean(true)),
                ("HevcEnablePreAnalysis", Variant::boolean(false)),
                ("HevcRateControlPreAnalysisEnable", Variant::boolean(false)),
                ("HevcInputQueueSize", Variant::int(1)),
                ("HevcQueryTimeout", Variant::int(10)),
                ("HevcHeaderInsertionMode", Variant::int(2)),
            ] {
                if let Err(e) = component.set(name, value) {
                    tracing::debug!("{e:#}");
                }
            }
            let mut encoder = Self {
                component,
                context,
                _gpu: gpu.clone(),
                seq: Sequence::new(Format::Hevc),
                fps: config.fps,
                time: 0,
            };
            encoder.bitrate(config.bitrate_kbps)?;
            check(
                (encoder.component.table::<ComponentTable>().init)(
                    encoder.component.ptr(),
                    1,
                    config.width as i32,
                    config.height as i32,
                ),
                "initialize HEVC encoder",
            )?;
            Ok(encoder)
        }
    }
    pub fn bitrate(&mut self, kbps: u32) -> Result<()> {
        self.component
            .set("HevcTargetBitrate", Variant::int(i64::from(kbps) * 1000))?;
        self.component
            .set("HevcPeakBitrate", Variant::int(i64::from(kbps) * 1000))?;
        self.component.set(
            "HevcVBVBufferSize",
            Variant::int(i64::from(kbps) * 1000 / i64::from(self.fps)),
        )
    }
    pub fn encode(&mut self, texture: &ID3D11Texture2D, keyframe: bool) -> Result<(Vec<u8>, bool)> {
        // SAFETY: AMF wraps and retains this D3D11 texture. Only one input is
        // outstanding; its output is collected before the ring slot can recur.
        unsafe {
            let mut surface = null_mut();
            check(
                (self.context.table::<ContextTable>().wrap_dx11)(
                    self.context.ptr(),
                    texture.as_raw(),
                    &mut surface,
                    null_mut(),
                ),
                "wrap DX11 surface",
            )?;
            let surface = Object::new(surface)?;
            let duration = 10_000_000 / i64::from(self.fps);
            (surface.table::<DataTable>().set_pts)(surface.ptr(), self.time);
            (surface.table::<DataTable>().set_duration)(surface.ptr(), duration);
            self.time += duration;
            if keyframe {
                surface.set("HevcForcePictureType", Variant::int(2))?;
                surface.set("HevcInsertHeader", Variant::boolean(true))?;
            }
            let table = self.component.table::<ComponentTable>();
            check(
                (table.submit)(self.component.ptr(), surface.ptr()),
                "submit input",
            )?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let mut output = null_mut();
                let code = (table.output)(self.component.ptr(), &mut output);
                if !output.is_null() {
                    let output = Object::new(output)?;
                    check(code, "query output")?;
                    let table = output.table::<BufferTable>();
                    let length = (table.size)(output.ptr());
                    ensure!(length <= 64 * 1024 * 1024, "AMF output exceeds 64 MiB");
                    let bytes = (table.native)(output.ptr());
                    ensure!(!bytes.is_null(), "AMF output buffer is null");
                    return self
                        .seq
                        .assemble(std::slice::from_raw_parts(bytes.cast(), length), || None);
                }
                if code != 0 && code != 24 {
                    check(code, "query output")?;
                }
                if Instant::now() >= deadline {
                    bail!("AMF output timed out");
                }
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: terminate before releasing references and the D3D11 device.
        unsafe {
            (self.component.table::<ComponentTable>().terminate)(self.component.ptr());
            (self.context.table::<ContextTable>().terminate)(self.context.ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn variant_matches_win64_amf_abi() {
        assert_eq!(std::mem::size_of::<super::Variant>(), 24);
        assert_eq!(std::mem::align_of::<super::Variant>(), 8);
        assert_eq!(std::mem::offset_of!(super::ContextTable, wrap_dx11), 49 * 8);
        assert_eq!(std::mem::offset_of!(super::ComponentTable, output), 23 * 8);
        assert_eq!(std::mem::offset_of!(super::BufferTable, native), 25 * 8);
    }
}
