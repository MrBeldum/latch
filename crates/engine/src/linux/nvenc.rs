//! NVIDIA's video encoder (NVENC): `libnvidia-encode.so.1` and the CUDA
//! driver `libcuda.so.1`, both part of the NVIDIA driver and loaded when
//! first asked for, so a machine without them loads nothing and links
//! nothing. Structures follow `nvEncodeAPI.h` of Video Codec SDK 12.0
//! (the API every driver since 520 accepts, and the first with AV1), and
//! their sizes are checked against it at compile time.
//!
//! Pictures arrive as NV12 in an encoder-owned input buffer (converted by
//! the CPU, so every encoder sees the same BT.709 limited-range picture),
//! one at a time and synchronously: P1, ultra-low-latency tuning, CBR with
//! a one-frame VBV, no B-frames, an infinite GOP with IDRs on request, and
//! parameter sets before every IDR.

use super::hardware::{unusable, Caps};
use super::nv12;
use crate::software::PictureEncoder;
use crate::video::{Codec, VideoConfig};
use anyhow::{ensure, Context, Result};
use latch_codec::Yuv420;
use libloading::Library;
use std::ffi::{c_char, c_void, CStr};
use std::ptr::null_mut;
use std::sync::OnceLock;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Guid(u32, u16, u16, [u8; 8]);

