//! VA-API encoding (Intel's iHD and Mesa's AMD/Intel drivers) through
//! `libva.so.2` and `libva-drm.so.2`, loaded when first asked for, on the
//! first DRM render node whose driver encodes the codec asked for (an
//! Intel iGPU beside an AMD card, or NVIDIA's decode-only VA-API driver in
//! front of an Intel one, may each have what the other lacks). Structures
//! follow libva 2.x (`va.h`, `va_enc_h264.h`, `va_enc_hevc.h`), whose
//! layout libva keeps stable; their sizes are checked at compile time.
//!
//! H.264 (Constrained Baseline or High) and HEVC Main, with the driver
//! writing the parameter sets and slice headers (no packed headers): one
//! slice, P-frames that refer to the frame before, IDRs on request, CBR
//! with a one-frame HRD buffer. HEVC parameters have no conformance window
//! without packed headers, so HEVC codes only sizes in whole minimum
//! coding blocks (8 pixels or more), and other sizes are not offered it.
//! Pictures arrive as NV12 converted by the CPU. A driver that writes no parameter sets fails the probe in
//! [`super::hardware`] and is not used. AV1 needs the application to
//! write sequence and frame headers and is not offered.

use super::hardware::{is_unusable, unusable, Caps};
use super::nv12;
use crate::software::PictureEncoder;
use crate::video::{Codec, VideoConfig};
use anyhow::{ensure, Context, Result};
use latch_codec::Yuv420;
use libloading::Library;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr::null_mut;
use std::sync::OnceLock;

type Display = *mut c_void;
type Status = c_int;
type Id = c_uint;

const SUCCESS: Status = 0;
const INVALID_ID: Id = 0xffff_ffff;
const PROFILE_H264_CONSTRAINED_BASELINE: c_int = 13;
const PROFILE_H264_HIGH: c_int = 7;
const PROFILE_HEVC_MAIN: c_int = 17;
const ENTRYPOINT_ENC_SLICE: c_int = 6;
const ENTRYPOINT_ENC_SLICE_LP: c_int = 8;
const ATTRIB_RT_FORMAT: c_int = 0;
const ATTRIB_RATE_CONTROL: c_int = 5;
const ATTRIB_ENC_MAX_REF_FRAMES: c_int = 13;
const ATTRIB_MAX_PICTURE_WIDTH: c_int = 18;
const ATTRIB_MAX_PICTURE_HEIGHT: c_int = 19;
const ATTRIB_ENC_HEVC_BLOCK_SIZES: c_int = 51;
const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
const RT_FORMAT_YUV420: u32 = 0x1;
const RC_CBR: u32 = 0x2;
const RC_VBR: u32 = 0x4;
const FOURCC_NV12: u32 = 0x3231_564e;
const CODED_BUFFER: c_int = 21;
const SEQUENCE_PARAMETER_BUFFER: c_int = 22;
const PICTURE_PARAMETER_BUFFER: c_int = 23;
const SLICE_PARAMETER_BUFFER: c_int = 24;
const MISC_PARAMETER_BUFFER: c_int = 27;
const MISC_FRAME_RATE: u32 = 0;
const MISC_RATE_CONTROL: u32 = 1;
const MISC_HRD: u32 = 5;
const H264_INVALID: u32 = 0x1;
const H264_SHORT_TERM_REFERENCE: u32 = 0x8;
const HEVC_INVALID: u32 = 0x1;
const HEVC_RPS_ST_CURR_BEFORE: u32 = 0x10;
/// Statuses that say a driver has no such encoder: a configuration that
/// will never open, unlike a failed allocation or an unsupported size.
const UNSUPPORTED: [Status; 5] = [
    0x0a, // attribute not supported
    0x0c, // profile
    0x0d, // entry point
    0x0e, // render target format
    0x14, // unimplemented
];

#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigAttrib {
    kind: c_int,
    value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Image {
    image_id: Id,
    format: ImageFormat,
    buf: Id,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: i32,
    entry_bytes: i32,
    component_order: [i8; 4],
    va_reserved: [u32; 4],
}

#[repr(C)]
struct CodedBufferSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut CodedBufferSegment,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PictureH264 {
    picture_id: Id,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
    va_reserved: [u32; 4],
}
const NO_PICTURE_H264: PictureH264 = PictureH264 {
    picture_id: INVALID_ID,
    frame_idx: 0,
    flags: H264_INVALID,
    top_field_order_cnt: 0,
    bottom_field_order_cnt: 0,
    va_reserved: [0; 4],
};

