//! Runtime-only AMF HEVC path. ABI: AMF 1.4 core/{Factory,Context,Data,
//! Buffer,Variant}.h and components/{Component,VideoEncoderHEVC}.h. No SDK
//! library or native build is linked. All objects stay on the video worker.
use super::capture::Gpu;
use crate::{
    bitstream::{Format, Sequence},
    video::VideoConfig,
};
use anyhow::{ensure, Context, Result};
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
struct InterfaceTable {
    acquire: RcFn,
    release: RcFn,
    query: usize,
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
    context: usize,
    allocator: usize,
    caps: OutFn,
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
    fn get(&self, name: &str) -> Result<Variant> {
        let name16: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
        let mut value = Variant::default();
        // SAFETY: as for `set`; AMF fills the caller's variant.
        check(
            unsafe { (self.table::<Properties>().get)(self.ptr(), name16.as_ptr(), &mut value) },
            name,
        )?;
        Ok(value)
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
            (self.table::<InterfaceTable>().release)(self.ptr());
        }
    }
}
/// AMF_REPEAT: no output yet.
const AMF_REPEAT: i32 = 24;
/// AMF_SURFACE_NV12.
const NV12: i32 = 1;
/// AMF_SURFACE_P010.
#[cfg(test)]
const P010: i32 = 10;
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
/// The highest frame rate the GPU's HEVC encoder sustains at the stream's
/// size, from its throughput in 16×16 macroblocks a second.
fn max_fps(component: &Object, config: &VideoConfig) -> Option<u32> {
    let mut caps = null_mut();
    // SAFETY: GetCaps hands over one reference to an AMFCaps property storage.
    let caps = unsafe {
        check(
            (component.table::<ComponentTable>().caps)(component.ptr(), &mut caps),
            "caps",
        )
        .ok()?;
        Object::new(caps).ok()?
    };
    let throughput = caps.get("HevcMaxThroughput").ok()?.data[0];
    let blocks = u64::from(config.width.div_ceil(16)) * u64::from(config.height.div_ceil(16));
    u32::try_from(throughput / blocks.max(1))
        .ok()
        .filter(|&fps| fps > 0)
}
/// Long-term reference slots kept for recovery frames.
const LTR_SLOTS: usize = 4;

/// Recovery frames from long-term references (`docs/PROTOCOL.md`,
/// "RecoveryRequest"). Every few frames one is marked into the next slot,
/// round robin; after a loss the next frame predicts from the newest
/// slot whose picture the viewer has, and from nothing else.
struct Ltr {
    /// The picture each slot holds, as the encoder sees it.
    slots: [Option<u64>; LTR_SLOTS],
    /// Mark one picture in this many: four slots reach back about 130 ms.
    every: u64,
    marks: u64,
    /// The next picture predicts from this slot only; the viewer holds
    /// everything up to the second picture.
    force: Option<(usize, u64)>,
}

impl Ltr {
    fn new(fps: u32) -> Self {
        Self {
            slots: [None; LTR_SLOTS],
            every: u64::from(fps / 30).max(1),
            marks: 0,
            force: None,
        }
    }

    /// Per-picture properties for `picture`, and the picture a recovery
    /// frame predicts from.
    fn plan(&mut self, picture: u64, keyframe: bool) -> (Vec<(&'static str, i64)>, Option<u64>) {
        let mut props = Vec::new();
        let mut recovery = None;
        if keyframe {
            // An IDR empties the decoded picture buffer.
            self.slots = [None; LTR_SLOTS];
            self.force = None;
        } else if let Some((slot, base)) = self.force.take() {
            props.push(("HevcForceLTRReferenceBitfield", 1 << slot));
            // The encoder drops the long-term references a forced picture
            // does not use (AMF's default LTR mode).
            for (i, held) in self.slots.iter_mut().enumerate() {
                if i != slot {
                    *held = None;
                }
            }
            recovery = Some(base);
        }
        if recovery.is_none() && (keyframe || picture.is_multiple_of(self.every)) {
            let slot = (self.marks % LTR_SLOTS as u64) as usize;
            self.marks += 1;
            props.push(("HevcMarkCurrentWithLTRIndex", slot as i64));
            self.slots[slot] = Some(picture);
        }
        (props, recovery)
    }

    /// Plans the next picture to predict from the newest slot at or
    /// before `base`; false when there is none.
    fn recover_from(&mut self, base: u64) -> bool {
        let newest = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, held)| held.filter(|&p| p <= base).map(|p| (i, p)))
            .max_by_key(|&(_, p)| p);
        self.force = newest.map(|(slot, _)| (slot, base));
        self.force.is_some()
    }
}