const CODEC_H264: Guid = Guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
const CODEC_HEVC: Guid = Guid(
    0x790cdc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
const CODEC_AV1: Guid = Guid(
    0x0a352289,
    0x0aa7,
    0x4759,
    [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);
const PROFILE_H264_BASELINE: Guid = Guid(
    0x0727bcaa,
    0x78c4,
    0x4c83,
    [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
);
const PROFILE_H264_HIGH: Guid = Guid(
    0xe7cbc309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
const PROFILE_HEVC_MAIN: Guid = Guid(
    0xb514c39a,
    0xb55b,
    0x40fa,
    [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
const PROFILE_AV1_MAIN: Guid = Guid(
    0x5f2a39f5,
    0xf14e,
    0x4f95,
    [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97],
);
/// The fastest preset: the encoder must never be what limits the rate.
const PRESET_P1: Guid = Guid(
    0xfc0a8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);

const API_MAJOR: u32 = 12;
const API_MINOR: u32 = 0;
const API_VERSION: u32 = API_MAJOR | (API_MINOR << 24);
const fn struct_version(v: u32) -> u32 {
    API_VERSION | (v << 16) | (0x7 << 28)
}
const OPEN_SESSION_VER: u32 = struct_version(1);
const FUNCTION_LIST_VER: u32 = struct_version(2);
const CONFIG_VER: u32 = struct_version(8) | (1 << 31);
const INITIALIZE_VER: u32 = struct_version(5) | (1 << 31);
const RECONFIGURE_VER: u32 = struct_version(1) | (1 << 31);
const PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
const RC_PARAMS_VER: u32 = struct_version(1);
const CREATE_INPUT_VER: u32 = struct_version(1);
const CREATE_BITSTREAM_VER: u32 = struct_version(1);
const LOCK_INPUT_VER: u32 = struct_version(1);
const LOCK_BITSTREAM_VER: u32 = struct_version(2);
const PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);

const SUCCESS: i32 = 0;
/// Statuses that say this driver or GPU has no such encoder: no encode
/// device, an unsupported or missing device, an API version it does not
/// take, unimplemented. Out of memory (also what a GeForce card's session
/// limit gives), invalid or unsupported parameters (a size it does not
/// take) and the rest are for one open.
const UNSUPPORTED: [Status; 7] = [1, 2, 3, 4, 5, 15, 22];
const CAPS_PARAM_VER: u32 = struct_version(1);
const CAPS_WIDTH_MAX: u32 = 16;
const CAPS_HEIGHT_MAX: u32 = 17;
const CAPS_MB_NUM_MAX: u32 = 31;
const CAPS_MB_PER_SEC_MAX: u32 = 32;
const DEVICE_CUDA: u32 = 1;
const BUFFER_NV12: u32 = 0x1;
const TUNING_ULTRA_LOW_LATENCY: u32 = 3;
const RC_CBR: u32 = 2;
const INFINITE_GOP: u32 = 0xffff_ffff;
const PIC_STRUCT_FRAME: u32 = 1;
const PIC_TYPE_P: u32 = 0;
const PIC_TYPE_I: u32 = 2;
const PIC_TYPE_IDR: u32 = 3;
const PIC_FLAG_FORCEIDR: u32 = 0x2;
const PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
const H264_CABAC: u32 = 1;
const H264_CAVLC: u32 = 2;

type Ptr = *mut c_void;

#[repr(C)]
struct OpenSessionParams {
    version: u32,
    device_type: u32,
    device: Ptr,
    reserved: Ptr,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [Ptr; 64],
}

/// `NV_ENC_RC_PARAMS`.
#[repr(C)]
#[derive(Clone, Copy)]
struct RcParams {
    version: u32,
    rate_control_mode: u32,
    const_qp: [u32; 3],
    average_bit_rate: u32,
    max_bit_rate: u32,
    vbv_buffer_size: u32,
    vbv_initial_delay: u32,
    /// enableMinQP … strictGOPTarget, aqStrength; bit 9 is zeroReorderDelay.
    flags: u32,
    min_qp: [u32; 3],
    max_qp: [u32; 3],
    initial_rc_qp: [u32; 3],
    temporal_layer_idx_mask: u32,
    temporal_layer_qp: [u8; 8],
    target_quality: u8,
    target_quality_lsb: u8,
    lookahead_depth: u16,
    low_delay_key_frame_scale: u8,
    y_dc_qp_index_offset: i8,
    u_dc_qp_index_offset: i8,
    v_dc_qp_index_offset: i8,
    qp_map_mode: u32,
    multi_pass: u32,
    alpha_layer_bitrate_ratio: u32,
    cb_qp_index_offset: i8,
    cr_qp_index_offset: i8,
    reserved2: u16,
    reserved: [u32; 4],
}
const RC_ZERO_REORDER_DELAY: u32 = 1 << 9;

/// `NV_ENC_CONFIG_H264_VUI_PARAMETERS`, also HEVC's.
#[repr(C)]
#[derive(Clone, Copy)]
struct Vui {
    overscan_info_present_flag: u32,
    overscan_info: u32,
    video_signal_type_present_flag: u32,
    video_format: u32,
    video_full_range_flag: u32,
    colour_description_present_flag: u32,
    colour_primaries: u32,
    transfer_characteristics: u32,
    colour_matrix: u32,
    chroma_sample_location_flag: u32,
    chroma_sample_location_top: u32,
    chroma_sample_location_bot: u32,
    bitstream_restriction_flag: u32,
    timing_info_present_flag: u32,
    num_unit_in_ticks: u32,
    time_scale: u32,
    reserved: [u32; 12],
}

impl Vui {
    /// BT.709 limited range, as `latch_codec::color` converts.
    fn bt709(&mut self) {
        self.video_signal_type_present_flag = 1;
        self.video_format = 5; // unspecified
        self.video_full_range_flag = 0;
        self.colour_description_present_flag = 1;
        self.colour_primaries = 1;
        self.transfer_characteristics = 1;
        self.colour_matrix = 1;
    }
}

/// The leading fields of `NV_ENC_CONFIG_H264` (1792 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct H264Config {
    /// enableTemporalSVC …; bit 12 is repeatSPSPPS.
    flags: u32,
    level: u32,
    idr_period: u32,
    separate_colour_plane_flag: u32,
    disable_deblocking_filter_idc: u32,
    num_temporal_layers: u32,
    sps_id: u32,
    pps_id: u32,
    adaptive_transform_mode: u32,
    fmo_mode: u32,
    bdirect_mode: u32,
    entropy_coding_mode: u32,
    stereo_mode: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    vui: Vui,
    ltr_num_frames: u32,
    ltr_trust_mode: u32,
    chroma_format_idc: u32,
}
const H264_REPEAT_SPSPPS: u32 = 1 << 12;

/// The leading fields of `NV_ENC_CONFIG_HEVC` (1560 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct HevcConfig {
    level: u32,
    tier: u32,
    min_cu_size: u32,
    max_cu_size: u32,
    /// useConstrainedIntraPred …; bit 7 repeatSPSPPS, bits 9–10
    /// chromaFormatIDC, 11–13 pixelBitDepthMinus8.
    flags: u32,
    idr_period: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames_in_dpb: u32,
    ltr_num_frames: u32,
    vps_id: u32,
    sps_id: u32,
    pps_id: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    max_temporal_layers_minus1: u32,
    vui: Vui,
}
const HEVC_REPEAT_SPSPPS: u32 = 1 << 7;
const HEVC_CHROMA_FORMAT: u32 = 3 << 9;
const HEVC_BIT_DEPTH: u32 = 7 << 11;

/// The leading fields of `NV_ENC_CONFIG_AV1` (1552 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct Av1Config {
    level: u32,
    tier: u32,
    min_part_size: u32,
    max_part_size: u32,
    /// bit 0 outputAnnexBFormat, 5 repeatSeqHdr, 7–8 chromaFormatIDC,
    /// 12–14 inputPixelBitDepthMinus8, 15–17 pixelBitDepthMinus8.
    flags: u32,
    idr_period: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames_in_dpb: u32,
    num_tile_columns: u32,
    num_tile_rows: u32,
    tile_widths: Ptr,
    tile_heights: Ptr,
    max_temporal_layers_minus1: u32,
    color_primaries: u32,
    transfer_characteristics: u32,
    matrix_coefficients: u32,
    color_range: u32,
    chroma_sample_position: u32,
}
const AV1_ANNEX_B: u32 = 1;
const AV1_REPEAT_SEQ_HDR: u32 = 1 << 5;
const AV1_CHROMA_FORMAT: u32 = 3 << 7;
const AV1_BIT_DEPTHS: u32 = (7 << 12) | (7 << 15);

/// `NV_ENC_CODEC_CONFIG`: the union of the codecs' configurations.
#[repr(C)]
#[derive(Clone, Copy)]
union CodecConfig {
    h264: H264Config,
    hevc: HevcConfig,
    av1: Av1Config,
    size: [Ptr; 224],
}

/// `NV_ENC_CONFIG`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Config {
    version: u32,
    profile_guid: Guid,
    gop_length: u32,
    frame_interval_p: i32,
    mono_chrome_encoding: u32,
    frame_field_mode: u32,
    mv_precision: u32,
    rc: RcParams,
    codec: CodecConfig,
    reserved: [u32; 278],
    reserved2: [Ptr; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MeHintCounts([u32; 4]);

/// `NV_ENC_INITIALIZE_PARAMS`.
#[repr(C)]
#[derive(Clone, Copy)]
struct InitializeParams {
    version: u32,
    encode_guid: Guid,
    preset_guid: Guid,
    encode_width: u32,
    encode_height: u32,
    dar_width: u32,
    dar_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    enable_encode_async: u32,
    enable_ptd: u32,
    flags: u32,
    priv_data_size: u32,
    priv_data: Ptr,
    encode_config: *mut Config,
    max_encode_width: u32,
    max_encode_height: u32,
    max_me_hint_counts_per_block: [MeHintCounts; 2],
    tuning_info: u32,
    buffer_format: u32,
    reserved: [u32; 287],
    reserved2: [Ptr; 64],
}

#[repr(C)]
struct ReconfigureParams {
    version: u32,
    reinit: InitializeParams,
    /// bit 0 resetEncoder, bit 1 forceIDR.
    flags: u32,
}

#[repr(C)]
struct PresetConfig {
    version: u32,
    config: Config,
    reserved1: [u32; 255],
    reserved2: [Ptr; 64],
}

#[repr(C)]
struct CreateInputBuffer {
    version: u32,
    width: u32,
    height: u32,
    memory_heap: u32,
    buffer_fmt: u32,
    reserved: u32,
    input_buffer: Ptr,
    sys_mem_buffer: Ptr,
    reserved1: [u32; 57],
    reserved2: [Ptr; 63],
}

#[repr(C)]
struct CreateBitstreamBuffer {
    version: u32,
    size: u32,
    memory_heap: u32,
    reserved: u32,
    bitstream_buffer: Ptr,
    bitstream_buffer_ptr: Ptr,
    reserved1: [u32; 58],
    reserved2: [Ptr; 64],
}

#[repr(C)]
struct LockInputBuffer {
    version: u32,
    flags: u32,
    input_buffer: Ptr,
    buffer_data_ptr: Ptr,
    pitch: u32,
    reserved1: [u32; 251],
    reserved2: [Ptr; 64],
}

#[repr(C)]
struct LockBitstream {
    version: u32,
    flags: u32,
    output_bitstream: Ptr,
    slice_offsets: Ptr,
    frame_idx: u32,
    hw_encode_status: u32,
    num_slices: u32,
    bitstream_size_in_bytes: u32,
    output_time_stamp: u64,
    output_duration: u64,
    bitstream_buffer_ptr: Ptr,
    picture_type: u32,
    picture_struct: u32,
    frame_avg_qp: u32,
    frame_satd: u32,
    ltr_frame_idx: u32,
    ltr_frame_bitmap: u32,
    temporal_id: u32,
    reserved: [u32; 12],
    intra_mb_count: u32,
    inter_mb_count: u32,
    average_mvx: i32,
    average_mvy: i32,
    alpha_layer_size_in_bytes: u32,
    reserved1: [u32; 218],
    reserved2: [Ptr; 64],
}

/// `NV_ENC_PIC_PARAMS`; everything after `picture_type` stays zero.
#[repr(C)]
struct PicParams {
    version: u32,
    input_width: u32,
    input_height: u32,
    input_pitch: u32,
    encode_pic_flags: u32,
    frame_idx: u32,
    input_time_stamp: u64,
    input_duration: u64,
    input_buffer: Ptr,
    output_bitstream: Ptr,
    completion_event: Ptr,
    buffer_fmt: u32,
    picture_struct: u32,
    picture_type: u32,
    rest: [u32; 821],
}

type Status = i32;

/// `NV_ENC_CAPS_PARAM`.
#[repr(C)]
struct CapsParam {
    version: u32,
    caps_to_query: u32,
    reserved: [u32; 62],
}
type Encoder = Ptr;

/// `NV_ENCODE_API_FUNCTION_LIST`, in the header's order.
#[repr(C)]
struct FunctionList {
    version: u32,
    reserved: u32,
    open_encode_session: Ptr,
    get_encode_guid_count: Option<unsafe extern "C" fn(Encoder, *mut u32) -> Status>,
    get_encode_profile_guid_count: Ptr,
    get_encode_profile_guids: Ptr,
    get_encode_guids: Option<unsafe extern "C" fn(Encoder, *mut Guid, u32, *mut u32) -> Status>,
    get_input_format_count: Ptr,
    get_input_formats: Ptr,
    get_encode_caps:
        Option<unsafe extern "C" fn(Encoder, Guid, *mut CapsParam, *mut i32) -> Status>,
    get_encode_preset_count: Ptr,
    get_encode_preset_guids: Ptr,
    get_encode_preset_config: Ptr,
    initialize_encoder: Option<unsafe extern "C" fn(Encoder, *mut InitializeParams) -> Status>,
    create_input_buffer: Option<unsafe extern "C" fn(Encoder, *mut CreateInputBuffer) -> Status>,
    destroy_input_buffer: Option<unsafe extern "C" fn(Encoder, Ptr) -> Status>,
    create_bitstream_buffer:
        Option<unsafe extern "C" fn(Encoder, *mut CreateBitstreamBuffer) -> Status>,
    destroy_bitstream_buffer: Option<unsafe extern "C" fn(Encoder, Ptr) -> Status>,
    encode_picture: Option<unsafe extern "C" fn(Encoder, *mut PicParams) -> Status>,
    lock_bitstream: Option<unsafe extern "C" fn(Encoder, *mut LockBitstream) -> Status>,
    unlock_bitstream: Option<unsafe extern "C" fn(Encoder, Ptr) -> Status>,
    lock_input_buffer: Option<unsafe extern "C" fn(Encoder, *mut LockInputBuffer) -> Status>,
    unlock_input_buffer: Option<unsafe extern "C" fn(Encoder, Ptr) -> Status>,
    get_encode_stats: Ptr,
    get_sequence_params: Ptr,
    register_async_event: Ptr,
    unregister_async_event: Ptr,
    map_input_resource: Ptr,
    unmap_input_resource: Ptr,
    destroy_encoder: Option<unsafe extern "C" fn(Encoder) -> Status>,
    invalidate_ref_frames: Ptr,
    open_encode_session_ex:
        Option<unsafe extern "C" fn(*mut OpenSessionParams, *mut Encoder) -> Status>,
    register_resource: Ptr,
    unregister_resource: Ptr,
    reconfigure_encoder: Option<unsafe extern "C" fn(Encoder, *mut ReconfigureParams) -> Status>,
    reserved1: Ptr,
    create_mv_buffer: Ptr,
    destroy_mv_buffer: Ptr,
    run_motion_estimation_only: Ptr,
    get_last_error_string: Option<unsafe extern "C" fn(Encoder) -> *const c_char>,
    set_io_cuda_streams: Ptr,
    get_encode_preset_config_ex:
        Option<unsafe extern "C" fn(Encoder, Guid, Guid, u32, *mut PresetConfig) -> Status>,
    get_sequence_param_ex: Ptr,
    reserved2: [Ptr; 277],
}

// Sizes from nvEncodeAPI.h 12.0 (LP64): a mistake here would make the
// driver read or write past a structure.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Guid>() == 16);
    assert!(size_of::<OpenSessionParams>() == 1552);
    assert!(size_of::<RcParams>() == 128);
    assert!(size_of::<Vui>() == 112);
    assert!(offset_of!(H264Config, vui) == 72 && offset_of!(H264Config, chroma_format_idc) == 192);
    assert!(offset_of!(HevcConfig, vui) == 64);
    assert!(
        offset_of!(Av1Config, tile_widths) == 48
            && offset_of!(Av1Config, chroma_sample_position) == 84
    );
    assert!(size_of::<CodecConfig>() == 1792);
    assert!(offset_of!(Config, rc) == 40 && offset_of!(Config, codec) == 168);
    assert!(size_of::<Config>() == 3584);
    assert!(offset_of!(InitializeParams, encode_config) == 88);
    assert!(offset_of!(InitializeParams, tuning_info) == 136);
    assert!(size_of::<InitializeParams>() == 1808);
    assert!(offset_of!(ReconfigureParams, flags) == 1816);
    assert!(offset_of!(PresetConfig, config) == 8 && size_of::<PresetConfig>() == 5128);
    assert!(size_of::<CreateInputBuffer>() == 776);
    assert!(size_of::<CreateBitstreamBuffer>() == 776);
    assert!(size_of::<LockInputBuffer>() == 1544);
    assert!(offset_of!(LockBitstream, bitstream_buffer_ptr) == 56);
    assert!(size_of::<LockBitstream>() == 1544);
    assert!(offset_of!(PicParams, picture_type) == 72 && size_of::<PicParams>() == 3360);
    assert!(offset_of!(FunctionList, get_encode_preset_config_ex) == 8 + 39 * 8);
    assert!(size_of::<FunctionList>() == 2552);
    assert!(offset_of!(FunctionList, get_encode_caps) == 8 + 7 * 8);
    assert!(size_of::<CapsParam>() == 256);
};

/// A zeroed C structure: every one used here is plain data, valid zeroed.
fn zeroed<T>() -> Box<T> {
    // SAFETY: only instantiated with the repr(C) integer/pointer/array
    // structures above, for which all-zero bytes are a valid value.
    unsafe { Box::new(std::mem::zeroed()) }
}

type CuResult = i32;
type CuContext = Ptr;

/// The two driver libraries and the entry points used from them.
struct Api {
    _encode: Library,
    _cuda: Library,
    functions: Box<FunctionList>,
    cu_device_get: unsafe extern "C" fn(*mut i32, i32) -> CuResult,
    cu_device_get_name: unsafe extern "C" fn(*mut c_char, i32, i32) -> CuResult,
    cu_ctx_create: unsafe extern "C" fn(*mut CuContext, u32, i32) -> CuResult,
    cu_ctx_destroy: unsafe extern "C" fn(CuContext) -> CuResult,
    cu_ctx_push: unsafe extern "C" fn(CuContext) -> CuResult,
    cu_ctx_pop: unsafe extern "C" fn(*mut CuContext) -> CuResult,
}
// SAFETY: the function list is read-only after creation; libraries and
// function pointers may be used from any thread.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn load() -> Result<Api> {
    // SAFETY: these are the NVIDIA driver's own libraries; their
    // initializers have no preconditions.
    let (encode, cuda) = unsafe {
        (
            Library::new("libnvidia-encode.so.1").context("no NVIDIA encoder library")?,
            Library::new("libcuda.so.1").context("no CUDA driver library")?,
        )
    };
    // SAFETY: symbol types are those of nvEncodeAPI.h and cuda.h.
    unsafe {
        let max_version: libloading::Symbol<unsafe extern "C" fn(*mut u32) -> Status> =
            encode.get(b"NvEncodeAPIGetMaxSupportedVersion\0")?;
        let mut version = 0;
        ensure!(
            max_version(&mut version) == SUCCESS,
            "NVENC version query failed"
        );
        ensure!(
            (version >> 4, version & 0xf) >= (API_MAJOR, API_MINOR),
            "the NVIDIA driver supports NVENC API {}.{}; Latch needs {API_MAJOR}.{API_MINOR} (driver 520 or later)",
            version >> 4,
            version & 0xf
        );
        let create: libloading::Symbol<unsafe extern "C" fn(*mut FunctionList) -> Status> =
            encode.get(b"NvEncodeAPICreateInstance\0")?;
        let mut functions = zeroed::<FunctionList>();
        functions.version = FUNCTION_LIST_VER;
        ensure!(
            create(&mut *functions) == SUCCESS,
            "NvEncodeAPICreateInstance failed"
        );
        let cu_init: libloading::Symbol<unsafe extern "C" fn(u32) -> CuResult> =
            cuda.get(b"cuInit\0")?;
        ensure!(cu_init(0) == 0, "cuInit failed: no usable NVIDIA GPU");
        let api = Api {
            functions,
            cu_device_get: *cuda.get(b"cuDeviceGet\0")?,
            cu_device_get_name: *cuda.get(b"cuDeviceGetName\0")?,
            cu_ctx_create: *cuda.get(b"cuCtxCreate_v2\0")?,
            cu_ctx_destroy: *cuda.get(b"cuCtxDestroy_v2\0")?,
            cu_ctx_push: *cuda.get(b"cuCtxPushCurrent_v2\0")?,
            cu_ctx_pop: *cuda.get(b"cuCtxPopCurrent_v2\0")?,
            _encode: encode,
            _cuda: cuda,
        };
        let f = &api.functions;
        ensure!(
            f.open_encode_session_ex.is_some()
                && f.get_encode_guid_count.is_some()
                && f.get_encode_guids.is_some()
                && f.get_encode_preset_config_ex.is_some()
                && f.initialize_encoder.is_some()
                && f.create_input_buffer.is_some()
                && f.destroy_input_buffer.is_some()
                && f.create_bitstream_buffer.is_some()
                && f.destroy_bitstream_buffer.is_some()
                && f.encode_picture.is_some()
                && f.lock_bitstream.is_some()
                && f.unlock_bitstream.is_some()
                && f.lock_input_buffer.is_some()
                && f.unlock_input_buffer.is_some()
                && f.destroy_encoder.is_some()
                && f.reconfigure_encoder.is_some(),
            "the NVIDIA driver's encoder interface is incomplete"
        );
        Ok(api)
    }
}

/// Calls an entry point checked present at load.
macro_rules! call {
    ($api:expr, $name:ident ( $($arg:expr),* )) => {
        ($api.functions.$name.expect(concat!(stringify!($name), " was checked at load")))($($arg),*)
    };
}

/// The CUDA context current on this thread while it lives.
struct Current<'a>(&'a Api);
impl<'a> Current<'a> {
    fn new(api: &'a Api, context: CuContext) -> Result<Self> {
        // SAFETY: `context` is a live context created by this module.
        ensure!(
            unsafe { (api.cu_ctx_push)(context) } == 0,
            "cuCtxPushCurrent failed"
        );
        Ok(Self(api))
    }
}
impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut popped = null_mut();
        // SAFETY: pops the context `new` pushed on this thread.
        unsafe { (self.0.cu_ctx_pop)(&mut popped) };
    }
}