#[repr(C)]
struct SequenceH264 {
    seq_parameter_set_id: u8,
    level_idc: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    max_num_ref_frames: u32,
    picture_width_in_mbs: u16,
    picture_height_in_mbs: u16,
    seq_fields: u32,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames_in_pic_order_cnt_cycle: u8,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: [i32; 256],
    frame_cropping_flag: u8,
    frame_crop_left_offset: u32,
    frame_crop_right_offset: u32,
    frame_crop_top_offset: u32,
    frame_crop_bottom_offset: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct PictureParameterH264 {
    curr_pic: PictureH264,
    reference_frames: [PictureH264; 16],
    coded_buf: Id,
    pic_parameter_set_id: u8,
    seq_parameter_set_id: u8,
    last_picture: u8,
    frame_num: u16,
    pic_init_qp: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct SliceH264 {
    macroblock_address: u32,
    num_macroblocks: u32,
    macroblock_info: Id,
    slice_type: u8,
    pic_parameter_set_id: u8,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_active_override_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [PictureH264; 32],
    ref_pic_list1: [PictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PictureHevc {
    picture_id: Id,
    pic_order_cnt: i32,
    flags: u32,
    va_reserved: [u32; 4],
}
const NO_PICTURE_HEVC: PictureHevc = PictureHevc {
    picture_id: INVALID_ID,
    pic_order_cnt: 0,
    flags: HEVC_INVALID,
    va_reserved: [0; 4],
};

#[repr(C)]
struct SequenceHevc {
    general_profile_idc: u8,
    general_level_idc: u8,
    general_tier_flag: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    pic_width_in_luma_samples: u16,
    pic_height_in_luma_samples: u16,
    seq_fields: u32,
    log2_min_luma_coding_block_size_minus3: u8,
    log2_diff_max_min_luma_coding_block_size: u8,
    log2_min_transform_block_size_minus2: u8,
    log2_diff_max_min_transform_block_size: u8,
    max_transform_hierarchy_depth_inter: u8,
    max_transform_hierarchy_depth_intra: u8,
    pcm_sample_bit_depth_luma_minus1: u32,
    pcm_sample_bit_depth_chroma_minus1: u32,
    log2_min_pcm_luma_coding_block_size_minus3: u32,
    log2_max_pcm_luma_coding_block_size_minus3: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    vui_num_units_in_tick: u32,
    vui_time_scale: u32,
    min_spatial_segmentation_idc: u16,
    max_bytes_per_pic_denom: u8,
    max_bits_per_min_cu_denom: u8,
    scc_fields: u32,
    va_reserved: [u32; 7],
}

#[repr(C)]
struct PictureParameterHevc {
    decoded_curr_pic: PictureHevc,
    reference_frames: [PictureHevc; 15],
    coded_buf: Id,
    collocated_ref_pic_index: u8,
    last_picture: u8,
    pic_init_qp: u8,
    diff_cu_qp_delta_depth: u8,
    pps_cb_qp_offset: i8,
    pps_cr_qp_offset: i8,
    num_tile_columns_minus1: u8,
    num_tile_rows_minus1: u8,
    column_width_minus1: [u8; 19],
    row_height_minus1: [u8; 21],
    log2_parallel_merge_level_minus2: u8,
    ctu_max_bitsize_allowed: u8,
    num_ref_idx_l0_default_active_minus1: u8,
    num_ref_idx_l1_default_active_minus1: u8,
    slice_pic_parameter_set_id: u8,
    nal_unit_type: u8,
    pic_fields: u32,
    hierarchical_level_plus1: u8,
    va_byte_reserved: u8,
    scc_fields: u16,
    va_reserved: [u32; 15],
}

#[repr(C)]
struct SliceHevc {
    slice_segment_address: u32,
    num_ctu_in_slice: u32,
    slice_type: u8,
    slice_pic_parameter_set_id: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [PictureHevc; 15],
    ref_pic_list1: [PictureHevc; 15],
    luma_log2_weight_denom: u8,
    delta_chroma_log2_weight_denom: i8,
    delta_luma_weight_l0: [i8; 15],
    luma_offset_l0: [i8; 15],
    delta_chroma_weight_l0: [[i8; 2]; 15],
    chroma_offset_l0: [[i8; 2]; 15],
    delta_luma_weight_l1: [i8; 15],
    luma_offset_l1: [i8; 15],
    delta_chroma_weight_l1: [[i8; 2]; 15],
    chroma_offset_l1: [[i8; 2]; 15],
    max_num_merge_cand: u8,
    slice_qp_delta: i8,
    slice_cb_qp_offset: i8,
    slice_cr_qp_offset: i8,
    slice_beta_offset_div2: i8,
    slice_tc_offset_div2: i8,
    slice_fields: u32,
    pred_weight_table_bit_offset: u32,
    pred_weight_table_bit_length: u32,
    va_reserved: [u32; 6],
}

#[repr(C)]
struct RateControl {
    bits_per_second: u32,
    target_percentage: u32,
    window_size: u32,
    initial_qp: u32,
    min_qp: u32,
    basic_unit_size: u32,
    rc_flags: u32,
    icq_quality_factor: u32,
    max_qp: u32,
    quality_factor: u32,
    target_frame_size: u32,
    va_reserved: [u32; 4],
}
/// `rc_flags.disable_frame_skip`: a frame the driver skipped would be one
/// the viewer never gets.
const RC_DISABLE_FRAME_SKIP: u32 = 1 << 1;

#[repr(C)]
struct FrameRate {
    framerate: u32,
    framerate_flags: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct Hrd {
    initial_buffer_fullness: u32,
    buffer_size: u32,
    va_reserved: [u32; 4],
}

// Sizes from libva 2.x's headers (LP64).
const _: () = {
    use std::mem::size_of;
    assert!(size_of::<ImageFormat>() == 48);
    assert!(size_of::<Image>() == 120);
    assert!(size_of::<CodedBufferSegment>() == 48);
    assert!(size_of::<PictureH264>() == 36);
    assert!(size_of::<SequenceH264>() == 1132);
    assert!(size_of::<PictureParameterH264>() == 648);
    assert!(size_of::<SliceH264>() == 3140);
    assert!(size_of::<PictureHevc>() == 28);
    assert!(size_of::<SequenceHevc>() == 116);
    assert!(size_of::<PictureParameterHevc>() == 576);
    assert!(size_of::<SliceHevc>() == 1076);
    assert!(size_of::<RateControl>() == 60);
};

fn zeroed<T>() -> Box<T> {
    // SAFETY: only for the repr(C) integer/array structures above, for
    // which all-zero bytes are valid.
    unsafe { Box::new(std::mem::zeroed()) }
}

macro_rules! entry_points {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Api {
            _va: Library,
            _drm: Library,
            get_display_drm: unsafe extern "C" fn(c_int) -> Display,
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }
        fn load() -> Result<Api> {
            // SAFETY: libva's initializers have no preconditions; symbol
            // types are those of va.h and va_drm.h.
            unsafe {
                let va = Library::new("libva.so.2").context("no libva.so.2")?;
                let drm = Library::new("libva-drm.so.2").context("no libva-drm.so.2")?;
                Ok(Api {
                    get_display_drm: *drm.get(b"vaGetDisplayDRM\0")?,
                    $($name: *va.get(concat!(stringify!($name), "\0").as_bytes())?,)*
                    _va: va,
                    _drm: drm,
                })
            }
        }
    };
}

entry_points! {
    vaInitialize: fn(Display, *mut c_int, *mut c_int) -> Status;
    vaTerminate: fn(Display) -> Status;
    vaErrorStr: fn(Status) -> *const c_char;
    vaQueryVendorString: fn(Display) -> *const c_char;
    vaMaxNumEntrypoints: fn(Display) -> c_int;
    vaQueryConfigEntrypoints: fn(Display, c_int, *mut c_int, *mut c_int) -> Status;
    vaGetConfigAttributes: fn(Display, c_int, c_int, *mut ConfigAttrib, c_int) -> Status;
    vaCreateConfig: fn(Display, c_int, c_int, *mut ConfigAttrib, c_int, *mut Id) -> Status;
    vaDestroyConfig: fn(Display, Id) -> Status;
    vaCreateSurfaces: fn(Display, c_uint, c_uint, c_uint, *mut Id, c_uint, *mut c_void, c_uint) -> Status;
    vaDestroySurfaces: fn(Display, *mut Id, c_int) -> Status;
    vaCreateContext: fn(Display, Id, c_int, c_int, c_int, *mut Id, c_int, *mut Id) -> Status;
    vaDestroyContext: fn(Display, Id) -> Status;
    vaCreateBuffer: fn(Display, Id, c_int, c_uint, c_uint, *mut c_void, *mut Id) -> Status;
    vaDestroyBuffer: fn(Display, Id) -> Status;
    vaMapBuffer: fn(Display, Id, *mut *mut c_void) -> Status;
    vaUnmapBuffer: fn(Display, Id) -> Status;
    vaBeginPicture: fn(Display, Id, Id) -> Status;
    vaRenderPicture: fn(Display, Id, *mut Id, c_int) -> Status;
    vaEndPicture: fn(Display, Id) -> Status;
    vaSyncSurface: fn(Display, Id) -> Status;
    vaDeriveImage: fn(Display, Id, *mut Image) -> Status;
    vaCreateImage: fn(Display, *mut ImageFormat, c_int, c_int, *mut Image) -> Status;
    vaPutImage: fn(Display, Id, Id, c_int, c_int, c_uint, c_uint, c_int, c_int, c_uint, c_uint) -> Status;
    vaDestroyImage: fn(Display, Id) -> Status;
    vaSetErrorCallback: fn(Display, Option<MessageCallback>, *mut c_void) -> Option<MessageCallback>;
    vaSetInfoCallback: fn(Display, Option<MessageCallback>, *mut c_void) -> Option<MessageCallback>;
}

type MessageCallback = unsafe extern "C" fn(*mut c_void, *const c_char);

/// libva's messages go to the log at debug level, not to stderr: a host
/// without a VA-API driver should not print one complaint per start.
unsafe extern "C" fn message(_: *mut c_void, text: *const c_char) {
    if !text.is_null() {
        // SAFETY: libva passes a NUL-terminated message.
        let text = unsafe { CStr::from_ptr(text) };
        tracing::debug!("libva: {}", text.to_string_lossy().trim_end());
    }
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// An initialized VA display on a DRM render node.
struct Device {
    api: &'static Api,
    display: Display,
    _fd: OwnedFd,
    vendor: String,
}
// SAFETY: a VA display may be used from any thread, one call at a time;
// the encoder owning it is used by one thread at a time.
unsafe impl Send for Device {}

impl Device {
    fn check(&self, status: Status, what: &str) -> Result<()> {
        if status == SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr returns a static string for any status.
        let text = unsafe { CStr::from_ptr((self.api.vaErrorStr)(status)) };
        let error = anyhow::anyhow!("VA-API {what}: {} ({status})", text.to_string_lossy());
        Err(if UNSUPPORTED.contains(&status) {
            unusable(error)
        } else {
            error
        })
    }

    /// The render nodes in order, `$LATCH_VAAPI_DEVICE` first if set.
    fn nodes() -> Vec<std::path::PathBuf> {
        let mut nodes: Vec<std::path::PathBuf> = std::env::var_os("LATCH_VAAPI_DEVICE")
            .map(Into::into)
            .into_iter()
            .collect();
        nodes.extend((128..136).map(|n| format!("/dev/dri/renderD{n}").into()));
        nodes
    }

    /// The driver on `node`, initialized; `None` when there is no such
    /// node. A node this user may not open (not in the `render` group), or
    /// whose driver does not initialize, will not open later either.
    fn at(node: &std::path::Path) -> Result<Option<Self>> {
        let api = api()?;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(unusable(anyhow::anyhow!("{}: {e}", node.display()))),
        };
        let fd = OwnedFd::from(file);
        // SAFETY: `fd` stays open for as long as the display lives.
        let display = unsafe { (api.get_display_drm)(fd.as_raw_fd()) };
        if display.is_null() {
            return Ok(None);
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: a display just returned by vaGetDisplayDRM.
        let status = unsafe {
            (api.vaSetErrorCallback)(display, Some(message), null_mut());
            (api.vaSetInfoCallback)(display, Some(message), null_mut());
            (api.vaInitialize)(display, &mut major, &mut minor)
        };
        if status != SUCCESS {
            // SAFETY: as above; terminating also frees the display.
            unsafe { (api.vaTerminate)(display) };
            return Err(unusable(anyhow::anyhow!(
                "{}: no VA-API driver (status {status})",
                node.display()
            )));
        }
        // SAFETY: initialized; the vendor string lives with the display.
        let vendor = unsafe {
            let s = (api.vaQueryVendorString)(display);
            if s.is_null() {
                String::new()
            } else {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            }
        };
        Ok(Some(Self {
            api,
            display,
            _fd: fd,
            vendor,
        }))
    }

    /// The encode entry point for `profile`, low-power first.
    fn entrypoint(&self, profile: c_int) -> Option<c_int> {
        // SAFETY: the list has room for the driver's maximum.
        unsafe {
            let max = (self.api.vaMaxNumEntrypoints)(self.display).max(0) as usize;
            let mut list = vec![0; max.max(1)];
            let mut count = 0;
            if (self.api.vaQueryConfigEntrypoints)(
                self.display,
                profile,
                list.as_mut_ptr(),
                &mut count,
            ) != SUCCESS
            {
                return None;
            }
            list.truncate(count.max(0) as usize);
            [ENTRYPOINT_ENC_SLICE_LP, ENTRYPOINT_ENC_SLICE]
                .into_iter()
                .find(|e| list.contains(e))
        }
    }

    fn attribute(&self, profile: c_int, entrypoint: c_int, kind: c_int) -> Option<u32> {
        let mut attrib = ConfigAttrib { kind, value: 0 };
        // SAFETY: one attribute, in and out.
        let status = unsafe {
            (self.api.vaGetConfigAttributes)(self.display, profile, entrypoint, &mut attrib, 1)
        };
        (status == SUCCESS && attrib.value != ATTRIB_NOT_SUPPORTED).then_some(attrib.value)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: initialized in `open`; nothing uses it afterwards.
        unsafe { (self.api.vaTerminate)(self.display) };
    }
}

/// HEVC coding-tree sizes, as log2.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Blocks {
    ctb: u8,
    min_cb: u8,
    min_tb: u8,
    max_tb: u8,
    depth_inter: u8,
    depth_intra: u8,
}

impl Blocks {
    /// Pixels a side of the smallest coding block, which a coded picture's
    /// sides are whole multiples of (8 at the least).
    fn min_cb_size(&self) -> u32 {
        1 << self.min_cb.max(3)
    }

    /// The driver's own report when it gives one, else what drivers take:
    /// 64-sample trees on low-power hardware, 32 otherwise.
    fn from_attribute(value: Option<u32>, low_power: bool) -> Self {
        match value {
            Some(v) => {
                let field = |shift: u32| ((v >> shift) & 3) as u8;
                Self {
                    ctb: field(0) + 3,
                    min_cb: field(4) + 3,
                    max_tb: field(6) + 2,
                    min_tb: field(8) + 2,
                    depth_inter: field(10),
                    depth_intra: field(14),
                }
            }
            None => Self {
                ctb: if low_power { 6 } else { 5 },
                min_cb: 3,
                min_tb: 2,
                max_tb: 5,
                depth_inter: 3,
                depth_intra: 3,
            },
        }
    }
}

pub struct Vaapi {
    device: Device,
    codec: Codec,
    config: Id,
    context: Id,
    input: Id,
    recon: [Id; 2],
    coded: Id,
    /// For drivers that cannot map a surface: an image put into it.
    image: Option<Image>,
    width: u32,
    height: u32,
    fps: u32,
    kbps: u32,
    rate_changed: bool,
    blocks: Blocks,
    caps: Caps,
    /// Frames since the last IDR, which recon surface is next, whether
    /// the other holds a reference.
    since_idr: u32,
    current: usize,
    reference: bool,
    idr_id: u16,
}

impl Vaapi {
    /// On the first render node whose driver opens this encoder. The
    /// failure is [`Unusable`](super::hardware::Unusable) only when it is
    /// on every node that answered.
    pub fn open(codec: Codec, width: u32, height: u32, config: &VideoConfig) -> Result<Self> {
        let mut errors = Vec::new();
        for node in Device::nodes() {
            let opened = Device::at(&node).and_then(|device| match device {
                Some(device) => Self::open_on(device, codec, width, height, config)
                    .map(Some)
                    .map_err(|e| e.context(node.display().to_string())),
                None => Ok(None),
            });
            match opened {
                Ok(Some(this)) => return Ok(this),
                Ok(None) => {}
                Err(e) => errors.push(e),
            }
        }
        if errors.is_empty() {
            return Err(unusable(anyhow::anyhow!("no DRM render node")));
        }
        // One that may open another time says more than one that will not.
        let at = errors.iter().position(|e| !is_unusable(e)).unwrap_or(0);
        Err(errors.swap_remove(at))
    }

    fn open_on(
        device: Device,
        codec: Codec,
        width: u32,
        height: u32,
        config: &VideoConfig,
    ) -> Result<Self> {
        let no = |what: String| Err(unusable(anyhow::anyhow!(what)));
        let profile = match codec {
            Codec::H264Baseline => PROFILE_H264_CONSTRAINED_BASELINE,
            Codec::H264High => PROFILE_H264_HIGH,
            Codec::Hevc => PROFILE_HEVC_MAIN,
            Codec::Av1 => return no("Latch does not encode AV1 through VA-API".into()),
        };
        let Some(entrypoint) = device.entrypoint(profile) else {
            return no(format!("{} encodes no {}", device.vendor, codec.name()));
        };
        let rt = device
            .attribute(profile, entrypoint, ATTRIB_RT_FORMAT)
            .unwrap_or(0);
        if rt & RT_FORMAT_YUV420 == 0 {
            return no("VA-API driver encodes no 4:2:0".into());
        }
        let rc = device
            .attribute(profile, entrypoint, ATTRIB_RATE_CONTROL)
            .unwrap_or(0);
        let rc = if rc & RC_CBR != 0 {
            RC_CBR
        } else if rc & RC_VBR != 0 {
            RC_VBR
        } else {
            return no(format!(
                "VA-API driver has no bitrate control for {}",
                codec.name()
            ));
        };
        if let Some(refs) = device.attribute(profile, entrypoint, ATTRIB_ENC_MAX_REF_FRAMES) {
            if refs & 0xffff < 1 {
                return no("VA-API driver allows no P-frame reference".into());
            }
        }
        let blocks = Blocks::from_attribute(
            device.attribute(profile, entrypoint, ATTRIB_ENC_HEVC_BLOCK_SIZES),
            entrypoint == ENTRYPOINT_ENC_SLICE_LP,
        );
        let side = |kind, unknown| {
            device
                .attribute(profile, entrypoint, kind)
                .filter(|&v| v > 0)
                .unwrap_or(unknown)
        };
        let caps = Caps {
            max_width: side(ATTRIB_MAX_PICTURE_WIDTH, Caps::UNKNOWN.max_width),
            max_height: side(ATTRIB_MAX_PICTURE_HEIGHT, Caps::UNKNOWN.max_height),
            align: if codec == Codec::Hevc {
                blocks.min_cb_size()
            } else {
                Caps::UNKNOWN.align
            },
            ..Caps::UNKNOWN
        };
        // The size's fault, not the driver's: another size opens.
        ensure!(
            caps.fits(width, height),
            "VA-API {} takes up to {} × {} in multiples of {}, not {width} × {height}",
            codec.name(),
            caps.max_width,
            caps.max_height,
            caps.align
        );
        let api = device.api;
        let dpy = device.display;
        let mut attribs = [
            ConfigAttrib {
                kind: ATTRIB_RT_FORMAT,
                value: RT_FORMAT_YUV420,
            },
            ConfigAttrib {
                kind: ATTRIB_RATE_CONTROL,
                value: rc,
            },
        ];
        let mut this = Self {
            device,
            codec,
            config: INVALID_ID,
            context: INVALID_ID,
            input: INVALID_ID,
            recon: [INVALID_ID; 2],
            coded: INVALID_ID,
            image: None,
            width,
            height,
            fps: config.fps.max(1),
            kbps: config.bitrate_kbps.max(1),
            rate_changed: true,
            blocks,
            caps,
            since_idr: 0,
            current: 0,
            reference: false,
            idr_id: 0,
        };
        // SAFETY: out-parameters sized as libva says; every id is
        // destroyed in Drop.
        unsafe {
            let d = &this.device;
            d.check(
                (api.vaCreateConfig)(
                    dpy,
                    profile,
                    entrypoint,
                    attribs.as_mut_ptr(),
                    2,
                    &mut this.config,
                ),
                "configuration",
            )?;
            let mut surfaces = [INVALID_ID; 3];
            d.check(
                (api.vaCreateSurfaces)(
                    dpy,
                    RT_FORMAT_YUV420,
                    width,
                    height,
                    surfaces.as_mut_ptr(),
                    3,
                    null_mut(),
                    0,
                ),
                "surfaces",
            )?;
            this.input = surfaces[0];
            this.recon = [surfaces[1], surfaces[2]];
            let d = &this.device;
            let (w, h) = this.aligned();
            d.check(
                (api.vaCreateContext)(
                    dpy,
                    this.config,
                    w as c_int,
                    h as c_int,
                    1,
                    this.recon.as_mut_ptr(),
                    2,
                    &mut this.context,
                ),
                "context",
            )?;
            let size = (width * height * 3 + (1 << 16)) as c_uint;
            d.check(
                (api.vaCreateBuffer)(
                    dpy,
                    this.context,
                    CODED_BUFFER,
                    size,
                    1,
                    null_mut(),
                    &mut this.coded,
                ),
                "coded buffer",
            )?;
        }
        Ok(this)
    }

    /// What this encoder takes, as its driver said.
    pub fn caps(&self) -> Caps {
        self.caps
    }

    /// The coded size: whole macroblocks for H.264, minimum coding blocks
    /// for HEVC.
    fn aligned(&self) -> (u32, u32) {
        match self.codec {
            Codec::Hevc => (self.width, self.height),
            _ => (self.width.div_ceil(16) * 16, self.height.div_ceil(16) * 16),
        }
    }

    fn upload(&mut self, picture: &Yuv420) -> Result<()> {
        let api = self.device.api;
        let dpy = self.device.display;
        // SAFETY: the image's mapping covers `data_size` bytes, which
        // holds both planes at the pitches and offsets libva reports.
        unsafe {
            let derived = self.image.is_none().then(|| {
                let mut image = Image::default();
                ((api.vaDeriveImage)(dpy, self.input, &mut image) == SUCCESS).then_some(image)
            });
            let image = match derived.flatten() {
                Some(image) if image.format.fourcc == FOURCC_NV12 => image,
                derived => {
                    if let Some(image) = derived {
                        (api.vaDestroyImage)(dpy, image.image_id);
                    }
                    if self.image.is_none() {
                        let mut format = ImageFormat {
                            fourcc: FOURCC_NV12,
                            byte_order: 1,
                            bits_per_pixel: 12,
                            ..Default::default()
                        };
                        let mut image = Image::default();
                        self.device.check(
                            (api.vaCreateImage)(
                                dpy,
                                &mut format,
                                self.width as c_int,
                                self.height as c_int,
                                &mut image,
                            ),
                            "image",
                        )?;
                        self.image = Some(image);
                    }
                    self.image.expect("just made")
                }
            };
            let mut data = null_mut();
            self.device
                .check((api.vaMapBuffer)(dpy, image.buf, &mut data), "image map")?;
            let bytes = std::slice::from_raw_parts_mut(data.cast::<u8>(), image.data_size as usize);
            let (y_at, uv_at) = (image.offsets[0] as usize, image.offsets[1] as usize);
            let (y_pitch, uv_pitch) = (image.pitches[0] as usize, image.pitches[1] as usize);
            let h = self.height as usize;
            let fits = y_at + y_pitch * h <= uv_at
                && uv_at + uv_pitch * h.div_ceil(2) <= bytes.len()
                && y_pitch >= self.width as usize
                && uv_pitch >= self.width as usize;
            if fits {
                let (y, uv) = bytes.split_at_mut(uv_at);
                nv12::write(picture, &mut y[y_at..], y_pitch, uv, uv_pitch);
            }
            (api.vaUnmapBuffer)(dpy, image.buf);
            let put = if self.image.is_some() {
                (api.vaPutImage)(
                    dpy,
                    self.input,
                    image.image_id,
                    0,
                    0,
                    self.width,
                    self.height,
                    0,
                    0,
                    self.width,
                    self.height,
                )
            } else {
                (api.vaDestroyImage)(dpy, image.image_id);
                SUCCESS
            };
            ensure!(fits, "VA-API image layout does not fit the picture");
            self.device.check(put, "image upload")
        }
    }

    fn buffer<T>(&self, kind: c_int, value: &T) -> Result<Id> {
        let mut id = INVALID_ID;
        // SAFETY: libva copies `size_of::<T>()` bytes from `value`.
        let status = unsafe {
            (self.device.api.vaCreateBuffer)(
                self.device.display,
                self.context,
                kind,
                std::mem::size_of::<T>() as c_uint,
                1,
                (value as *const T).cast_mut().cast(),
                &mut id,
            )
        };
        self.device.check(status, "parameter buffer")?;
        Ok(id)
    }

    /// A miscellaneous parameter buffer: its type, then `value`.
    fn misc<T>(&self, kind: u32, value: &T) -> Result<Id> {
        let mut bytes = vec![0u8; 4 + std::mem::size_of::<T>()];
        bytes[..4].copy_from_slice(&kind.to_ne_bytes());
        // SAFETY: plain data copied into a buffer of its size.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (value as *const T).cast::<u8>(),
                bytes[4..].as_mut_ptr(),
                std::mem::size_of::<T>(),
            );
        }
        let mut id = INVALID_ID;
        // SAFETY: libva copies `bytes`.
        let status = unsafe {
            (self.device.api.vaCreateBuffer)(
                self.device.display,
                self.context,
                MISC_PARAMETER_BUFFER,
                bytes.len() as c_uint,
                1,
                bytes.as_mut_ptr().cast(),
                &mut id,
            )
        };
        self.device.check(status, "parameter buffer")?;
        Ok(id)
    }

    fn rate_buffers(&self, buffers: &mut Vec<Id>) -> Result<()> {
        let bps = self.kbps.saturating_mul(1000);
        let mut rc = zeroed::<RateControl>();
        rc.bits_per_second = bps;
        rc.target_percentage = 100;
        rc.window_size = 1000;
        rc.rc_flags = RC_DISABLE_FRAME_SKIP;
        buffers.push(self.misc(MISC_RATE_CONTROL, &*rc)?);
        let rate = FrameRate {
            framerate: self.fps | (1 << 16),
            framerate_flags: 0,
            va_reserved: [0; 4],
        };
        buffers.push(self.misc(MISC_FRAME_RATE, &rate)?);
        let size = bps / self.fps;
        let hrd = Hrd {
            initial_buffer_fullness: size,
            buffer_size: size,
            va_reserved: [0; 4],
        };
        buffers.push(self.misc(MISC_HRD, &hrd)?);
        Ok(())
    }

    fn h264_buffers(&self, idr: bool, buffers: &mut Vec<Id>) -> Result<()> {
        let high = self.codec == Codec::H264High;
        let (mbs_w, mbs_h) = (self.width.div_ceil(16), self.height.div_ceil(16));
        let frame_num = (self.since_idr % 256) as u16;
        let poc = (self.since_idr * 2) as i32;
        if idr {
            let mut seq = zeroed::<SequenceH264>();
            seq.level_idc = h264_level(self.codec, self.width, self.height, self.fps, self.kbps);
            seq.intra_period = 1 << 15;
            seq.intra_idr_period = 1 << 15;
            seq.ip_period = 1;
            seq.bits_per_second = self.kbps.saturating_mul(1000);
            seq.max_num_ref_frames = 1;
            seq.picture_width_in_mbs = mbs_w as u16;
            seq.picture_height_in_mbs = mbs_h as u16;
            // 4:2:0, frames only, direct 8x8 inference, 8-bit frame_num,
            // picture order from frame_num (type 2: no reordering).
            seq.seq_fields = 1 | (1 << 2) | (1 << 5) | (4 << 6) | (2 << 10) | (4 << 12);
            let crop_right = (mbs_w * 16 - self.width) / 2;
            let crop_bottom = (mbs_h * 16 - self.height) / 2;
            seq.frame_cropping_flag = u8::from(crop_right > 0 || crop_bottom > 0);
            seq.frame_crop_right_offset = crop_right;
            seq.frame_crop_bottom_offset = crop_bottom;
            seq.vui_parameters_present_flag = 1;
            // Timing, bitstream restriction (no reordering), 16-bit motion
            // vector lengths, vectors over picture boundaries.
            seq.vui_fields = (1 << 1) | (1 << 2) | (15 << 3) | (15 << 8) | (1 << 15);
            seq.num_units_in_tick = 1;
            seq.time_scale = self.fps * 2;
            buffers.push(self.buffer(SEQUENCE_PARAMETER_BUFFER, &*seq)?);
        }
        let recon = self.recon[self.current];
        let previous = self.recon[1 - self.current];
        let mut pic = zeroed::<PictureParameterH264>();
        pic.curr_pic = PictureH264 {
            picture_id: recon,
            frame_idx: frame_num.into(),
            flags: 0,
            top_field_order_cnt: poc,
            bottom_field_order_cnt: poc,
            va_reserved: [0; 4],
        };
        pic.reference_frames = [NO_PICTURE_H264; 16];
        let reference = PictureH264 {
            picture_id: previous,
            frame_idx: (frame_num.wrapping_sub(1) % 256).into(),
            flags: H264_SHORT_TERM_REFERENCE,
            top_field_order_cnt: poc - 2,
            bottom_field_order_cnt: poc - 2,
            va_reserved: [0; 4],
        };
        if !idr {
            pic.reference_frames[0] = reference;
        }
        pic.coded_buf = self.coded;
        pic.frame_num = frame_num;
        pic.pic_init_qp = 26;
        // IDR, reference, CABAC and 8x8 transforms for High, deblocking
        // control present.
        pic.pic_fields =
            u32::from(idr) | (1 << 1) | (u32::from(high) << 3) | (u32::from(high) << 8) | (1 << 9);
        buffers.push(self.buffer(PICTURE_PARAMETER_BUFFER, &*pic)?);
        let mut slice = zeroed::<SliceH264>();
        slice.num_macroblocks = mbs_w * mbs_h;
        slice.macroblock_info = INVALID_ID;
        slice.slice_type = if idr { 2 } else { 0 };
        slice.idr_pic_id = self.idr_id;
        slice.ref_pic_list0 = [NO_PICTURE_H264; 32];
        slice.ref_pic_list1 = [NO_PICTURE_H264; 32];
        if !idr {
            slice.ref_pic_list0[0] = reference;
        }
        buffers.push(self.buffer(SLICE_PARAMETER_BUFFER, &*slice)?);
        Ok(())
    }

    fn hevc_buffers(&self, idr: bool, buffers: &mut Vec<Id>) -> Result<()> {
        let b = self.blocks;
        let poc = self.since_idr as i32;
        if idr {
            let mut seq = zeroed::<SequenceHevc>();
            seq.general_profile_idc = 1;
            let (level, high_tier) = hevc_level(self.width, self.height, self.fps, self.kbps);
            seq.general_level_idc = level;
            seq.general_tier_flag = u8::from(high_tier);
            seq.intra_period = 1 << 15;
            seq.intra_idr_period = 1 << 15;
            seq.ip_period = 1;
            seq.bits_per_second = self.kbps.saturating_mul(1000);
            seq.pic_width_in_luma_samples = self.width as u16;
            seq.pic_height_in_luma_samples = self.height as u16;
            // 4:2:0 8-bit, strong intra smoothing, temporal motion vector
            // prediction, low delay. AMP, SAO and PCM off: not every
            // low-power encoder has them.
            seq.seq_fields = 1 | (1 << 10) | (1 << 15) | (1 << 16);
            seq.log2_min_luma_coding_block_size_minus3 = b.min_cb - 3;
            seq.log2_diff_max_min_luma_coding_block_size = b.ctb - b.min_cb;
            seq.log2_min_transform_block_size_minus2 = b.min_tb - 2;
            seq.log2_diff_max_min_transform_block_size = b.max_tb - b.min_tb;
            seq.max_transform_hierarchy_depth_inter = b.depth_inter;
            seq.max_transform_hierarchy_depth_intra = b.depth_intra;
            seq.vui_parameters_present_flag = 1;
            // Timing, bitstream restriction, vectors over boundaries,
            // restricted reference lists, 16-bit motion vector lengths.
            seq.vui_fields = (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7) | (15 << 8) | (15 << 13);
            seq.vui_num_units_in_tick = 1;
            seq.vui_time_scale = self.fps;
            seq.max_bits_per_min_cu_denom = 1;
            buffers.push(self.buffer(SEQUENCE_PARAMETER_BUFFER, &*seq)?);
        }
        let recon = self.recon[self.current];
        let previous = self.recon[1 - self.current];
        // In the current picture's short-term set, before it in output
        // order, as FFmpeg marks it: drivers that build the reference
        // picture set from these flags see the one reference there is.
        let reference = PictureHevc {
            picture_id: previous,
            pic_order_cnt: poc - 1,
            flags: HEVC_RPS_ST_CURR_BEFORE,
            va_reserved: [0; 4],
        };
        let mut pic = zeroed::<PictureParameterHevc>();
        pic.decoded_curr_pic = PictureHevc {
            picture_id: recon,
            pic_order_cnt: poc,
            flags: 0,
            va_reserved: [0; 4],
        };
        pic.reference_frames = [NO_PICTURE_HEVC; 15];
        if !idr {
            pic.reference_frames[0] = reference;
        }
        pic.coded_buf = self.coded;
        pic.collocated_ref_pic_index = if idr { 0xff } else { 0 };
        pic.pic_init_qp = 30;
        pic.nal_unit_type = if idr { 19 } else { 1 };
        // IDR, coding type (1 I, 2 P), reference, CU QP deltas for rate
        // control, loop filter across slices.
        pic.pic_fields =
            u32::from(idr) | (if idr { 1 } else { 2 } << 1) | (1 << 4) | (1 << 9) | (1 << 16);
        buffers.push(self.buffer(PICTURE_PARAMETER_BUFFER, &*pic)?);
        let ctb = 1u32 << b.ctb;
        let mut slice = zeroed::<SliceHevc>();
        slice.num_ctu_in_slice = self.width.div_ceil(ctb) * self.height.div_ceil(ctb);
        slice.slice_type = if idr { 2 } else { 1 };
        slice.ref_pic_list0 = [NO_PICTURE_HEVC; 15];
        slice.ref_pic_list1 = [NO_PICTURE_HEVC; 15];
        if !idr {
            slice.ref_pic_list0[0] = reference;
        }
        slice.max_num_merge_cand = 5;
        // Last slice, temporal MVP, loop filter across slices, collocated
        // from list 0.
        slice.slice_fields = 1 | (1 << 4) | (1 << 12) | (1 << 13);
        buffers.push(self.buffer(SLICE_PARAMETER_BUFFER, &*slice)?);
        Ok(())
    }

    fn render(&mut self, idr: bool) -> Result<()> {
        let mut buffers = Vec::with_capacity(6);
        let built = (|| {
            if idr || self.rate_changed {
                self.rate_buffers(&mut buffers)?;
            }
            match self.codec {
                Codec::Hevc => self.hevc_buffers(idr, &mut buffers),
                _ => self.h264_buffers(idr, &mut buffers),
            }
        })();
        let api = self.device.api;
        let dpy = self.device.display;
        // SAFETY: the context, surface and buffers are this encoder's;
        // buffers are destroyed after the picture ends, as libva 2 asks.
        let result = built.and_then(|()| unsafe {
            let d = &self.device;
            d.check(
                (api.vaBeginPicture)(dpy, self.context, self.input),
                "begin picture",
            )?;
            let rendered = (api.vaRenderPicture)(
                dpy,
                self.context,
                buffers.as_mut_ptr(),
                buffers.len() as c_int,
            );
            let ended = (api.vaEndPicture)(dpy, self.context);
            d.check(rendered, "render picture")?;
            d.check(ended, "end picture")?;
            d.check((api.vaSyncSurface)(dpy, self.input), "sync")
        });
        for id in buffers {
            // SAFETY: created above, used by nothing after the sync.
            unsafe { (api.vaDestroyBuffer)(dpy, id) };
        }
        result
    }

    fn take_output(&self, out: &mut Vec<u8>) -> Result<()> {
        let api = self.device.api;
        let dpy = self.device.display;
        // SAFETY: a mapped coded buffer is a list of segments, each with
        // `size` valid bytes, until it is unmapped.
        unsafe {
            let mut segment = null_mut();
            self.device.check(
                (api.vaMapBuffer)(dpy, self.coded, &mut segment),
                "coded buffer map",
            )?;
            let mut segment = segment.cast::<CodedBufferSegment>();
            while !segment.is_null() {
                let s = &*segment;
                if !s.buf.is_null() {
                    out.extend_from_slice(std::slice::from_raw_parts(
                        s.buf.cast::<u8>(),
                        s.size as usize,
                    ));
                }
                segment = s.next;
            }
            (api.vaUnmapBuffer)(dpy, self.coded);
        }
        Ok(())
    }
}

impl PictureEncoder for Vaapi {
    fn name(&self) -> String {
        let vendor = self.device.vendor.split(" - ").next().unwrap_or_default();
        format!("VA-API {} ({vendor})", self.codec.name())
    }
    fn hardware(&self) -> bool {
        true
    }
    fn codec(&self) -> Codec {
        self.codec
    }

    fn encode(&mut self, picture: &Yuv420, keyframe: bool, out: &mut Vec<u8>) -> Result<bool> {
        ensure!(
            (picture.width, picture.height) == (self.width, self.height),
            "picture size differs from the VA-API session's"
        );
        let idr = keyframe || !self.reference || self.since_idr >= 1 << 15;
        if idr {
            self.since_idr = 0;
        }
        self.upload(picture)?;
        self.render(idr)?;
        self.rate_changed = false;
        self.take_output(out)?;
        if idr {
            self.idr_id = self.idr_id.wrapping_add(1);
        }
        self.since_idr += 1;
        self.current = 1 - self.current;
        self.reference = true;
        Ok(idr)
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.kbps = kbps.max(1);
        self.rate_changed = true;
    }
}

impl Drop for Vaapi {
    fn drop(&mut self) {
        let api = self.device.api;
        let dpy = self.device.display;
        // SAFETY: each id is this encoder's and destroyed once, before the
        // display terminates (Device drops after this).
        unsafe {
            if let Some(image) = self.image {
                (api.vaDestroyImage)(dpy, image.image_id);
            }
            if self.coded != INVALID_ID {
                (api.vaDestroyBuffer)(dpy, self.coded);
            }
            if self.context != INVALID_ID {
                (api.vaDestroyContext)(dpy, self.context);
            }
            let mut surfaces = [self.input, self.recon[0], self.recon[1]];
            if surfaces[0] != INVALID_ID {
                (api.vaDestroySurfaces)(dpy, surfaces.as_mut_ptr(), 3);
            }
            if self.config != INVALID_ID {
                (api.vaDestroyConfig)(dpy, self.config);
            }
        }
    }
}

/// The lowest H.264 level for this size, rate and bitrate, as
/// `level_idc`. A level allows MaxBR times 1250 bits a second for High
/// and 1000 for Constrained Baseline (cpbBrVclFactor).
fn h264_level(codec: Codec, width: u32, height: u32, fps: u32, kbps: u32) -> u8 {
    let factor = if codec == Codec::H264High { 1250 } else { 1000 };
    let mbs = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
    for (id, mbps, max_fs, max_br) in [
        (30, 40500, 1620, 10000),
        (31, 108000, 3600, 14000),
        (32, 216000, 5120, 20000),
        (40, 245760, 8192, 20000),
        (41, 245760, 8192, 50000),
        (42, 522240, 8704, 50000),
        (50, 589824, 22080, 135000),
        (51, 983040, 36864, 240000),
        (52, 2073600, 36864, 240000),
        (60, 4177920, 139264, 240000),
        (61, 8355840, 139264, 480000),
    ] {
        if mbs <= max_fs
            && mbs * u64::from(fps) <= mbps
            && u64::from(kbps) * 1000 <= max_br * factor
        {
            return id;
        }
    }
    62
}

/// The lowest HEVC level for this size, rate and bitrate, as
/// `general_level_idc` (30 × the level), and whether it is in the High
/// tier: Main where its bitrate allows, else High (levels 4 and up), whose
/// MaxBR is 2.5 to 4 times Main's.
fn hevc_level(width: u32, height: u32, fps: u32, kbps: u32) -> (u8, bool) {
    let samples = u64::from(width) * u64::from(height);
    let rate = samples * u64::from(fps);
    let kbps = u64::from(kbps);
    for (idc, max_ps, max_sr, main_br, high_br) in [
        (90, 552_960, 16_588_800, 6_000, 0),
        (93, 983_040, 33_177_600, 10_000, 0),
        (120, 2_228_224, 66_846_720, 12_000, 30_000),
        (123, 2_228_224, 133_693_440, 20_000, 50_000),
        (150, 8_912_896, 267_386_880, 25_000, 100_000),
        (153, 8_912_896, 534_773_760, 40_000, 160_000),
        (156, 8_912_896, 1_069_547_520, 60_000, 240_000),
        (180, 35_651_584, 1_069_547_520, 60_000, 240_000),
        (183, 35_651_584, 2_139_095_040, 120_000, 480_000),
        (186, 35_651_584, 4_278_190_080, 240_000, 800_000),
    ] {
        if samples > max_ps || rate > max_sr {
            continue;
        }
        if kbps <= main_br {
            return (idc, false);
        }
        if kbps <= high_br {
            return (idc, true);
        }
    }
    (186, true)
}

/// Whether a VA-API driver initializes on some render node.
#[cfg(test)]
pub fn opens() -> bool {
    Device::nodes()
        .iter()
        .any(|node| matches!(Device::at(node), Ok(Some(_))))
}

/// Whether libva loads at all. Cheap; [`super::hardware`] then opens a
/// real session per codec.
pub fn present() -> bool {
    api().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_size_rate_and_bitrate() {
        let high = |w, h, fps, kbps| h264_level(Codec::H264High, w, h, fps, kbps);
        assert_eq!(high(1920, 1080, 60, 20_000), 42);
        assert_eq!(high(1920, 1080, 30, 20_000), 40);
        assert_eq!(high(2560, 1440, 60, 20_000), 51);
        assert_eq!(high(3840, 2160, 60, 50_000), 52);
        assert_eq!(high(3840, 2160, 120, 50_000), 60);
        assert_eq!(high(3840, 2160, 240, 50_000), 61);
        // 60 Mb/s is within 4.2's 62.5 for High, past its 50 for Baseline.
        assert_eq!(high(1920, 1080, 60, 60_000), 42);
        assert_eq!(h264_level(Codec::H264Baseline, 1920, 1080, 60, 60_000), 50);
        assert_eq!(hevc_level(1920, 1080, 60, 20_000), (123, false));
        assert_eq!(hevc_level(3840, 2160, 60, 20_000), (153, false));
        assert_eq!(hevc_level(3840, 2160, 120, 20_000), (156, false));
        assert_eq!(hevc_level(7680, 4320, 60, 20_000), (183, false));
        // 1080p60 at 50 and 100 Mb/s: 4.1's High tier, then 5's.
        assert_eq!(hevc_level(1920, 1080, 60, 50_000), (123, true));
        assert_eq!(hevc_level(1920, 1080, 60, 100_000), (150, true));
        assert_eq!(hevc_level(3840, 2160, 60, 1_000_000), (186, true));
    }

    #[test]
    fn hevc_block_sizes_come_from_the_driver_when_it_says() {
        assert_eq!(Blocks::from_attribute(None, true).ctb, 6);
        assert_eq!(Blocks::from_attribute(None, false).ctb, 5);
        // 64 trees, 8 coding blocks, 32..4 transforms, depth 2 / 1.
        let v = 3 | (1 << 2) | (3 << 6) | (2 << 10) | (1 << 14);
        let b = Blocks::from_attribute(Some(v), false);
        assert_eq!(
            b,
            Blocks {
                ctb: 6,
                min_cb: 3,
                min_tb: 2,
                max_tb: 5,
                depth_inter: 2,
                depth_intra: 1
            }
        );
    }
}