pub(super) struct Encoder {
    component: Object,
    context: Object,
    // Retain the device while AMF holds its native pointer.
    _gpu: Gpu,
    seq: Sequence,
    fps: u32,
    time: i64,
    /// Inputs submitted whose output has not been taken yet.
    pub pending: usize,
    /// Pictures submitted so far; the last one's number.
    picture: u64,
    ltr: Option<Ltr>,
}
impl Encoder {
    pub fn new(config: &VideoConfig, gpu: &Gpu) -> Result<Self> {
        match Self::open(
            config,
            gpu,
            NV12,
            &[("HevcMaxOfLTRFrames", Variant::int(LTR_SLOTS as i64))],
        ) {
            Ok(mut encoder) => {
                encoder.ltr = Some(Ltr::new(encoder.fps));
                Ok(encoder)
            }
            Err(e) => {
                tracing::info!("AMF HEVC without long-term references: {e:#}");
                Self::open(config, gpu, NV12, &[])
            }
        }
    }

    /// Whether this encoder makes recovery frames.
    pub fn recovers(&self) -> bool {
        self.ltr.is_some()
    }

    /// See [`crate::video::VideoPipeline::recover_from`]: the next picture
    /// submitted predicts from a long-term reference the viewer holds.
    pub fn recover_from(&mut self, picture: u64) -> bool {
        self.ltr.as_mut().is_some_and(|l| l.recover_from(picture))
    }
    /// `extra` properties are set after Latch's own, before the encoder
    /// starts; `format` is the input surface format.
    fn open(
        config: &VideoConfig,
        gpu: &Gpu,
        format: i32,
        extra: &[(&str, Variant)],
    ) -> Result<Self> {
        // SAFETY: the factory is a process singleton; owned outputs have their
        // documented vtable. AMF_DX11_1=111 is an SDK enum.
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
            // Usage first: it resets every other property to its defaults.
            component.set("HevcUsage", Variant::int(1))?; // ultra low latency
            component.set("HevcQualityPreset", Variant::int(10))?; // speed
            component.set("HevcProfile", Variant::int(1))?; // Main
            component.set(
                "HevcFrameSize",
                Variant::pair(5, config.width, config.height),
            )?;
            // Asked for more pictures a second than the hardware encodes,
            // AMD's encoder stalls outright (4K at 240 fps on an RX 6600).
            // Rate control is told the rate it can reach instead; frames
            // never arrive faster than it encodes them, so the bits per
            // frame still hold the bitrate.
            let fps = match max_fps(&component, config) {
                Some(max) if max < config.fps => {
                    tracing::info!(
                        requested = config.fps,
                        max,
                        "AMF HEVC encodes at most {max} fps at {}x{}",
                        config.width,
                        config.height
                    );
                    max
                }
                _ => config.fps,
            };
            component.set("HevcFrameRate", Variant::pair(7, fps, 1))?;
            component.set("HevcRateControlMethod", Variant::int(3))?; // CBR

            // The buffer bounds each frame; padding small frames up to the
            // mean would only spend the link on filler NAL units.
            component.set("HevcEnforceHRD", Variant::boolean(true))?;
            component.set("HevcFillerDataEnable", Variant::boolean(false))?;
            // One endless GOP: an IDR only when the engine forces one.
            component.set("HevcGOPSize", Variant::int(0))?;
            component.set("HevcGOPSPerIDR", Variant::int(0))?;
            component.set("HevcRateControlSkipFrameEnable", Variant::boolean(false))?;
            for (name, value) in [
                ("LowLatencyInternal", Variant::boolean(true)),
                ("HevcEnablePreAnalysis", Variant::boolean(false)),
                ("HevcRateControlPreAnalysisEnable", Variant::boolean(false)),
                ("HevcInputQueueSize", Variant::int(2)),
                ("HevcQueryTimeout", Variant::int(1)),
                ("HevcHeaderInsertionMode", Variant::int(2)), // IDR aligned
            ] {
                if let Err(e) = component.set(name, value) {
                    tracing::debug!("{e:#}");
                }
            }
            for (name, value) in extra {
                component.set(name, *value)?;
            }
            let mut encoder = Self {
                component,
                context,
                _gpu: gpu.clone(),
                seq: Sequence::new(Format::Hevc),
                fps,
                time: 0,
                pending: 0,
                picture: 0,
                ltr: None,
            };
            encoder.bitrate(config.bitrate_kbps)?;
            check(
                (encoder.component.table::<ComponentTable>().init)(
                    encoder.component.ptr(),
                    format,
                    config.width as i32,
                    config.height as i32,
                ),
                "initialize HEVC encoder",
            )?;
            Ok(encoder)
        }
    }
    pub fn bitrate(&mut self, kbps: u32) -> Result<()> {
        let bits = i64::from(kbps) * 1000;
        self.component
            .set("HevcTargetBitrate", Variant::int(bits))?;
        self.component.set("HevcPeakBitrate", Variant::int(bits))?;
        // One frame interval of bits: no frame may take much longer than an
        // interval to cross a link that carries the bitrate.
        self.component.set(
            "HevcVBVBufferSize",
            Variant::int(bits / i64::from(self.fps)),
        )
    }
    /// Hands `texture` to the encoder without waiting for its output.
    /// Returns the picture's number and, for a recovery frame, the
    /// picture it was asked to recover from.
    pub fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        keyframe: bool,
    ) -> Result<(u64, Option<u64>)> {
        // SAFETY: AMF wraps and retains this D3D11 texture until its output
        // is taken; the caller keeps fewer inputs pending than its ring has
        // slots, so a slot is never rewritten while the encoder reads it.
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
                surface.set("HevcForcePictureType", Variant::int(2))?; // IDR
                surface.set("HevcInsertHeader", Variant::boolean(true))?;
            }
            self.picture += 1;
            let picture = self.picture;
            let (props, recovery) = self
                .ltr
                .as_mut()
                .map_or((Vec::new(), None), |l| l.plan(picture, keyframe));
            for (name, value) in props {
                surface.set(name, Variant::int(value))?;
            }
            check(
                (self.component.table::<ComponentTable>().submit)(
                    self.component.ptr(),
                    surface.ptr(),
                ),
                "submit input",
            )?;
            self.pending += 1;
            Ok((picture, recovery))
        }
    }
    /// Encodes one picture and waits for it.
    #[cfg(test)]
    pub fn encode(&mut self, texture: &ID3D11Texture2D, keyframe: bool) -> Result<(Vec<u8>, bool)> {
        let _ = self.submit(texture, keyframe)?;
        self.receive(Duration::from_secs(3))?
            .context("AMF output timed out")
    }
    /// The oldest pending input's access unit, waiting up to `wait` for it.
    pub fn receive(&mut self, wait: Duration) -> Result<Option<(Vec<u8>, bool)>> {
        let deadline = Instant::now() + wait;
        // SAFETY: the output buffer's native bytes live as long as `output`.
        unsafe {
            let table = self.component.table::<ComponentTable>();
            loop {
                let mut output = null_mut();
                // Blocks for at most the query timeout set at open; a
                // runtime without that timeout returns at once, so poll it.
                let asked = Instant::now();
                let code = (table.output)(self.component.ptr(), &mut output);
                if output.is_null() && asked.elapsed() < Duration::from_micros(500) {
                    std::thread::sleep(Duration::from_micros(100));
                }
                if !output.is_null() {
                    let output = Object::new(output)?;
                    check(code, "query output")?;
                    self.pending = self.pending.saturating_sub(1);
                    let table = output.table::<BufferTable>();
                    let length = (table.size)(output.ptr());
                    ensure!(length <= 64 * 1024 * 1024, "AMF output exceeds 64 MiB");
                    let bytes = (table.native)(output.ptr());
                    ensure!(!bytes.is_null(), "AMF output buffer is null");
                    return self
                        .seq
                        .assemble(std::slice::from_raw_parts(bytes.cast(), length), || None)
                        .map(Some);
                }
                if code != 0 && code != AMF_REPEAT {
                    check(code, "query output")?;
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
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
    #[repr(C)]
    struct CapsTable {
        properties: super::Properties,
        acceleration: usize,
        input: super::OutFn,
    }
    #[repr(C)]
    struct IoCapsTable {
        interface: super::InterfaceTable,
        width: unsafe extern "system" fn(super::Ptr, *mut i32, *mut i32),
        height: unsafe extern "system" fn(super::Ptr, *mut i32, *mut i32),
    }
    #[test]
    fn long_term_references_recover_only_from_what_the_viewer_holds() {
        use super::{Ltr, LTR_SLOTS};
        let mut ltr = Ltr::new(60);
        assert_eq!(ltr.every, 2);
        let mut marked = Vec::new();
        for picture in 1..=12 {
            let (props, recovery) = ltr.plan(picture, picture == 1);
            assert_eq!(recovery, None);
            if props
                .iter()
                .any(|(n, _)| *n == "HevcMarkCurrentWithLTRIndex")
            {
                marked.push(picture);
            }
        }
        // The IDR, then every other picture, round robin over the slots.
        assert_eq!(marked, [1, 2, 4, 6, 8, 10, 12]);
        assert_eq!(ltr.slots, [Some(8), Some(10), Some(12), Some(6)]);
        // The viewer holds up to 11: slot 1 (10) is the newest it has.
        assert!(ltr.recover_from(11));
        let (props, recovery) = ltr.plan(13, false);
        assert_eq!(recovery, Some(11));
        assert_eq!(props, [("HevcForceLTRReferenceBitfield", 1 << 1)]);
        assert_eq!(ltr.slots, [None, Some(10), None, None]);
        // Older than every slot left: nothing to predict from.
        assert!(!ltr.recover_from(9));
        // An IDR forgets every slot.
        ltr.plan(14, true);
        assert!(!ltr.recover_from(13));
        assert_eq!(ltr.slots.iter().flatten().count(), 1);
        assert_eq!(LTR_SLOTS, ltr.slots.len());
    }

    #[test]
    fn variant_matches_win64_amf_abi() {
        assert_eq!(std::mem::size_of::<super::Variant>(), 24);
        assert_eq!(std::mem::align_of::<super::Variant>(), 8);
        assert_eq!(std::mem::offset_of!(super::ContextTable, wrap_dx11), 49 * 8);
        assert_eq!(std::mem::offset_of!(super::ComponentTable, output), 23 * 8);
        assert_eq!(std::mem::offset_of!(super::BufferTable, native), 25 * 8);
        assert_eq!(std::mem::offset_of!(super::ComponentTable, caps), 26 * 8);
        assert_eq!(std::mem::offset_of!(CapsTable, input), 14 * 8);
        assert_eq!(std::mem::offset_of!(IoCapsTable, width), 3 * 8);
    }

    /// Run in the signed-in desktop on an AMD GPU: the encoder's own limits,
    /// one-at-a-time against pipelined throughput, and whether reference
    /// invalidation (LTR), intra refresh and Main10 work on this GPU.
    #[test]
    #[ignore]
    fn hardware_amf() -> anyhow::Result<()> {
        use super::super::{capture::Converter, display};
        use super::*;
        use crate::video::Codec;
        use std::collections::VecDeque;
        use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*};
        let _runtime = super::super::Runtime::new(false)?;
        let gpu = Gpu::new(&display::select()?.adapter)?;
        let config = |width, height, bitrate_kbps| VideoConfig {
            width,
            height,
            fps: 240,
            bitrate_kbps,
            codec: Codec::Hevc,
        };
        // Two different noise pictures: every frame changes everywhere.
        let noise = |width: u32, height: u32| -> anyhow::Result<Vec<ID3D11Texture2D>> {
            let mut seed = 0x1234_5678u32;
            (0..2)
                .map(|_| {
                    let texture = gpu.texture(
                        width,
                        height,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        D3D11_BIND_RENDER_TARGET.0 as u32,
                        false,
                    )?;
                    let bytes: Vec<u8> = (0..width * height * 4)
                        .map(|_| {
                            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            (seed >> 24) as u8
                        })
                        .collect();
                    // SAFETY: the buffer holds the whole texture at this pitch.
                    unsafe {
                        gpu.context.UpdateSubresource(
                            &texture,
                            0,
                            None,
                            bytes.as_ptr().cast(),
                            width * 4,
                            0,
                        );
                    }
                    Ok(texture)
                })
                .collect()
        };
        for (width, height, kbps) in [
            (1920, 1080, 50_000),
            (2560, 1440, 80_000),
            (3840, 2160, 120_000),
        ] {
            let inputs = noise(width, height)?;
            let mut converter = Converter::new(&gpu, (width, height), (width, height), 240)?;
            let encoder = Encoder::new(&config(width, height, kbps), &gpu)?;
            // SAFETY: GetCaps returns an owned AMFCaps property storage.
            unsafe {
                let mut caps = null_mut();
                check(
                    (encoder.component.table::<ComponentTable>().caps)(
                        encoder.component.ptr(),
                        &mut caps,
                    ),
                    "caps",
                )?;
                let caps = Object::new(caps)?;
                let mut input = null_mut();
                check(
                    (caps.table::<CapsTable>().input)(caps.ptr(), &mut input),
                    "input caps",
                )?;
                let input = Object::new(input)?;
                let (mut min_w, mut max_w, mut min_h, mut max_h) = (0, 0, 0, 0);
                (input.table::<IoCapsTable>().width)(input.ptr(), &mut min_w, &mut max_w);
                (input.table::<IoCapsTable>().height)(input.ptr(), &mut min_h, &mut max_h);
                eprintln!("HEVC input: width={min_w}..{max_w}, height={min_h}..{max_h}");
                let mut line = format!("{width}x{height} caps:");
                for name in [
                    "HevcMaxThroughput",
                    "HevcRequestedThroughput",
                    "HevcMaxBitrate",
                    "HevcMaxLevel",
                    "HevcNumOfHwInstances",
                    "HevcNumOfStreams",
                    "HevcMaxReferenceFrames",
                ] {
                    match caps.get(name) {
                        Ok(v) => line += &format!(" {name}={}", v.data[0] as i64),
                        Err(e) => line += &format!(" {name}=({e})"),
                    }
                }
                eprintln!("{line}");
            }
            drop(encoder);
            for (content, frames) in [("still", 1), ("noise", 2)] {
                for depth in 1..=2 {
                    let mut encoder = Encoder::new(&config(width, height, kbps), &gpu)?;
                    let mut sent = VecDeque::new();
                    let (mut latencies, mut done, total) = (Vec::new(), 0, 480);
                    let mut start = Instant::now();
                    let mut submitted = 0;
                    while done < total {
                        while encoder.pending < depth && submitted < total {
                            let picture = converter.convert(&inputs[submitted % frames])?;
                            sent.push_back(Instant::now());
                            encoder.submit(&picture.texture, submitted == 0)?;
                            submitted += 1;
                        }
                        encoder
                            .receive(Duration::from_secs(3))?
                            .context("AMF output timed out")?;
                        let at = sent.pop_front().context("output without input")?;
                        done += 1;
                        if done == 60 {
                            start = Instant::now();
                            latencies.clear();
                        }
                        latencies.push(at.elapsed().as_secs_f64() * 1000.0);
                    }
                    latencies.sort_by(f64::total_cmp);
                    eprintln!(
                        "{width}x{height} {content} depth={depth}: {:.0} fps, submit->output p50={:.2}ms p95={:.2}ms",
                        (total - 60) as f64 / start.elapsed().as_secs_f64(),
                        latencies[latencies.len() / 2],
                        latencies[latencies.len() * 95 / 100]
                    );
                }
            }
        }
        // Features for later work: each is opened, then exercised briefly.
        let (width, height) = (1920, 1080);
        let inputs = noise(width, height)?;
        let mut converter = Converter::new(&gpu, (width, height), (width, height), 240)?;
        let probe = |extra: &[(&str, Variant)]| {
            Encoder::open(&config(width, height, 50_000), &gpu, NV12, extra)
        };
        // Long-term references: mark frame 0, then force a later frame to
        // predict from it alone, as a viewer's loss report would.
        match probe(&[("HevcMaxOfLTRFrames", Variant::int(1))]) {
            Err(e) => eprintln!("LTR: open failed: {e:#}"),
            Ok(mut encoder) => {
                let mut result = Vec::new();
                for i in 0..8 {
                    let picture = converter.convert(&inputs[i % 2])?;
                    let mut props = vec![];
                    if i == 0 {
                        props.push(("HevcMarkCurrentWithLTRIndex", Variant::int(0)));
                    }
                    if i == 6 {
                        props.push(("HevcForceLTRReferenceBitfield", Variant::int(1)));
                    }
                    result.push(raw(&mut encoder, &picture.texture, i == 0, &props)?);
                }
                eprintln!("LTR frames (bytes, idr, marked, referenced): {result:?}");
            }
        }
        // Intra refresh: a band of intra CTBs per frame instead of IDRs.
        match probe(&[("HevcIntraRefreshCTBsNumberPerSlot", Variant::int(64))]) {
            Err(e) => eprintln!("intra refresh: open failed: {e:#}"),
            Ok(mut encoder) => {
                let mut sizes = Vec::new();
                for i in 0..8 {
                    let picture = converter.convert(&inputs[0])?;
                    let (bytes, idr, ..) = raw(&mut encoder, &picture.texture, i == 0, &[])?;
                    sizes.push((bytes, idr));
                }
                eprintln!("intra refresh frames (bytes, idr): {sizes:?}");
            }
        }
        // Main10 from a 10-bit (P010) surface.
        match Encoder::open(
            &config(width, height, 50_000),
            &gpu,
            P010,
            &[
                ("HevcProfile", Variant::int(2)),
                ("HevcColorBitDepth", Variant::int(10)),
            ],
        ) {
            Err(e) => eprintln!("Main10: open failed: {e:#}"),
            Ok(mut encoder) => {
                let texture = gpu.texture(
                    width,
                    height,
                    DXGI_FORMAT_P010,
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    false,
                )?;
                match encoder.encode(&texture, true) {
                    Err(e) => eprintln!("Main10: encode failed: {e:#}"),
                    Ok((data, idr)) => {
                        // general_profile_idc: the SPS's third byte after its NAL header.
                        let sps = crate::bitstream::nals(&data)
                            .find(|nal| (nal[0] >> 1) & 0x3f == 33)
                            .map(|nal| nal[3] & 0x1f);
                        eprintln!(
                            "Main10: {} bytes, idr={idr}, SPS general_profile_idc={sps:?}",
                            data.len()
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// One frame through `encoder` with per-frame properties, returning
    /// (bytes, idr, marked LTR index, referenced LTR bitfield).
    fn raw(
        encoder: &mut super::Encoder,
        texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        keyframe: bool,
        props: &[(&str, super::Variant)],
    ) -> anyhow::Result<(usize, bool, i64, i64)> {
        use super::*;
        // SAFETY: as in Encoder::submit and Encoder::receive.
        unsafe {
            let mut surface = null_mut();
            check(
                (encoder.context.table::<ContextTable>().wrap_dx11)(
                    encoder.context.ptr(),
                    texture.as_raw(),
                    &mut surface,
                    null_mut(),
                ),
                "wrap",
            )?;
            let surface = Object::new(surface)?;
            if keyframe {
                surface.set("HevcForcePictureType", Variant::int(2))?;
                surface.set("HevcInsertHeader", Variant::boolean(true))?;
            }
            for (name, value) in props {
                surface.set(name, *value)?;
            }
            let table = encoder.component.table::<ComponentTable>();
            check(
                (table.submit)(encoder.component.ptr(), surface.ptr()),
                "submit",
            )?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let mut output = null_mut();
                let code = (table.output)(encoder.component.ptr(), &mut output);
                if !output.is_null() {
                    let output = Object::new(output)?;
                    check(code, "output")?;
                    let marked = output
                        .get("HevcMarkedLTRIndex")
                        .map_or(-2, |v| v.data[0] as i64);
                    let referenced = output
                        .get("HevcReferencedLTRIndexBitfield")
                        .map_or(-2, |v| v.data[0] as i64);
                    let buffer = output.table::<BufferTable>();
                    let bytes = std::slice::from_raw_parts(
                        (buffer.native)(output.ptr()).cast::<u8>(),
                        (buffer.size)(output.ptr()),
                    );
                    let (data, idr) = encoder.seq.assemble(bytes, || None)?;
                    return Ok((data.len(), idr, marked, referenced));
                }
                anyhow::ensure!(Instant::now() < deadline, "no output");
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }
}