pub struct Nvenc {
    api: &'static Api,
    context: CuContext,
    encoder: Encoder,
    input: Ptr,
    output: Ptr,
    codec: Codec,
    width: u32,
    height: u32,
    frame: u32,
    /// Kept for reconfiguration; `init.encode_config` points into `preset`.
    preset: Box<PresetConfig>,
    init: Box<InitializeParams>,
    device: String,
    encode_guid: Guid,
}
// SAFETY: the session is used by one thread at a time, with its CUDA
// context pushed for every call.
unsafe impl Send for Nvenc {}

impl Nvenc {
    pub fn open(codec: Codec, width: u32, height: u32, config: &VideoConfig) -> Result<Self> {
        let api = api().map_err(unusable)?;
        let (encode_guid, profile_guid) = match codec {
            Codec::H264Baseline => (CODEC_H264, PROFILE_H264_BASELINE),
            Codec::H264High => (CODEC_H264, PROFILE_H264_HIGH),
            Codec::Hevc => (CODEC_HEVC, PROFILE_HEVC_MAIN),
            Codec::Av1 => (CODEC_AV1, PROFILE_AV1_MAIN),
        };
        let mut device = 0;
        let mut context = null_mut();
        let mut name = [0 as c_char; 256];
        // SAFETY: plain out-parameters of the CUDA driver API.
        unsafe {
            if (api.cu_device_get)(&mut device, 0) != 0 {
                return Err(unusable(anyhow::anyhow!("no CUDA device")));
            }
            (api.cu_device_get_name)(name.as_mut_ptr(), name.len() as i32, device);
            ensure!(
                (api.cu_ctx_create)(&mut context, 0, device) == 0,
                "cuCtxCreate failed"
            );
            // Created current: make every use push it explicitly.
            let mut popped = null_mut();
            (api.cu_ctx_pop)(&mut popped);
        }
        // SAFETY: cuDeviceGetName wrote a NUL-terminated string (or nothing).
        let device_name = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let mut this = Self {
            api,
            context,
            encoder: null_mut(),
            input: null_mut(),
            output: null_mut(),
            codec,
            width,
            height,
            frame: 0,
            preset: zeroed(),
            init: zeroed(),
            device: device_name,
            encode_guid,
        };
        this.start(encode_guid, profile_guid, config)?;
        Ok(this)
    }

    fn error(&self, what: &str, status: Status) -> anyhow::Error {
        let detail = if self.encoder.is_null() {
            String::new()
        } else {
            // SAFETY: the session is live; the string is the driver's own.
            unsafe {
                let s = call!(self.api, get_last_error_string(self.encoder));
                if s.is_null() {
                    String::new()
                } else {
                    format!(": {}", CStr::from_ptr(s).to_string_lossy())
                }
            }
        };
        let error = anyhow::anyhow!("NVENC {what} failed (status {status}){detail}");
        if UNSUPPORTED.contains(&status) {
            unusable(error)
        } else {
            error
        }
    }

    /// What this GPU's encoder for the session's codec takes.
    pub fn caps(&self) -> Caps {
        let query = |caps: u32| {
            let mut param = zeroed::<CapsParam>();
            param.version = CAPS_PARAM_VER;
            param.caps_to_query = caps;
            let mut value = 0;
            // SAFETY: a live session; one value out.
            let status = unsafe {
                call!(
                    self.api,
                    get_encode_caps(self.encoder, self.encode_guid, &mut *param, &mut value)
                )
            };
            u64::try_from(value).ok().filter(|_| status == SUCCESS)
        };
        let _current = Current::new(self.api, self.context).ok();
        let side = |caps, unknown: u32| {
            query(caps)
                .filter(|&v| v > 0)
                .map_or(unknown, |v| u32::try_from(v).unwrap_or(unknown))
        };
        Caps {
            max_width: side(CAPS_WIDTH_MAX, Caps::UNKNOWN.max_width),
            max_height: side(CAPS_HEIGHT_MAX, Caps::UNKNOWN.max_height),
            max_pixels: query(CAPS_MB_NUM_MAX).unwrap_or(0) * 256,
            max_pixel_rate: query(CAPS_MB_PER_SEC_MAX).unwrap_or(0) * 256,
            ..Caps::UNKNOWN
        }
    }

    fn start(&mut self, encode_guid: Guid, profile_guid: Guid, config: &VideoConfig) -> Result<()> {
        let api = self.api;
        let _current = Current::new(api, self.context)?;
        let mut open = zeroed::<OpenSessionParams>();
        open.version = OPEN_SESSION_VER;
        open.device_type = DEVICE_CUDA;
        open.device = self.context;
        open.api_version = API_VERSION;
        // SAFETY: every structure is sized as the header says (checked
        // above) and outlives the call; handles come from the driver.
        unsafe {
            let status = call!(api, open_encode_session_ex(&mut *open, &mut self.encoder));
            if status != SUCCESS {
                self.encoder = null_mut();
                return Err(self.error("session", status));
            }
            let mut count = 0;
            call!(api, get_encode_guid_count(self.encoder, &mut count));
            let mut guids = vec![Guid(0, 0, 0, [0; 8]); count as usize];
            call!(
                api,
                get_encode_guids(self.encoder, guids.as_mut_ptr(), count, &mut count)
            );
            guids.truncate(count as usize);
            if !guids.contains(&encode_guid) {
                return Err(unusable(anyhow::anyhow!(
                    "this GPU has no {} encoder",
                    self.codec.name()
                )));
            }
            self.preset.version = PRESET_CONFIG_VER;
            self.preset.config.version = CONFIG_VER;
            let status = call!(
                api,
                get_encode_preset_config_ex(
                    self.encoder,
                    encode_guid,
                    PRESET_P1,
                    TUNING_ULTRA_LOW_LATENCY,
                    &mut *self.preset
                )
            );
            if status != SUCCESS {
                return Err(self.error("preset", status));
            }
        }
        let c = &mut self.preset.config;
        c.version = CONFIG_VER;
        c.profile_guid = profile_guid;
        c.gop_length = INFINITE_GOP;
        c.frame_interval_p = 1;
        rate(&mut c.rc, config.bitrate_kbps, config.fps);
        // SAFETY: the union member written is the one for this codec, as
        // the preset filled it.
        unsafe {
            match self.codec {
                Codec::H264Baseline | Codec::H264High => {
                    let h = &mut c.codec.h264;
                    h.flags |= H264_REPEAT_SPSPPS;
                    h.idr_period = INFINITE_GOP;
                    h.chroma_format_idc = 1;
                    h.slice_mode = 0;
                    h.slice_mode_data = 0;
                    if self.codec == Codec::H264Baseline {
                        // What latch-codec decodes: CAVLC, one reference.
                        h.entropy_coding_mode = H264_CAVLC;
                        h.max_num_ref_frames = 1;
                    } else {
                        h.entropy_coding_mode = H264_CABAC;
                    }
                    h.vui.bt709();
                }
                Codec::Hevc => {
                    let h = &mut c.codec.hevc;
                    h.flags = (h.flags & !(HEVC_CHROMA_FORMAT | HEVC_BIT_DEPTH))
                        | HEVC_REPEAT_SPSPPS
                        | (1 << 9);
                    h.idr_period = INFINITE_GOP;
                    h.slice_mode = 0;
                    h.slice_mode_data = 0;
                    h.vui.bt709();
                }
                Codec::Av1 => {
                    let a = &mut c.codec.av1;
                    // Low-overhead OBUs, as other AV1 encoders give them.
                    a.flags = (a.flags & !(AV1_ANNEX_B | AV1_CHROMA_FORMAT | AV1_BIT_DEPTHS))
                        | AV1_REPEAT_SEQ_HDR
                        | (1 << 7);
                    a.idr_period = INFINITE_GOP;
                    a.color_primaries = 1;
                    a.transfer_characteristics = 1;
                    a.matrix_coefficients = 1;
                    a.color_range = 0;
                }
            }
        }
        let init = &mut *self.init;
        init.version = INITIALIZE_VER;
        init.encode_guid = encode_guid;
        init.preset_guid = PRESET_P1;
        init.encode_width = self.width;
        init.encode_height = self.height;
        init.dar_width = self.width;
        init.dar_height = self.height;
        init.frame_rate_num = config.fps;
        init.frame_rate_den = 1;
        init.enable_ptd = 1;
        init.encode_config = &mut self.preset.config;
        init.max_encode_width = self.width;
        init.max_encode_height = self.height;
        init.tuning_info = TUNING_ULTRA_LOW_LATENCY;
        init.buffer_format = BUFFER_NV12;
        // SAFETY: as above.
        unsafe {
            let status = call!(api, initialize_encoder(self.encoder, init));
            if status != SUCCESS {
                return Err(self.error("initialization", status));
            }
            let mut input = zeroed::<CreateInputBuffer>();
            input.version = CREATE_INPUT_VER;
            input.width = self.width;
            input.height = self.height;
            input.buffer_fmt = BUFFER_NV12;
            let status = call!(api, create_input_buffer(self.encoder, &mut *input));
            if status != SUCCESS {
                return Err(self.error("input buffer", status));
            }
            self.input = input.input_buffer;
            let mut output = zeroed::<CreateBitstreamBuffer>();
            output.version = CREATE_BITSTREAM_VER;
            let status = call!(api, create_bitstream_buffer(self.encoder, &mut *output));
            if status != SUCCESS {
                return Err(self.error("bitstream buffer", status));
            }
            self.output = output.bitstream_buffer;
        }
        Ok(())
    }
}

/// CBR at `kbps` with a one-frame VBV: a frame never queues behind the
/// one before it on a link of that rate.
fn rate(rc: &mut RcParams, kbps: u32, fps: u32) {
    let bps = kbps.saturating_mul(1000).max(1000);
    rc.version = RC_PARAMS_VER;
    rc.rate_control_mode = RC_CBR;
    rc.average_bit_rate = bps;
    rc.max_bit_rate = bps;
    rc.vbv_buffer_size = bps / fps.max(1);
    rc.vbv_initial_delay = rc.vbv_buffer_size;
    rc.flags |= RC_ZERO_REORDER_DELAY;
}

impl PictureEncoder for Nvenc {
    fn name(&self) -> String {
        format!("NVENC {} ({})", self.codec.name(), self.device)
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
            "picture size differs from the NVENC session's"
        );
        let api = self.api;
        let _current = Current::new(api, self.context)?;
        // SAFETY: buffers belong to this session; the locked input is
        // written within its pitch × NV12 height and unlocked before
        // encoding, and the locked bitstream is copied before unlocking.
        unsafe {
            let mut lock = zeroed::<LockInputBuffer>();
            lock.version = LOCK_INPUT_VER;
            lock.input_buffer = self.input;
            let status = call!(api, lock_input_buffer(self.encoder, &mut *lock));
            if status != SUCCESS {
                return Err(self.error("input lock", status));
            }
            let pitch = lock.pitch as usize;
            let height = self.height as usize;
            let data = std::slice::from_raw_parts_mut(
                lock.buffer_data_ptr.cast::<u8>(),
                nv12::size(pitch, height),
            );
            let (y, uv) = data.split_at_mut(pitch * height);
            nv12::write(picture, y, pitch, uv, pitch);
            call!(api, unlock_input_buffer(self.encoder, self.input));

            let mut pic = zeroed::<PicParams>();
            pic.version = PIC_PARAMS_VER;
            pic.input_width = self.width;
            pic.input_height = self.height;
            pic.input_pitch = pitch as u32;
            pic.encode_pic_flags = if keyframe {
                PIC_FLAG_FORCEIDR | PIC_FLAG_OUTPUT_SPSPPS
            } else {
                0
            };
            pic.frame_idx = self.frame;
            pic.input_time_stamp = self.frame.into();
            pic.input_buffer = self.input;
            pic.output_bitstream = self.output;
            pic.buffer_fmt = BUFFER_NV12;
            pic.picture_struct = PIC_STRUCT_FRAME;
            pic.picture_type = if keyframe { PIC_TYPE_IDR } else { PIC_TYPE_P };
            self.frame = self.frame.wrapping_add(1);
            let status = call!(api, encode_picture(self.encoder, &mut *pic));
            if status != SUCCESS {
                return Err(self.error("encode", status));
            }
            let mut lock = zeroed::<LockBitstream>();
            lock.version = LOCK_BITSTREAM_VER;
            lock.output_bitstream = self.output;
            let status = call!(api, lock_bitstream(self.encoder, &mut *lock));
            if status != SUCCESS {
                return Err(self.error("bitstream lock", status));
            }
            out.extend_from_slice(std::slice::from_raw_parts(
                lock.bitstream_buffer_ptr.cast::<u8>(),
                lock.bitstream_size_in_bytes as usize,
            ));
            call!(api, unlock_bitstream(self.encoder, self.output));
            Ok(matches!(lock.picture_type, PIC_TYPE_IDR | PIC_TYPE_I))
        }
    }

    fn set_bitrate(&mut self, kbps: u32) {
        let fps = self.init.frame_rate_num;
        rate(&mut self.preset.config.rc, kbps, fps);
        let Ok(_current) = Current::new(self.api, self.context) else {
            return;
        };
        let mut params = zeroed::<ReconfigureParams>();
        params.version = RECONFIGURE_VER;
        params.reinit = *self.init;
        params.reinit.encode_config = &mut self.preset.config;
        // SAFETY: the session is live and the parameters are those it was
        // initialized with, with a new rate.
        let status = unsafe { call!(self.api, reconfigure_encoder(self.encoder, &mut *params)) };
        if status != SUCCESS {
            tracing::warn!("{:#}", self.error("bitrate change", status));
        }
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        let api = self.api;
        if let Ok(_current) = Current::new(api, self.context) {
            // SAFETY: each handle is destroyed once, with the session's
            // context current, before the context itself.
            unsafe {
                if !self.input.is_null() {
                    call!(api, destroy_input_buffer(self.encoder, self.input));
                }
                if !self.output.is_null() {
                    call!(api, destroy_bitstream_buffer(self.encoder, self.output));
                }
                if !self.encoder.is_null() {
                    call!(api, destroy_encoder(self.encoder));
                }
            }
        }
        // SAFETY: created in `open`, no longer current anywhere.
        unsafe { (api.cu_ctx_destroy)(self.context) };
    }
}

/// Whether NVENC is here at all: the driver's libraries load and know the
/// API. Cheap; [`super::hardware`] then opens a real session per codec.
pub fn present() -> bool {
    api().is_ok()
}
