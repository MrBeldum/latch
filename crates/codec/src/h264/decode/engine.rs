//! The decode driver: Annex-B NAL loop → SPS/PPS state → slice-data
//! macroblock layer → intra/inter reconstruction → deblocking → DPB
//! insertion → [`Frame`] output.
//!
//! Scope is the baseline subset the rest of the crate implements:
//! I/P slices, CAVLC, frame pictures (`frame_mbs_only` enforced in the
//! SPS parse), 4:2:0 chroma. Field pictures, MBAFF, B/SP/SI slices,
//! CABAC and data partitioning surface as [`Error::Unsupported`];
//! malformed streams surface as [`Error`] instead of panicking.


use super::cavlc::{self, BlockKind};
use super::deblock;
use super::dpb::Dpb;
use super::golomb::Br;
use super::inter;
use super::intra::{self, NbSamples};
use super::mb::{MbMap, MbState, MbType, Nb, SubMbType, intra4x4_neighbours};
use super::nal::{self, Nal};
use super::pps::{self, Pps};
use super::slice::{self, Mmco, SliceHeader, SliceType};
use super::sps::{self, Sps};
use super::tables::{
    CAVLC_SCAN_8X8, CBP_INTER, CBP_INTRA, QPC_TABLE, block_index,
    block_xy,
};
use super::transform;
use super::{Frame, Limits};
use std::vec::Vec;
use super::{Error, Result};

/// Coded planes of one in-progress picture. Luma stride `w`, chroma
/// stride `w/2`; both full-macroblock aligned.
#[derive(Clone)]
struct FrameBuf {
    w: usize,
    h: usize,
    y: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
}

impl FrameBuf {
    fn new(w: usize, h: usize) -> FrameBuf {
        FrameBuf {
            w,
            h,
            y: std::vec![0u8; w * h],
            cb: std::vec![0u8; w * h / 4],
            cr: std::vec![0u8; w * h / 4],
        }
    }
}

/// Per-picture decode state.
struct Pic {
    sps: Sps,
    first_header: SliceHeader,
    buf: FrameBuf,
    /// Per-MB state (picture order).
    mbs: Vec<MbState>,
    /// Which SPS/PPS this picture is coded under.
    sps_id: u32,
    pps_id: u32,
    /// PPS chroma QP offsets at picture creation (deblocking input;
    /// kept on the picture so a mid-stream PPS switch cannot rewrite
    /// an in-flight picture's filter state).
    chroma_off_cb: i32,
    chroma_off_cr: i32,
    /// `frame_num` of the picture (slice-boundary detection).
    frame_num: u32,
    /// Picture order count of this picture (spec 8.2.1), needed for
    /// RefPicList construction and temporal-direct scaling.
    poc: i64,
    /// `nal_ref_idc` of its slices.
    nal_ref_idc: u8,
    /// Whether the picture is IDR, and its `idr_pic_id` (multi-slice
    /// IDR pictures must repeat the same id).
    idr: bool,
    idr_pic_id: u32,
    /// Deferred marking: `(adaptive, idr_marking, mmco)` of the
    /// picture's last slice.
    marking: (bool, (bool, bool), Vec<Mmco>),
}

/// Decoder state carried across NALs of one `decode` call.
///
/// Exposed so [`super::decode`] can be a thin wrapper; a caller wanting
/// push-based decoding can drive [`Decoder::push_stream`] itself.
pub struct Decoder {
    limits: Limits,
    sps_table: [Option<Sps>; 32],
    pps_table: [Option<Pps>; 256],
    need_idr: bool,
    dpb: Dpb,
    pic: Option<Pic>,
    frames: Vec<Frame>,
    /// Display-order sort keys parallel to `frames`: `(epoch, poc)`
    /// where `epoch` bumps on every IDR so per-GOP POC restarts sort
    /// inside their own GOP (B-slice streams decode out of order).
    frame_pocs: Vec<(i64, i64)>,
    /// Current POC epoch — incremented by every decoded IDR picture.
    poc_epoch: i64,
    /// Monotonic slice counter feeding `MbState::slice_id`.
    slice_seq: u32,
    /// POC bookkeeping for `pic_order_cnt_type` 0: MSB/LSB of the
    /// previous reference picture (spec 8.2.1.1).
    prev_poc_msb: i64,
    prev_poc_lsb: i64,
    /// `frame_num` of the previous reference picture (POC types 1/2).
    prev_frame_num: u32,
    /// `frameNumOffset` accumulator of POC type 1.
    prev_frame_num_offset: i64,
}

/// Per-slice state shared by every macroblock call.
struct SliceCx<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    h: &'a SliceHeader,
    /// Resolved reference list 0: indices into `dpb.refs` in list order.
    ref_order: &'a [usize],
    /// `QPy` — updated by `mb_qp_delta`; skip MBs keep it untouched
    /// (spec 7.4.5: `QPy` carries through `mb_skip_run`).
    qp_prev: i32,
    /// Previous MB's `mb_qp_delta` — the CABAC `mb_qp_delta` context
    /// splits on it being zero (spec 9.3.2.7); skipped/direct MBs reset it.
    /// Slice id for `MbState::slice_id`.
    slice_id: u32,
}

impl Decoder {
    /// Creates a decoder honouring `limits`.
    pub fn new(limits: Limits) -> Decoder {
        Decoder {
            limits,
            sps_table: std::array::from_fn(|_| None),
            pps_table: std::array::from_fn(|_| None),
            need_idr: true,
            dpb: Dpb::default(),
            pic: None,
            frames: Vec::new(),
            frame_pocs: Vec::new(),
            poc_epoch: 0,
            slice_seq: 0,
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
            prev_frame_num: 0,
            prev_frame_num_offset: 0,
        }
    }

    pub(super) fn max_input(&self) -> usize { self.limits.max_input }

    pub(super) fn invalidate(&mut self) {
        self.dpb.flush(); self.pic = None; self.frames.clear(); self.frame_pocs.clear(); self.need_idr = true;
    }

    /// Top-level POC derivation (spec 8.2.1): wraps the type-specific
    /// computation and the prev-picture state updates the spec makes
    /// conditional on `nal_ref_idc != 0`. Called once per new picture,
    /// using the *first slice's* header values.
    fn poc_of(&mut self, h: &SliceHeader, sps: &Sps, nal_ref_idc: u8, idr: bool) -> i64 {
        let poc = compute_poc(
            h,
            sps,
            nal_ref_idc,
            idr,
            self.prev_poc_msb,
            self.prev_poc_lsb,
            self.prev_frame_num,
            self.prev_frame_num_offset,
        );
        if nal_ref_idc != 0 {
            self.prev_frame_num = h.frame_num;
            self.prev_frame_num_offset = poc.1;
            self.prev_poc_msb = poc.2;
            self.prev_poc_lsb = poc.3;
        }
        poc.0
    }

    /// Feeds a whole Annex-B stream; returns decoded frames in order.
    pub fn push_stream(&mut self, stream: &[u8]) -> Result<Vec<Frame>> {
        let nals = nal::split_annex_b(stream)?;
        if nals.is_empty() {
            return Err(Error::BadValue("h264 stream with no NAL units"));
        }
        let old_sps = self.sps_table.clone(); let old_pps = self.pps_table.clone();
        let result = (|| { for n in &nals { self.nal(n)?; } self.end_of_stream() })();
        if result.is_err() { self.sps_table = old_sps; self.pps_table = old_pps; }
        result
    }

    fn nal(&mut self, n: &Nal<'_>) -> Result<()> {
        match n.unit_type {
            nal::NAL_SPS => {
                let rbsp = nal::rbsp(n.payload)?;
                let sps = sps::parse(&rbsp)?;
                let id = sps.id as usize;
                self.sps_table[id] = Some(sps);
            }
            nal::NAL_PPS => {
                let rbsp = nal::rbsp(n.payload)?;
                let p = pps::parse(&rbsp)?;
                let id = p.id as usize;
                self.pps_table[id] = Some(p);
            }
            nal::NAL_SLICE | nal::NAL_IDR => self.slice_nal(n, n.unit_type == nal::NAL_IDR)?,
            nal::NAL_DPA | nal::NAL_DPB | nal::NAL_DPC => {
                return Err(Error::Unsupported("h264 slice data partitioning"));
            }
            // Non-VCL NALs this crate does not decode — skipped.
            nal::NAL_SEI
            | nal::NAL_AUD
            | nal::NAL_EOSEQ
            | nal::NAL_EOSTREAM
            | nal::NAL_FILLER
            | nal::NAL_SPS_EXT => {}
            // Auxiliary/extension/scalable slice types are real
            // picture data we refuse, not padding.
            nal::NAL_CODED_SLICE_AUX | 14 | 15 | 20 | 21 => {
                return Err(Error::Unsupported("h264 auxiliary/extension slice"));
            }
            _ => {}
        }
        Ok(())
    }

    /// One VCL NAL: header parse, picture-boundary detection, MB loop.
    fn slice_nal(&mut self, n: &Nal<'_>, idr: bool) -> Result<()> {
        let rbsp = nal::rbsp(n.payload)?;
        let mut peek = Br::new(&rbsp);
        peek.ue()?; let kind = peek.ue()?;
        if self.need_idr && matches!(kind,0|5) { return Err(Error::MissingReference); }
        let pid = peek.ue()? as usize;
        let pps = self.pps_table.get(pid).and_then(Option::as_ref).ok_or(Error::BadValue("unknown PPS"))?.clone();
        let sps = self.sps_table.get(pps.sps_id as usize).and_then(Option::as_ref).ok_or(Error::BadValue("unknown SPS"))?.clone();
        let mut br = Br::new(&rbsp);
        let h = slice::parse_header(&mut br, idr, n.ref_idc, &sps, &pps)?;
        if h.redundant_pic_cnt > 0 { return Ok(()); }
        if self.need_idr && !idr { return Err(Error::MissingReference); }
        self.slice_seq = self.slice_seq.wrapping_add(1);
        let slice_id = self.slice_seq;

        let new_pic = match &self.pic {
            Some(p) => {
                p.sps_id != sps.id
                    || p.pps_id != h.pps_id
                    || p.idr != idr
                    || p.frame_num != h.frame_num
                    || (p.nal_ref_idc == 0) != (n.ref_idc == 0)
                    || p.first_header.pic_order_cnt_lsb != h.pic_order_cnt_lsb
                    || p.first_header.delta_poc_bottom != h.delta_poc_bottom
                    || p.first_header.delta_poc0 != h.delta_poc0
                    || p.first_header.delta_poc1 != h.delta_poc1
                    || p.sps.width_mbs != sps.width_mbs || p.sps.height_mbs != sps.height_mbs
                    || p.sps.crop != sps.crop
                    || (h.first_mb == 0 && p.mbs[0].slice_id != u32::MAX)
                    || (idr && p.idr && p.idr_pic_id != h.idr_pic_id)
            }
            None => true,
        };
        if new_pic && !idr && n.ref_idc != 0 {
            let modulus = 1u32 << sps.log2_max_frame_num;
            let mut next = (self.prev_frame_num + 1) % modulus;
            if h.frame_num != self.prev_frame_num && h.frame_num != next {
                if !sps.gaps_in_frame_num_allowed { return Err(Error::MissingReference); }
                while next != h.frame_num { self.dpb.gap(next)?; next = (next + 1) % modulus; }
            }
        }
        let cur_poc = if new_pic {
            Some(self.poc_of(&h, &sps, n.ref_idc, idr))
        } else {
            None
        };
        if new_pic {
            self.finish_picture()?;
            if idr {
                self.need_idr = false;
                self.prev_frame_num = 0;
                self.dpb.reset(1u32 << sps.log2_max_frame_num, sps.max_num_ref_frames, self.limits.max_refs);
                // IDR flushes the DPB unless the header asks for
                // long-term keeping (spec 8.2.5.1).
                if !h.idr_marking.0 {
                    self.dpb.flush();
                }
            }
            let w = (sps.width_mbs * 16) as usize;
            let hh = (sps.height_mbs * 16) as usize;
            let poc = cur_poc.unwrap_or(0);
            self.pic = Some(Pic {
                sps: sps.clone(),
                first_header: h.clone(),
                buf: FrameBuf::new(w, hh),
                mbs: std::vec![MbState::new(); (sps.width_mbs * sps.height_mbs) as usize],
                sps_id: sps.id,
                pps_id: h.pps_id,
                chroma_off_cb: pps.chroma_qp_index_offset,
                chroma_off_cr: pps.chroma_qp_index_offset_cr,
                frame_num: h.frame_num,
                poc,
                nal_ref_idc: n.ref_idc,
                idr,
                idr_pic_id: h.idr_pic_id,
                marking: (false, h.idr_marking, Vec::new()),
            });
        }
        if let Some(p) = self.pic.as_mut() {
            p.nal_ref_idc = n.ref_idc;
            if h.adaptive_marking || !h.mmco.is_empty() {
                p.marking = (h.adaptive_marking, h.idr_marking, h.mmco.clone());
            }
        }

        let ref_order: Vec<usize> = if h.slice_type != SliceType::I {
            let order = self
                .dpb
                .ref_list0(&h, h.frame_num)?;
            if order.is_empty() || self.dpb.refs.is_empty() {
                return Err(Error::MissingReference);
            }
            order
        } else {
            Vec::new()
        };
        let mut cx = SliceCx {
            sps: &sps,
            pps: &pps,
            h: &h,
            ref_order: &ref_order,
            qp_prev: pps.pic_init_qp + h.slice_qp_delta,
            slice_id,
        };
        let pic = self.pic.as_mut().unwrap();
        slice_data(pic, &mut br, &mut cx, &self.dpb, &rbsp)?;
        for m in pic.mbs.iter_mut().filter(|m| m.slice_id == slice_id) {
            for (i, &r) in m.ref_idx.iter().enumerate() {
                m.deblock_refs[i] = if r == 255 { 255 } else { *ref_order.get(r as usize).ok_or(Error::MissingReference)? as u8 };
            }
        }
        Ok(())
    }

    /// Closes the current picture: deblock, DPB insert + marking, crop,
    /// output.
    fn finish_picture(&mut self) -> Result<()> {
        let Some(pic) = self.pic.take() else {
            return Ok(());
        };
        let sps = &pic.sps;
        let wm = sps.width_mbs as usize;
        let hm = sps.height_mbs as usize;
        if pic.mbs.iter().any(|m| m.slice_id == u32::MAX) { return Err(Error::BadValue("incomplete picture")); }
        let mut mbs = pic.mbs;
        for m in &mut mbs { m.ref_idx = m.deblock_refs; }
        let mut buf = pic.buf;

        deblock::filter_frame(
            &mut buf.y,
            &mut buf.cb,
            &mut buf.cr,
            buf.w,
            &mbs,
            wm,
            hm,
            &QPC_TABLE,
            pic.chroma_off_cb,
            pic.chroma_off_cr,
        );

        if pic.nal_ref_idc != 0 {
            // Colocation data for future B-slice temporal direct
            // prediction: the picture's own L0/L1 lists (as frame_nums)
            // plus per-MB mv/ref snapshots.
    

            // Synthesised header carrying the picture's deferred
            // marking — `apply_marking` reads only these fields.
            let stub = SliceHeader {
                redundant_pic_cnt: 0,
                first_mb: 0,
                slice_type: SliceType::I,
                pps_id: pic.pps_id,
                frame_num: pic.frame_num,
                idr_pic_id: 0,
                pic_order_cnt_lsb: 0,
                delta_poc_bottom: 0,
                delta_poc0: 0,
                delta_poc1: 0,
                num_ref_idx_l0_active: 1,
                reorder_l0: Vec::new(),
                idr_marking: pic.marking.1,
                adaptive_marking: pic.marking.0,
                mmco: pic.marking.2.clone(),
                slice_qp_delta: 0,
                disable_deblock_idc: 0,
                offset_a: 0,
                offset_b: 0,
            };
            let long_term = self.dpb.apply_marking(&stub, pic.frame_num, pic.nal_ref_idc)?;
            let mmco5 = pic.marking.2.iter().any(|m| m.op == 5);
            let slot = self.dpb.push(if mmco5 { 0 } else { pic.frame_num },
                if mmco5 { 0 } else { pic.poc }, buf.y.clone(), buf.cb.clone(), buf.cr.clone(), buf.w);
            self.dpb.refs[slot].long_term = long_term;
            // MMCO op 5 (spec 8.2.5.4.6): re-base POC — the just-
            // finished picture becomes the wrap reference.
            if pic.marking.0 && pic.marking.2.iter().any(|m| m.op == 5) {
                self.prev_frame_num = 0;
                self.prev_frame_num_offset = 0;
                self.prev_poc_msb = 0;
                self.prev_poc_lsb = -i64::from(pic.first_header.delta_poc_bottom).min(0);
            }
        }
        if self.frames.len() >= self.limits.max_frames {
            return Err(Error::too_large("h264 frames", self.limits.max_frames));
        }
        if pic.idr {
            self.poc_epoch += 1;
        }
        self.frame_pocs.push((self.poc_epoch, pic.poc));
        let mut frame = crop_frame(sps, buf)?;
        frame.poc = pic.poc; frame.epoch = self.poc_epoch as u64; self.frames.push(frame);
        if pic.marking.2.iter().any(|m| m.op == 5) { self.poc_epoch += 1; }
        Ok(())
    }

    /// End-of-stream: flushes the in-progress picture (if any) and
    /// drains every decoded frame so far in presentation order. The
    /// drain is load-bearing: `push_stream` is called once per input
    /// sample and must emit only the *new* frames each time, never
    /// the whole accumulated history.
    fn end_of_stream(&mut self) -> Result<Vec<Frame>> {
        self.finish_picture()?;
        self.frame_pocs.clear();
        Ok(core::mem::take(&mut self.frames))
    }
}

/// Picture-order-count derivation (spec 8.2.1), ported from the
/// reference decoder's `ff_h264_init_poc`. Returns
/// `(poc, frame_num_offset_used, poc_msb_used, poc_lsb_used)` — the
/// "used" values are what the spec stores back for non-reference
/// pictures (IDR resets them to the current values, MMCO-5 to the
/// picture's own).
#[allow(clippy::too_many_arguments)]
fn compute_poc(
    h: &SliceHeader,
    sps: &Sps,
    nal_ref_idc: u8,
    idr: bool,
    prev_msb: i64,
    prev_lsb: i64,
    prev_fn: u32,
    prev_fno: i64,
) -> (i64, i64, i64, i64) {
    let max_frame_num = 1i64 << sps.log2_max_frame_num;
    let mut frame_num_offset = prev_fno;
    if i64::from(h.frame_num) < i64::from(prev_fn) {
        frame_num_offset += max_frame_num;
    }
    if idr {
        // IDR resets the wrap reference to this picture's own lsb.
        let lsb = i64::from(h.pic_order_cnt_lsb);
        let poc = lsb.min(lsb + i64::from(h.delta_poc_bottom));
        return (poc, 0, 0, lsb);
    }
    match sps.pic_order_cnt_type {
        0 => {
            let max_lsb = 1i64 << sps.log2_max_poc_lsb;
            let lsb = i64::from(h.pic_order_cnt_lsb);
            // prev_lsb == -1 marks "no previous reference yet" (fresh
            // decoder or just after IDR): adopt the current lsb.
            let plsb = if prev_lsb < 0 { lsb } else { prev_lsb };
            let msb = if lsb < plsb && plsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > plsb && plsb - lsb < -max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            };
            let top = msb + lsb;
            let bottom = top + i64::from(h.delta_poc_bottom);
            (top.min(bottom), frame_num_offset, msb, lsb)
        }
        1 => {
            // Spec 8.2.1.2 as implemented by the reference decoder.
            let mut abs_frame_num = if sps.offset_for_ref_frame.is_empty() {
                0i64
            } else {
                frame_num_offset + i64::from(h.frame_num)
            };
            if nal_ref_idc == 0 && abs_frame_num > 0 {
                abs_frame_num -= 1;
            }
            let cycle_len = sps.offset_for_ref_frame.len() as i64;
            let expected_delta: i64 = sps.offset_for_ref_frame.iter().map(|&v| i64::from(v)).sum();
            let mut expected_poc = 0i64;
            if abs_frame_num > 0 && cycle_len > 0 {
                let poc_cycle_cnt = (abs_frame_num - 1) / cycle_len;
                let frame_num_in_cycle = (abs_frame_num - 1) % cycle_len;
                expected_poc = poc_cycle_cnt * expected_delta;
                for (i, &d) in sps.offset_for_ref_frame.iter().enumerate() {
                    if i as i64 > frame_num_in_cycle {
                        break;
                    }
                    expected_poc += i64::from(d);
                }
            }
            if nal_ref_idc == 0 {
                expected_poc += i64::from(sps.offset_for_non_ref_pic);
            }
            let top = expected_poc + i64::from(h.delta_poc0);
            let bottom = top + i64::from(sps.offset_for_top_to_bottom) + i64::from(h.delta_poc1);
            (top.min(bottom), frame_num_offset, 0, 0)
        }
        _ => {
            // Type 2 (spec 8.2.1.3): poc = 2*(frameNumOffset+frame_num)
            // minus one for non-reference pictures.
            let mut poc = 2 * (frame_num_offset + i64::from(h.frame_num));
            if nal_ref_idc == 0 {
                poc -= 1;
            }
            (poc, frame_num_offset, 0, 0)
        }
    }
}

/// `Pic::poc` of the picture in progress (set at creation).
/// Crops a coded [`FrameBuf`] to the display rectangle from the SPS and
/// packs it into a [`Frame`]. `Sps::crop` holds the four
/// `frame_cropping_*_offset` values already scaled to frame units;
/// chroma offsets are `offset` samples in each halved plane.
fn crop_frame(sps: &Sps, mut buf: FrameBuf) -> Result<Frame> {
    let x = sps.crop[0] as usize * 2; let y = sps.crop[2] as usize * 2;
    let w = buf.w.checked_sub((sps.crop[0] as usize+sps.crop[1] as usize)*2).ok_or(Error::BadValue("crop width"))?;
    let h = buf.h.checked_sub((sps.crop[2] as usize+sps.crop[3] as usize)*2).ok_or(Error::BadValue("crop height"))?;
    if w == 0 || h == 0 { return Err(Error::BadValue("empty crop")); }
    let mut picture = if x == 0 && y == 0 {
        let padded_h = h.div_ceil(16)*16;
        buf.y.truncate(buf.w*padded_h); buf.cb.truncate(buf.w*padded_h/4); buf.cr.truncate(buf.w*padded_h/4);
        crate::Yuv420 { width: w as u32, height: h as u32, y_stride: buf.w, c_stride: buf.w/2, y: buf.y, u: buf.cb, v: buf.cr }
    } else {
        let mut output = crate::Yuv420::new(w as u32,h as u32);
        for (dst,src,stride,sw,sh,sx,sy,ss) in [
            (&mut output.y,&buf.y,output.y_stride,w,h,x,y,buf.w),
            (&mut output.u,&buf.cb,output.c_stride,w/2,h/2,x/2,y/2,buf.w/2),
            (&mut output.v,&buf.cr,output.c_stride,w/2,h/2,x/2,y/2,buf.w/2),
        ] {
            for (row,d) in dst.chunks_mut(stride).take(sh).enumerate() { let start=(sy+row)*ss+sx; d[..sw].copy_from_slice(&src[start..start+sw]); }
        }
        output
    };
    picture.pad_edges();
    Ok(Frame { picture, poc: 0, epoch: 0 })
}

/// The macroblock-layer loop of `slice_data` (spec 7.3.4).
fn slice_data(
    pic: &mut Pic,
    br: &mut Br<'_>,
    cx: &mut SliceCx<'_>,
    dpb: &Dpb,
    _rbsp: &[u8],
) -> Result<()> {
    if cx.pps.cabac { return Err(Error::Unsupported("CABAC")); }
    let wm = cx.sps.width_mbs as usize;
    let total = (cx.sps.width_mbs * sps_height(cx)) as usize;
    let mut mb_idx = cx.h.first_mb as usize;
    if mb_idx > total {
        return Err(Error::BadValue("first_mb_in_slice past picture"));
    }
    loop {
        if cx.h.slice_type == SliceType::P {
            let run = br.ue()?;
            for _ in 0..run {
                if mb_idx >= total {
                    return Err(Error::BadValue("mb_skip_run past picture"));
                }
                if pic.mbs[mb_idx].slice_id != u32::MAX { return Err(Error::BadValue("overlapping slices")); }
                p_skip(pic, cx, dpb, mb_idx, wm)?;
                mb_idx += 1;
            }
            if mb_idx >= total || br.no_more_rbsp_data() {
                break;
            }
        }
        if mb_idx >= total || pic.mbs[mb_idx].slice_id != u32::MAX { return Err(Error::BadValue("overlapping slices")); }
        mb_decode(pic, br, cx, dpb, mb_idx, wm)?;
        mb_idx += 1;
        if mb_idx >= total || br.no_more_rbsp_data() {
            break;
        }
    }
    br.trailing_bits()
}

/// `sps.height_mbs` through the slice context (one-liner to keep the
/// two loops readable).
fn sps_height(cx: &SliceCx<'_>) -> u32 {
    cx.sps.height_mbs
}

/// P_Skip reconstruction (spec 8.4.1 / 7.3.5.2): one 16x16 partition,
/// `ref_idx` 0, MV = the median predictor, no residual.
fn p_skip(pic: &mut Pic, cx: &mut SliceCx<'_>, dpb: &Dpb, idx: usize, wm: usize) -> Result<()> {
    let map = MbMap {
        idx,
        x: idx % wm,
        y: idx / wm,
        width: wm,
        sid: cx.slice_id,
    };
    if cx.ref_order.is_empty() {
        return Err(Error::BadValue("P_Skip with empty ref list"));
    }
    // Spec 8.4.1.1: the skip motion vector is the median predictor,
    // EXCEPT it is forced to (0,0) when the left or top neighbour is
    // unavailable (frame edge or another slice) or when either one's
    // reference-0 vector is already (0,0).
    let a = mv_at(&pic.mbs, None, map, -1, 0);
    let b = mv_at(&pic.mbs, None, map, 0, -1);
    let force_zero = |n: Option<([i16; 2], i32)>| match n {
        None => true,
        Some((mv, r)) => r == 0 && mv == [0, 0],
    };
    let mvp = if force_zero(a) || force_zero(b) {
        [0i16; 2]
    } else {
        mvp_l0_parts(&pic.mbs, None, map, 0, 0, 4, 4, 0)
    };

    {
        let m = &mut pic.mbs[idx];
        m.mb_type = MbType::PSkip;
        m.skip = true;
        m.cbp = 0;
        m.slice_id = cx.slice_id;
        m.dbg_idx = idx as u32;
        m.qp_y = cx.qp_prev.clamp(0, 51) as u8;
        m.disable_deblock_idc = cx.h.disable_deblock_idc;
        m.filter_offset_a = cx.h.offset_a;
        m.filter_offset_b = cx.h.offset_b;
        for b in m.mv.iter_mut() {
            *b = [mvp[0], mvp[1]];
        }
        for r in m.ref_idx.iter_mut() {
            *r = 0;
        }
        m.mv_valid = [true; 16];
        m.nz = [0; 24];
    }
    mc_partition(
        &mut pic.buf,
        cx,
        dpb,
        map.x * 16,
        map.y * 16,
        16,
        16,
        [i32::from(mvp[0]), i32::from(mvp[1])],
        0,
    )?;
    Ok(())
}

/// B_Skip / B_Direct_16x16 reconstruction: temporal or spatial direct
/// prediction per 8x8 region, no residual, no coded motion.
/// Full macroblock decode: syntax parse then reconstruction.
fn mb_decode(
    pic: &mut Pic,
    br: &mut Br<'_>,
    cx: &mut SliceCx<'_>,
    dpb: &Dpb,
    idx: usize,
    wm: usize,
) -> Result<()> {
    let code = br.ue()?;
    let mb_type = match cx.h.slice_type {
        SliceType::I => MbType::i_slice(code)?,
        SliceType::P => MbType::p_slice(code)?,
    };
    let map = MbMap {
        idx,
        x: idx % wm,
        y: idx / wm,
        width: wm,
        sid: cx.slice_id,
    };
    let mut m = MbState::new();
    m.mb_type = mb_type;
    m.slice_id = cx.slice_id;
    m.dbg_idx = idx as u32;
    m.disable_deblock_idc = cx.h.disable_deblock_idc;
    m.filter_offset_a = cx.h.offset_a;
    m.filter_offset_b = cx.h.offset_b;

    // ---- mb_pred / sub_mb_pred (spec 7.3.5) ----
    // Inter partitions as `(x4, y4, w4, h4, ref_idx, mvd)` on the
    // 4x4-block grid.
    let mut inter_parts: Vec<(usize, usize, usize, usize, u8, [i16; 2])> = Vec::new();
    let sub_direct_allowed_8x8 = true;
    let mut sub_blocks_8x8 = false;
    match mb_type {
        MbType::I4x4 => {
            // transform_8x8_mode_flag (spec 7.3.5 mb_pred) — the
            // High-profile I_8x8 syntax, gated on
            // `pps.transform_8x8_mode_flag` alone (the CABAC/ctx399
            // gate additionally fires on direct_8x8_inference).
            let t8x8_allowed = cx.pps.transform_8x8_mode;
            if t8x8_allowed && br.bit()? {
                m.transform8x8 = true;
            }
            if m.transform8x8 {
                // One mode per 8x8 group (spec 7.3.5.1
                // prev_intra8x8_pred_mode_flag); the mode fills the
                // group's four 4x4 cache cells so the 8x8 MPM reads
                // neighbours like the reference decoder.
                for blk in 0..4 {
                    let g = blk * 4;
                    let mpm = i4x4_mpm(&pic.mbs, &m, map, g, cx.pps.constrained_intra_pred);
                    let flag = br.bit()?;
                    let mode = if flag {
                        mpm
                    } else {
                        let rem = br.bits(3)? as u8;
                        if rem < mpm { rem } else { rem + 1 }
                    };
                    for i in 0..4 {
                        let (bx, by) = block_xy(g + i);
                        m.i4x4_modes[by * 4 + bx] = mode;
                    }
                }
            } else {
                // Parsed in luma4x4BlkIdx (group-major) order; stored
                // raster-indexed for neighbour lookup.
                for blk in 0..16 {
                    let (bx, by) = block_xy(blk);
                    let r = by * 4 + bx;
                    let mpm = i4x4_mpm(&pic.mbs, &m, map, blk, cx.pps.constrained_intra_pred);
                    let flag = br.bit()?;
                    let mode = if flag {
                        mpm
                    } else {
                        let rem = br.bits(3)? as u8;
                        if rem < mpm { rem } else { rem + 1 }
                    };
                    let _ = (blk, r);
                    m.i4x4_modes[r] = mode;
                }
            }
            m.chroma_pred = br.ue()? as u8;
            if m.chroma_pred > 3 {
                return Err(Error::BadValue("intra_chroma_pred_mode over 3"));
            }
        }
        MbType::I16x16 { .. } => {
            m.chroma_pred = br.ue()? as u8;
            if m.chroma_pred > 3 {
                return Err(Error::BadValue("intra_chroma_pred_mode over 3"));
            }
        }
        MbType::IPcm => {}
        MbType::PSkip => return Err(Error::BadValue("P_Skip inside mb_skip_run gap")),
        MbType::P8x8 | MbType::P8x8Ref0 => {
            let mut sub_types = [SubMbType::S8x8; 4];
            for st in sub_types.iter_mut() {
                *st = SubMbType::from_code(br.ue()?)?;
            }
            // ffmpeg ff_h264_p_sub_mb_type_info (h264data.c): sub code
            // 3 (4x4 partitions) maps to MB_TYPE_8x8 which *blocks*
            // the flag — the flag bit exists only when every sub is
            // an 8x8 partition.
            if sub_types.iter().any(|st| !matches!(st, SubMbType::S8x8)) {
                sub_blocks_8x8 = true;
            }
            // ref_idx_l0 coded once per 8x8 sub-mb (spec 7.3.5.1);
            // P_8x8ref0 implies zero.
            let mut refs = [0u8; 4];
            let need_ref = cx.h.num_ref_idx_l0_active > 1 && mb_type != MbType::P8x8Ref0;
            for r in refs.iter_mut() {
                if need_ref {
                    *r = br.te(cx.h.num_ref_idx_l0_active - 1)? as u8;
                    if (*r as usize) >= (cx.h.num_ref_idx_l0_active as usize) {
                        return Err(Error::BadValue("ref_idx_l0 out of list"));
                    }
                }
            }
            for (sm, &st) in sub_types.iter().enumerate() {
                let (sizes, nparts) = st.parts();
                let sx = (sm % 2) * 2;
                let sy = (sm / 2) * 2;
                for pi in 0..nparts {
                    let _ = sizes.get(pi);
                    let mvd = [br.se()?, br.se()?];
                    let (x4, y4, w4, h4) = sub_part_geo(sx, sy, st, pi);
                    inter_parts.push((x4, y4, w4, h4, refs[sm], clamp_mvd(mvd)?));
                }
            }
        }
        _ => {
            // P16x16 / P16x8 / P8x16.
            let parts: [(usize, usize, usize, usize); 2] = match mb_type {
                MbType::P16x16 => [(0, 0, 4, 4), (0, 0, 0, 0)],
                MbType::P16x8 => [(0, 0, 4, 2), (0, 2, 4, 2)],
                MbType::P8x16 => [(0, 0, 2, 4), (2, 0, 2, 4)],
                _ => unreachable!(),
            };
            let nparts = if mb_type == MbType::P16x16 { 1 } else { 2 };
            let need_ref = cx.h.num_ref_idx_l0_active > 1;
            // Spec 7.3.5.1 order: ref_idx_l0 for EVERY partition first,
            // then mvd_l0 for every partition. Interleaving them (as
            // ref,mvd,ref,mvd) desyncs the stream whenever a partition
            // uses a non-zero reference index.
            let mut refs = [0u8; 2];
            for r in refs.iter_mut().take(nparts) {
                if need_ref {
                    *r = br.te(cx.h.num_ref_idx_l0_active - 1)? as u8;
                    if *r as usize >= cx.h.num_ref_idx_l0_active as usize {
                        return Err(Error::BadValue("ref_idx_l0 out of list"));
                    }
                }
            }
            for (pi, &(x4, y4, w4, h4)) in parts.iter().take(nparts).enumerate() {
                let mvd = clamp_mvd([br.se()?, br.se()?])?;
                inter_parts.push((x4, y4, w4, h4, refs[pi], mvd));
            }
        }
    }

    // ---- coded_block_pattern + mb_qp_delta (spec 7.3.5) ----
    // B_Direct_16x16 codes cbp like any other inter MB (see the
    // CABAC twin of this block).
    let (mut cbp_luma, mut cbp_chroma) = (0u8, 0u8);
    if mb_type != MbType::IPcm {
        if let MbType::I16x16 {
            cbp_chroma: cc,
            cbp_luma: cl,
            ..
        } = mb_type
        {
            cbp_luma = cl;
            cbp_chroma = cc;
        } else {
            let cbp = br.ue()? as usize;
            if cbp >= 48 {
                return Err(Error::BadValue("coded_block_pattern over 47"));
            }
            let v = if mb_type.is_intra() {
                CBP_INTRA[cbp]
            } else {
                CBP_INTER[cbp]
            };
            cbp_luma = v % 16;
            cbp_chroma = v / 16;
        }
    }
    // `MbState::cbp` feeds the deblocker's residual gates (deblock.rs
    // `compute_bs`: the `edges = mask_edge==3 && !(cbp&15)` shortcut and
    // the 8x8DCT `cbp&7==7` bypass read it for CAVLC MBs too — the
    // CABAC twin stores it at the same point).
    m.cbp = (cbp_chroma << 4) | cbp_luma;
    // transform_size_8x8_flag (ffmpeg h264_cabac.c:2347:
    // `dct8x8_allowed && (cbp&15) && !IS_INTRA`; dct8x8_allowed is the
    // PPS flag narrowed by get_dct8x8_allowed for P_8x8/B_8x8).
    let dct8x8_allowed = cx.pps.transform_8x8_mode
        && !matches!(mb_type, MbType::I16x16 { .. } | MbType::IPcm)
        && !sub_blocks_8x8
        && (sub_direct_allowed_8x8 || cx.sps.direct_8x8_inference)
        ;
    let t8x8 = dct8x8_allowed && cbp_luma > 0 && !mb_type.is_intra() && br.bit()?;

    // Intra keeps the I_8x8 flag read during mb_pred (ffmpeg
    // h264_cavlc.c:797); inter takes the read here.
    m.transform8x8 = if mb_type.is_intra() {
        m.transform8x8
    } else {
        t8x8
    };
    let coded = cbp_luma > 0 || cbp_chroma > 0 || matches!(mb_type, MbType::I16x16 { .. });
    let mut qp_y = cx.qp_prev;
    if coded && mb_type != MbType::IPcm {
        let delta = br.se()?;
        qp_y = (i64::from(cx.qp_prev) + i64::from(delta)).rem_euclid(52) as i32;
        cx.qp_prev = qp_y;
    }
    if mb_type == MbType::IPcm {
        qp_y = 0;
    }
    m.qp_y = qp_y.clamp(0, 51) as u8;

    // ---- residual parse (spec 7.3.5.3) ----
    // `nz_acc` is the in-flight nC context: neighbour reads come from
    // `pic.mbs` (fully decoded MBs) and same-MB reads from `nz_acc`.
    let mut nz_acc = [0u8; 24];
    let mut luma_res = [[0i32; 16]; 16];
    let mut luma_res8 = [[0i32; 64]; 4];
    let mut dc_y = [0i32; 16];
    let mut chroma_ac = [[[0i32; 16]; 4]; 2];
    let mut chroma_dc = [[0i32; 4]; 2];
    if mb_type != MbType::IPcm {
        if let MbType::I16x16 { .. } = mb_type {
            let nc = nc_value(&pic.mbs, &nz_acc, map, 0, Plane::Luma);
            let r = cavlc::decode_block(br, nc, BlockKind::LumaOrChromaAc4x4)?;
            for s in 0..16 {
                // The 4x4 DC grid is raster-ordered over luma blocks:
                // position = by*4 + bx of the block (spec 8.5.11.1).
                dc_y[cavlc::raster_index(BlockKind::LumaOrChromaAc4x4, s)] = r.levels[s];
            }
            for g in 0..4usize {
                if cbp_luma & (1 << g) == 0 {
                    continue;
                }
                for s4 in 0..4usize {
                    let blk = g * 4 + s4;
                    let nc = nc_value(&pic.mbs, &nz_acc, map, blk, Plane::Luma);
                    let r = cavlc::decode_block(br, nc, BlockKind::Ac15)?;
                    for s in 0..15 {
                        let ri = cavlc::raster_index(BlockKind::Ac15, s);
                        luma_res[blk][ri] = r.levels[s];
                    }
                    nz_acc[blk] = r.total_coeff;
                }
            }
        } else if cbp_luma > 0 {
            for (g, res8) in luma_res8.iter_mut().enumerate() {
                if cbp_luma & (1 << g) == 0 {
                    continue;
                }
                if m.transform8x8 {
                    // CAVLC 8x8: four 16-coefficient reads per group
                    // (spec 7.3.5.3.2), each scattered into the 8x8
                    // coefficient grid by `CAVLC_SCAN_8X8` — the same
                    // `zigzag_scan8x8_cavlc` the reference decoder uses.
                    for s4 in 0..4usize {
                        let blk = g * 4 + s4;
                        let nc = nc_value(&pic.mbs, &nz_acc, map, blk, Plane::Luma);
                        let r = cavlc::decode_block(br, nc, BlockKind::LumaOrChromaAc4x4)?;
                        for k in 0..16 {
                            let ri = CAVLC_SCAN_8X8[16 * s4 + k] as usize;
                            res8[ri] = r.levels[k];
                        }
                        nz_acc[blk] = r.total_coeff;
                    }
                } else {
                    for s4 in 0..4usize {
                        let blk = g * 4 + s4;
                        let nc = nc_value(&pic.mbs, &nz_acc, map, blk, Plane::Luma);
                        let r = cavlc::decode_block(br, nc, BlockKind::LumaOrChromaAc4x4)?;
                        for s in 0..16 {
                            let ri = cavlc::raster_index(BlockKind::LumaOrChromaAc4x4, s);
                            luma_res[blk][ri] = r.levels[s];
                        }
                        nz_acc[blk] = r.total_coeff;
                    }
                }
            }
        }
        if cbp_chroma > 0 {
            for dc in chroma_dc.iter_mut() {
                let r = cavlc::decode_block(br, -1, BlockKind::ChromaDc)?;
                for s in 0..4 {
                    dc[cavlc::raster_index(BlockKind::ChromaDc, s)] = r.levels[s];
                }
            }
            if cbp_chroma == 2 {
                for c in 0..2usize {
                    for b in 0..4usize {
                        let nc = nc_value(&pic.mbs, &nz_acc, map, b, Plane::chroma(c));
                        let r = cavlc::decode_block(br, nc, BlockKind::Ac15)?;
                        for s in 0..15 {
                            let ri = cavlc::raster_index(BlockKind::Ac15, s);
                            chroma_ac[c][b][ri] = r.levels[s];
                        }
                        nz_acc[16 + c * 4 + b] = r.total_coeff;
                    }
                }
            }
        }
    }
    m.nz = nz_acc;

    // ---- reconstruction ----
    let px0 = map.x * 16;
    let py0 = map.y * 16;
    match mb_type {
        MbType::IPcm => {
            br.byte_align();
            for row in 0..16 {
                for x in 0..16 {
                    pic.buf.y[(py0 + row) * pic.buf.w + px0 + x] = br.byte()?;
                }
            }
            for plane in [&mut pic.buf.cb, &mut pic.buf.cr] {
                for row in 0..8 {
                    for x in 0..8 { let o = (py0/2+row)*(pic.buf.w/2)+px0/2+x; plane[o] = br.byte()?; }
                }
            }
            m.nz = [16; 24];
        }
        MbType::I4x4 => {
            if m.transform8x8 {
                // I_8x8: predict one 8x8 group at a time in raster
                // order — each group reads the reconstructed samples
                // of the earlier groups (spec 8.3.5.2 availability).
                for (g, raw_resid) in luma_res8.iter().enumerate() {
                    let nb = gather_luma8x8(&pic.buf, &pic.mbs, map, g, cx.pps);
                    // Top-left 4x4 cell of this 8x8 group on the raster
                    // mode grid: y*4 + x with (x, y) = (2*(g%2), 2*(g/2)).
                    let mode = m.i4x4_modes[8 * (g / 2) + 2 * (g % 2)];
                    let mut pred = [0u8; 64];
                    intra::pred8x8(mode, &nb, &mut pred);

                    let mut resid = *raw_resid;
                    if coded && cbp_luma & (1 << g) != 0 {
                        transform::dequant_8x8(&mut resid, m.qp_y);
                        transform::inverse_8x8(&mut resid);
                    }
                    let (bx, by) = ((g % 2) * 8, (g / 2) * 8);
                    for yy in 0..8 {
                        for xx in 0..8 {
                            let v = i32::from(pred[yy * 8 + xx]) + resid[yy * 8 + xx];
                            pic.buf.y[(py0 + by + yy) * pic.buf.w + px0 + bx + xx] =
                                v.clamp(0, 255) as u8;
                        }
                    }
                }
            } else {
                // Reconstruct in group-major coding order.
                for (blk, raw_resid) in luma_res.iter().enumerate() {
                    let (bx, by) = block_xy(blk);
                    let r = by * 4 + bx;
                    let nb = gather4x4(&pic.buf, &pic.mbs, map, bx, by, blk, cx.pps);
                    let mut pred = [0u8; 16];
                    intra::pred4x4(m.i4x4_modes[r], &nb, &mut pred)?;
                    let mut resid = *raw_resid;
                    if coded {
                        transform::dequant_4x4(&mut resid, m.qp_y);
                        transform::inverse_4x4(&mut resid)?;
                    }
                    for yy in 0..4 {
                        for xx in 0..4 {
                            let v = i32::from(pred[yy * 4 + xx]) + resid[yy * 4 + xx];
                            pic.buf.y[(py0 + by * 4 + yy) * pic.buf.w + px0 + bx * 4 + xx] =
                                v.clamp(0, 255) as u8;
                        }
                    }
                }
            }
            chroma_recon(
                pic, cx, map, &m, &chroma_ac, &chroma_dc, cbp_chroma, px0, py0,
            )?;
        }
        MbType::I16x16 { pred, .. } => {
            let nb = gather16x16(&pic.buf, &pic.mbs, map, cx.pps);
            let mut out = [0u8; 256];
            intra::pred16x16(pred, &nb, &mut out)?;
            let mut dc = dc_y;
            transform::inv_luma_dc(&mut dc, m.qp_y);
            for (blk, raw_resid) in luma_res.iter().enumerate() {
                let (bx, by) = block_xy(blk);
                let mut resid = *raw_resid;
                resid[0] = dc[by * 4 + bx];
                // AC positions dequantise only when the 8x8 group was
                // coded; the Hadamard-scaled DC in slot 0 plus zeros
                // still goes through the inverse transform either way.
                if cbp_luma & (1 << (blk / 4)) != 0 {
                    for (i, v) in resid.iter_mut().enumerate().skip(1) {
                        let (x, y) = (i % 4, i / 4);
                        *v = transform::dequant_coeff(*v, m.qp_y, x, y);
                    }
                }
                transform::inverse_4x4(&mut resid)?;
                for yy in 0..4 {
                    for xx in 0..4 {
                        let v =
                            i32::from(out[(by * 4 + yy) * 16 + bx * 4 + xx]) + resid[yy * 4 + xx];
                        pic.buf.y[(py0 + by * 4 + yy) * pic.buf.w + px0 + bx * 4 + xx] =
                            v.clamp(0, 255) as u8;
                    }
                }
            }
            chroma_recon(
                pic, cx, map, &m, &chroma_ac, &chroma_dc, cbp_chroma, px0, py0,
            )?;
        }
        _ => {
            // Inter: per partition MVP + MC, then residual add.
            for &(x4, y4, w4, h4, ref_idx, mvd) in &inter_parts {
                let mvp = mvp_l0_parts(&pic.mbs, Some(&m), map, x4, y4, w4, h4, ref_idx);
                let mv = [
                    mvp[0].wrapping_add(mvd[0]),
                    mvp[1].wrapping_add(mvd[1]),
                ];
                for yy in y4..y4 + h4 {
                    for xx in x4..x4 + w4 {
                        let b = block_index(xx, yy);
                        m.mv[b] = mv;
                        m.ref_idx[b] = ref_idx;
                        m.mv_valid[b] = true;
                    }
                }
                mc_partition(
                    &mut pic.buf,
                    cx,
                    dpb,
                    px0 + x4 * 4,
                    py0 + y4 * 4,
                    w4 * 4,
                    h4 * 4,
                    [i32::from(mv[0]), i32::from(mv[1])],
                    ref_idx,
                )?;
            }
            // Add luma residual where cbp says coefficients exist.

            if m.transform8x8 {
                for (g, raw_resid) in luma_res8.iter().enumerate() {
                    if raw_resid.iter().all(|&v| v == 0) {
                        continue;
                    }
                    let (bx, by) = ((g % 2) * 8, (g / 2) * 8);
                    let mut resid = *raw_resid;
                    transform::dequant_8x8(&mut resid, m.qp_y);

                    transform::inverse_8x8(&mut resid);
                    let o = (py0 + by) * pic.buf.w + px0 + bx;
                    transform::add_residual_8x8(&mut pic.buf.y[o..], pic.buf.w, &resid);
                }
            } else {
                for (blk, raw_resid) in luma_res.iter().enumerate() {
                    if raw_resid.iter().all(|&v| v == 0) {
                        continue;
                    }
                    let (bx, by) = block_xy(blk);
                    let mut resid = *raw_resid;
                    transform::dequant_4x4(&mut resid, m.qp_y);

                    transform::inverse_4x4(&mut resid)?;
                    let o = (py0 + by * 4) * pic.buf.w + px0 + bx * 4;
                    transform::add_residual_4x4(&mut pic.buf.y[o..], pic.buf.w, &resid);
                }
            }
            chroma_recon(
                pic, cx, map, &m, &chroma_ac, &chroma_dc, cbp_chroma, px0, py0,
            )?;
        }
    }

    pic.mbs[idx] = m;
    Ok(())
}

/// `mvd` clamped to the representable quarter-pel range (a hostile
/// `se()` can produce values far beyond i16).
fn clamp_mvd(mvd: [i32; 2]) -> Result<[i16; 2]> {
    Ok([i16::try_from(mvd[0]).map_err(|_| Error::BadValue("horizontal motion difference"))?,
        i16::try_from(mvd[1]).map_err(|_| Error::BadValue("vertical motion difference"))?])
}

/// Chroma reconstruction shared by all MB types: intra MBs predict
/// with `intra_chroma_pred_mode`; inter MBs keep the samples chroma MC
/// already wrote. Residual is added either way.
#[allow(clippy::too_many_arguments)]
fn chroma_recon(
    pic: &mut Pic,
    cx: &SliceCx<'_>,
    map: MbMap,
    m: &MbState,
    chroma_ac: &[[[i32; 16]; 4]; 2],
    chroma_dc: &[[i32; 4]; 2],
    cbp_chroma: u8,
    px0: usize,
    py0: usize,
) -> Result<()> {
    let cw = pic.buf.w / 2;
    for c in 0..2usize {
        let qpc = chroma_qp(cx.pps, m.qp_y, c);
        let mut pred = [0u8; 64];
        if m.mb_type.is_intra() {
            let plane: &[u8] = if c == 0 { &pic.buf.cb } else { &pic.buf.cr };
            let nb = gather8x8(plane, &pic.mbs, map, cx.pps, cw);
            intra::pred_chroma(m.chroma_pred, &nb, &mut pred)?;
        }
        let dc = transform::inv_chroma_dc(&chroma_dc[c], qpc);
        let plane = if c == 0 {
            &mut pic.buf.cb
        } else {
            &mut pic.buf.cr
        };
        for (b, (ac, &dcv)) in chroma_ac[c].iter().zip(dc.iter()).enumerate() {
            let (bx, by) = (b % 2, b / 2);
            let mut resid = *ac;
            resid[0] = dcv;
            if cbp_chroma > 0 {
                for (i, v) in resid.iter_mut().enumerate().skip(1) {
                    let (x, y) = (i % 4, i / 4);
                    *v = transform::dequant_coeff(*v, qpc, x, y);
                }
                transform::inverse_4x4(&mut resid)?;
            }
            for yy in 0..4 {
                for xx in 0..4 {
                    let o = (py0 / 2 + by * 4 + yy) * cw + px0 / 2 + bx * 4 + xx;
                    let base = if m.mb_type.is_intra() {
                        pred[(by * 4 + yy) * 8 + bx * 4 + xx]
                    } else {
                        plane[o]
                    };
                    plane[o] = (i32::from(base) + resid[yy * 4 + xx]).clamp(0, 255) as u8;
                }
            }
        }
    }
    Ok(())
}

/// Chroma QP for a plane via its own PPS offset (spec 8.5.8): Cb uses
/// `chroma_qp_index_offset`, Cr uses `second_chroma_qp_index_offset`
/// (which defaults to the first when the PPS extension tail is absent).
fn chroma_qp(pps: &Pps, qp_y: u8, c: usize) -> u8 {
    let off = if c == 0 {
        pps.chroma_qp_index_offset
    } else {
        pps.chroma_qp_index_offset_cr
    };
    let idx = (i32::from(qp_y) + off).clamp(0, 51) as usize;
    QPC_TABLE[idx]
}

/// Motion-compensates one partition's luma + chroma into `buf`
/// (weighted prediction applied when the slice carries a table).
#[allow(clippy::too_many_arguments)]
fn mc_partition(
    buf: &mut FrameBuf,
    cx: &SliceCx<'_>,
    dpb: &Dpb,
    x0: usize,
    y0: usize,
    pw: usize,
    ph: usize,
    mv: [i32; 2],
    ref_idx: u8,
) -> Result<()> {
    let ri = ref_idx as usize;
    let &ref_slot = cx
        .ref_order
        .get(ri)
        .ok_or(Error::BadValue("ref_idx past active list"))?;
    let rf = dpb
        .refs
        .get(ref_slot)
        .ok_or(Error::MissingReference)?;
    if rf.width != buf.w || rf.y.len() != buf.w * buf.h { return Err(Error::MissingReference); }
    let mut luma = [0u8; 256];
    inter::mc_luma(
        &rf.y,
        buf.w,
        buf.h,
        x0 as i32,
        y0 as i32,
        (mv[0], mv[1]),
        pw,
        ph,
        &mut luma,
    );

    for yy in 0..ph {
        buf.y[(y0 + yy) * buf.w + x0..(y0 + yy) * buf.w + x0 + pw]
            .copy_from_slice(&luma[yy * pw..yy * pw + pw]);
    }
    let (cx0, cy0, cw2, ch2) = (x0 / 2, y0 / 2, pw / 2, ph / 2);
    for c in 0..2usize {
        let rf_plane = if c == 0 { &rf.cb } else { &rf.cr };
        let mut tmp = [0u8; 64];
        inter::mc_chroma(
            rf_plane,
            buf.w / 2,
            buf.h / 2,
            cx0 as i32,
            cy0 as i32,
            (mv[0], mv[1]),
            cw2,
            ch2,
            &mut tmp,
        );
        let dst = if c == 0 { &mut buf.cb } else { &mut buf.cr };
        for yy in 0..ch2 {
            dst[(cy0 + yy) * (buf.w / 2) + cx0..(cy0 + yy) * (buf.w / 2) + cx0 + cw2]
                .copy_from_slice(&tmp[yy * cw2..yy * cw2 + cw2]);
        }
    }
    Ok(())
}

/// Plane selector for nC derivation.
#[derive(Copy, Clone)]
enum Plane {
    /// Luma 4x4 block (group-major idx 0..15).
    Luma,
    /// Cb chroma 4x4 AC block (0..3 on the chroma 2x2 grid).
    Cb,
    /// Cr.
    Cr,
}

impl Plane {
    fn chroma(c: usize) -> Plane {
        if c == 0 { Plane::Cb } else { Plane::Cr }
    }
    /// Index into `MbState::nz` / the in-flight accumulator.
    fn nz_idx(self, blk: usize) -> usize {
        match self {
            Plane::Luma => blk,
            Plane::Cb => 16 + blk,
            Plane::Cr => 20 + blk,
        }
    }
    /// `(x, y)` of `blk` on this plane's block grid (4x4 luma,
    /// 2x2 chroma).
    fn xy(self, blk: usize) -> (usize, usize) {
        match self {
            Plane::Luma => block_xy(blk),
            _ => (blk % 2, blk / 2),
        }
    }
    /// Back from grid coords to this plane's block index space.
    fn index(self, x: usize, y: usize) -> usize {
        match self {
            Plane::Luma => block_index(x, y),
            _ => y * 2 + x,
        }
    }
    /// Grid dimension (4 for luma, 2 for chroma).
    fn dim(self) -> i32 {
        match self {
            Plane::Luma => 4,
            _ => 2,
        }
    }
}

/// `nC` for CAVLC (spec 9.2.1): TotalCoeff of the left (nA) and top
/// (nB) blocks. Same-MB neighbours read from `nz_acc`; foreign-MB
/// neighbours read `mbs`. Unavailable neighbours drop out; both missing
/// gives nC 0 (Table 9-5 column 0).
fn nc_value(mbs: &[MbState], nz_acc: &[u8; 24], map: MbMap, blk: usize, plane: Plane) -> i32 {
    let (x, y) = plane.xy(blk);
    let dim = plane.dim();
    let fetch = |nx: i32, ny: i32| -> Option<i32> {
        if nx >= 0 && ny >= 0 && nx < dim && ny < dim {
            return Some(i32::from(
                nz_acc[plane.nz_idx(plane.index(nx as usize, ny as usize))],
            ));
        }
        // Foreign-MB block. nA (nx < 0, ny inside) comes from mbA's
        // right edge; nB (ny < 0) from mbB's bottom edge. nx<0&ny<0
        // doesn't happen here (nA/nB are axis neighbours only).
        let nidx = if nx < 0 { map.mb_a() } else { map.mb_b() }?;
        let nb = &mbs[nidx];
        if nb.slice_id != map.sid {
            return None;
        }
        if nb.mb_type == MbType::IPcm {
            return Some(16);
        }
        let lx = (nx + dim) % dim;
        let ly = (ny + dim) % dim;
        let v = i32::from(nb.nz[plane.nz_idx(plane.index(lx as usize, ly as usize))]);
        Some(v)
    };
    let na = fetch(x as i32 - 1, y as i32);
    let nb = fetch(x as i32, y as i32 - 1);
    match (na, nb) {
        (Some(a), Some(b)) => (a + b + 1) >> 1,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 0,
    }
}

/// Spec 8.3.1.1's `predIntra4x4PredMode` for *group-major* block `blk`:
/// `min(modeA, modeB)` when both neighbours are available, DC when
/// either is not; a coded but non-I4x4 neighbour contributes DC.
fn i4x4_mpm(mbs: &[MbState], cur: &MbState, map: MbMap, blk: usize, constrained: bool) -> u8 {
    let [a, b, _c, _d] = intra4x4_neighbours(blk);
    // Spec 8.3.1.1: a neighbour is *unavailable* when its block is
    // outside the picture, in a not-yet-decoded MB, a not-yet-coded
    // same-MB block, or — under constrained_intra_pred — an inter MB.
    // When EITHER A or B is unavailable the most-probable mode is DC
    // outright (dcPredModePredictedFlag is false); only when both are
    // available is it min(modeA, modeB), where a non-I4x4 neighbour
    // contributes DC.
    let mode = |nb: Nb, x: usize, y: usize| -> Option<u8> {
        match nb {
            Nb::Curr => {
                if block_index(x, y) < blk && cur.i4x4_modes[y * 4 + x] != 0xff {
                    Some(cur.i4x4_modes[y * 4 + x])
                } else {
                    None
                }
            }
            Nb::None => None,
            other => {
                let nidx = match other {
                    Nb::A => map.mb_a(),
                    Nb::B => map.mb_b(),
                    Nb::C => map.mb_c(),
                    Nb::D => map.mb_d(),
                    _ => None,
                };
                match nidx {
                    Some(i) => {
                        let nb2 = &mbs[i];
                        if nb2.slice_id != map.sid || (constrained && !nb2.mb_type.is_intra()) {
                            None
                        } else if nb2.mb_type == MbType::I4x4 {
                            Some(nb2.i4x4_modes[y * 4 + x])
                        } else {
                            Some(2)
                        }
                    }
                    None => None,
                }
            }
        }
    };
    let ma = mode(a.0, a.1, a.2);
    let mb_ = mode(b.0, b.1, b.2);
    match (ma, mb_) {
        (Some(x), Some(y)) => x.min(y),
        _ => 2,
    }
}

/// MVP for one partition (spec 8.4.1.3): directional special cases
/// then the A/B/C median. `cur` carries this MB's in-flight state —
/// same-MB partitions decoded earlier in this MB are valid neighbours.
#[allow(clippy::too_many_arguments)]
fn mvp_l0_parts(
    mbs: &[MbState],
    cur: Option<&MbState>,
    map: MbMap,
    x4: usize,
    y4: usize,
    w4: usize,
    h4: usize,
    ref_idx: u8,
) -> [i16; 2] {
    // Neighbour blocks (spec 6.4.11.7 / 8.4.1.3.2):
    //   A: the 4x4 block left of (x4, y4)
    //   B: above (x4, y4)
    //   C: above (x4 + w4, y4) — falls back to D when unavailable.
    let a = mv_at(mbs, cur, map, x4 as i32 - 1, y4 as i32);
    let b = mv_at(mbs, cur, map, x4 as i32, y4 as i32 - 1);
    let mut c = mv_at(mbs, cur, map, x4 as i32 + w4 as i32, y4 as i32 - 1);
    if c.is_none() {
        c = mv_at(mbs, cur, map, x4 as i32 - 1, y4 as i32 - 1);
    }
    let (mv_a, ref_a) = a.unwrap_or(([0, 0], -1));
    let (mv_b, ref_b) = b.unwrap_or(([0, 0], -1));
    let (mv_c, ref_c) = c.unwrap_or(([0, 0], -1));
    let ri = i32::from(ref_idx);

    // Directional single-candidate rules are MB-partition-only (spec
    // 8.4.1.1 "If MbPartWidth is 16 and MbPartHeight is 8..." / the
    // 8x16 analogue; ffmpeg keeps them in pred_16x8/pred_8x16_motion,
    // called only from MB-partition decode). Sub-mb partitions
    // (8x8/8x4/4x8/4x4) always take the median path — the previous
    // 8x4/4x8 cases generalised the rules down to sub-mbs and broke
    // every mixed-sub P8x8 MB in t2 (t2_8x8_part frame 1+).
    if w4 == 4 && h4 == 2 {
        if y4 == 0 && ref_b == ri {
            return mv_b;
        }
        if y4 == 2 && ref_a == ri {
            return mv_a;
        }
    }
    if w4 == 2 && h4 == 4 {
        if x4 == 0 && ref_a == ri {
            return mv_a;
        }
        if x4 == 2 && ref_c == ri {
            return mv_c;
        }
    }
    // Median (spec 8-211/8-212): a single matching ref short-circuits;
    // with B and C both unavailable the predictor is A alone.
    if b.is_none() && c.is_none() {
        return mv_a;
    }
    let matches = [ref_a == ri, ref_b == ri, ref_c == ri];
    if matches.iter().filter(|&&m| m).count() == 1 {
        if matches[0] {
            return mv_a;
        }
        if matches[1] {
            return mv_b;
        }
        return mv_c;
    }
    [
        med3(mv_a[0], mv_b[0], mv_c[0]),
        med3(mv_a[1], mv_b[1], mv_c[1]),
    ]
}

fn med3(a: i16, b: i16, c: i16) -> i16 {
    a.max(b).min(a.min(b).max(c))
}

/// MV + ref of the inter block containing 4x4-grid sample `(x, y)` —
/// negative or ≥4 coordinates reach into the neighbour MBs. Returns
/// `Some((mv, ref))` where `ref == -1` means "cannot predict from it"
/// (intra-coded or undecoded); `None` when the position is outside the
/// picture.
fn mv_at(
    mbs: &[MbState],
    cur: Option<&MbState>,
    map: MbMap,
    x: i32,
    y: i32,
) -> Option<([i16; 2], i32)> {
    if (0..4).contains(&x) && (0..4).contains(&y) {
        let nb = cur?;
        let b = block_index(x as usize, y as usize);
        if nb.mb_type.is_intra() {
            return Some(([0, 0], -1));
        }
        let r = nb.ref_idx[b];
        // A same-MB block whose partition has not been decoded yet
        // (ref 0xff or MV not committed — refs commit before mvds in
        // P8x8) is an unavailable neighbour (spec 8.4.1.1), not a
        // zero-motion candidate.
        if r == 0xff || !nb.mv_valid[b] {
            return None;
        }
        return Some((nb.mv[b], i32::from(r)));
    }
    let nidx = match (x < 0, y < 0) {
        (true, false) => map.mb_a(),
        (false, true) => {
            if x >= 4 {
                map.mb_c()
            } else {
                map.mb_b()
            }
        }
        (true, true) => map.mb_d(),
        // In-MB coordinates that reach the right or below neighbour
        // are not yet coded — unavailable, not a bug.
        (false, false) => None,
    }?;
    let nb = &mbs[nidx];
    if nb.slice_id != map.sid {
        return None;
    }
    let lx = x.rem_euclid(4);
    let ly = y.rem_euclid(4);
    let b = block_index(lx as usize, ly as usize);
    if nb.mb_type.is_intra() {
        return Some(([0, 0], -1));
    }
    let r = nb.ref_idx[b];
    if !nb.mv_valid[b] {
        return Some((nb.mv[b], -1));
    }
    Some((nb.mv[b], if r == 0xff { -1 } else { i32::from(r) }))
}

/// Sub-partition geometry inside the 8x8 sub-mb whose top-left on the
/// 4x4 grid is `(sx, sy)`: returns `(x4, y4, w4, h4)` in MB coords.
fn sub_part_geo(sx: usize, sy: usize, st: SubMbType, part: usize) -> (usize, usize, usize, usize) {
    match st {
        SubMbType::S8x8 => (sx, sy, 2, 2),
        SubMbType::S8x4 => (sx, sy + part, 2, 1),
        SubMbType::S4x8 => (sx + part, sy, 1, 2),
        SubMbType::S4x4 => (sx + part % 2, sy + part / 2, 1, 1),
        // B sub-partition geometry comes from `SubMbType::parts` +
        // `b_part_xy` instead; this function only serves P types.
    }
}

/// Whether a foreign neighbour MB's samples may feed intra prediction:
/// it must be coded, and under `constrained_intra_pred` it must be
/// intra.
fn intra_ok(nb: &MbState, pps: &Pps, sid: u32) -> bool {
    nb.slice_id == sid && (!pps.constrained_intra_pred || nb.mb_type.is_intra())
}

/// Gathers prediction neighbours for a luma 4x4 block at grid `(bx, by)`
/// (group-major `blk`) into [`NbSamples`]. Availability follows spec
/// 6.4.11.4/8.3.1.1: same-MB blocks are available only when earlier in
/// coding order; foreign blocks need a coded (and under
/// `constrained_intra_pred`, intra) neighbour MB.
fn gather4x4(
    buf: &FrameBuf,
    mbs: &[MbState],
    map: MbMap,
    bx: usize,
    by: usize,
    blk: usize,
    pps: &Pps,
) -> NbSamples {
    let px = map.x * 16 + bx * 4;
    let py = map.y * 16 + by * 4;
    let mut nb = NbSamples {
        left: [0; 16],
        top: [0; 16],
        top_right: None,
        top_left: None,
        has_left: false,
        has_top: false,
    };
    let [aref, bref, cref, dref] = intra4x4_neighbours(blk);
    let foreign = |who: Nb| -> Option<&MbState> {
        let i = match who {
            Nb::A => map.mb_a(),
            Nb::B => map.mb_b(),
            Nb::C => map.mb_c(),
            Nb::D => map.mb_d(),
            _ => None,
        }?;
        mbs.get(i)
    };
    // Left: available iff its source block is usable (Curr is always
    // earlier — left-of-block is always group-major-earlier? NO: the
    // left block of (bx,by) is (bx-1,by) which is earlier; mbA is
    // always fully decoded).
    let left_ok = match aref.0 {
        Nb::Curr => true, // (bx-1, by) is always earlier
        Nb::None => false,
        w => foreign(w)
            .map(|s| intra_ok(s, pps, map.sid))
            .unwrap_or(false),
    };
    if left_ok && px > 0 {
        nb.has_left = true;
        for i in 0..4 {
            nb.left[i] = buf.y[(py + i) * buf.w + px - 1];
        }
    }
    let top_ok = match bref.0 {
        Nb::Curr => true,
        Nb::None => false,
        w => foreign(w)
            .map(|s| intra_ok(s, pps, map.sid))
            .unwrap_or(false),
    };
    if top_ok && py > 0 {
        nb.has_top = true;
        for i in 0..4 {
            nb.top[i] = buf.y[(py - 1) * buf.w + px + i];
        }
        // Top-right samples live in the C block (spec 6.4.11.4):
        // Curr(x+1,y-1) is available only if coded earlier; mbB for
        // by==0&bx<3; mbC at the top-right corner; None otherwise.
        let tr_ok = match cref.0 {
            Nb::Curr => block_index(cref.1, cref.2) < blk,
            Nb::None => false,
            w => foreign(w)
                .map(|s| intra_ok(s, pps, map.sid))
                .unwrap_or(false),
        };
        if tr_ok {
            let mut tr = [0u8; 16];
            for (i, d) in tr.iter_mut().enumerate() {
                *d = buf.y[(py - 1) * buf.w + px + 4 + i];
            }
            nb.top_right = Some(tr);
        }
    }
    // Top-left corner sample lives in the D block.
    let tl_ok = match dref.0 {
        Nb::Curr => true, // (bx-1,by-1) is always earlier
        Nb::None => false,
        w => foreign(w)
            .map(|s| intra_ok(s, pps, map.sid))
            .unwrap_or(false),
    };
    if tl_ok && px > 0 && py > 0 {
        nb.top_left = Some(buf.y[(py - 1) * buf.w + px - 1]);
    }
    nb
}

/// Neighbour samples for a 16x16 luma block (A/B/D foreign only).
fn gather16x16(buf: &FrameBuf, mbs: &[MbState], map: MbMap, pps: &Pps) -> NbSamples {
    let px = map.x * 16;
    let py = map.y * 16;
    let mut nb = NbSamples {
        left: [0; 16],
        top: [0; 16],
        top_right: None,
        top_left: None,
        has_left: false,
        has_top: false,
    };
    if let Some(a) = map.mb_a() {
        if intra_ok(&mbs[a], pps, map.sid) && px > 0 {
            nb.has_left = true;
            for i in 0..16 {
                nb.left[i] = buf.y[(py + i) * buf.w + px - 1];
            }
        }
    }
    if let Some(b) = map.mb_b() {
        if intra_ok(&mbs[b], pps, map.sid) && py > 0 {
            nb.has_top = true;
            for i in 0..16 {
                nb.top[i] = buf.y[(py - 1) * buf.w + px + i];
            }
        }
    }
    if let Some(d) = map.mb_d() {
        if intra_ok(&mbs[d], pps, map.sid) && px > 0 && py > 0 {
            nb.top_left = Some(buf.y[(py - 1) * buf.w + px - 1]);
        }
    }
    nb
}

/// Neighbour samples for an 8x8 chroma block (foreign A/B/D only).
fn gather8x8(plane: &[u8], mbs: &[MbState], map: MbMap, pps: &Pps, cw: usize) -> NbSamples {
    let px = map.x * 8;
    let py = map.y * 8;
    let mut nb = NbSamples {
        left: [0; 16],
        top: [0; 16],
        top_right: None,
        top_left: None,
        has_left: false,
        has_top: false,
    };
    if let Some(a) = map.mb_a() {
        if intra_ok(&mbs[a], pps, map.sid) && px > 0 {
            nb.has_left = true;
            for i in 0..8 {
                nb.left[i] = plane[(py + i) * cw + px - 1];
            }
        }
    }
    if let Some(b) = map.mb_b() {
        if intra_ok(&mbs[b], pps, map.sid) && py > 0 {
            nb.has_top = true;
            for i in 0..8 {
                nb.top[i] = plane[(py - 1) * cw + px + i];
            }
        }
    }
    if let Some(d) = map.mb_d() {
        if intra_ok(&mbs[d], pps, map.sid) && px > 0 && py > 0 {
            nb.top_left = Some(plane[(py - 1) * cw + px - 1]);
        }
    }
    nb
}

fn gather_luma8x8(buf: &FrameBuf, mbs: &[MbState], map: MbMap, g: usize, pps: &Pps) -> NbSamples {
    let gx = (g % 2) * 2;
    let gy = (g / 2) * 2;
    let px = map.x * 16 + gx * 4;
    let py = map.y * 16 + gy * 4;
    let mut nb = NbSamples {
        left: [0; 16],
        top: [0; 16],
        top_right: None,
        top_left: None,
        has_left: false,
        has_top: false,
    };
    let ok = |i: Option<usize>| -> bool {
        i.and_then(|j| mbs.get(j))
            .map(|s| intra_ok(s, pps, map.sid))
            .unwrap_or(false)
    };
    // LEFT column (px-1, py..py+7): group 1/3 read the previous
    // same-MB group (always available — intra MBs reconstruct in
    // group order); group 0/2 read mbA's right edge.
    if g % 2 == 1 || (px > 0 && ok(map.mb_a())) {
        nb.has_left = true;
        for i in 0..8 {
            nb.left[i] = buf.y[(py + i) * buf.w + px - 1];
        }
    }
    // TOP row (px..px+7, py-1): groups 2/3 read the same-MB group
    // above; groups 0/1 read mbB's bottom row.
    if g >= 2 || (py > 0 && ok(map.mb_b())) {
        nb.has_top = true;
        for i in 0..8 {
            nb.top[i] = buf.y[(py - 1) * buf.w + px + i];
        }
    }
    // TOP_LEFT sample (px-1, py-1): g0 -> mbD, g1 -> mbB, g2 -> mbA,
    // g3 -> the same-MB group-0 corner (already reconstructed).
    let tl: Option<u8> = match g {
        0 => {
            if px > 0 && py > 0 && ok(map.mb_d()) {
                Some(buf.y[(py - 1) * buf.w + px - 1])
            } else {
                None
            }
        }
        1 => {
            if py > 0 && ok(map.mb_b()) {
                Some(buf.y[(py - 1) * buf.w + px - 1])
            } else {
                None
            }
        }
        2 => {
            if px > 0 && ok(map.mb_a()) {
                Some(buf.y[(py - 1) * buf.w + px - 1])
            } else {
                None
            }
        }
        _ => Some(buf.y[(py - 1) * buf.w + px - 1]),
    };
    nb.top_left = tl;
    // TOP_RIGHT row (px+8..px+15, py-1): g0 -> mbB's right half;
    // g1 -> mbC's left half; g2 -> same-MB group-1's bottom row
    // (reconstructed already); g3 -> the not-yet-decoded right MB.
    nb.top_right = match g {
        0 => {
            if py > 0 && ok(map.mb_b()) {
                let mut t = [0u8; 16];
                for (i, tv) in t.iter_mut().enumerate().take(8) {
                    *tv = buf.y[(py - 1) * buf.w + px + 8 + i];
                }
                Some(t)
            } else {
                None
            }
        }
        1 => {
            if py > 0 && ok(map.mb_c()) {
                let mut t = [0u8; 16];
                for (i, tv) in t.iter_mut().enumerate().take(8) {
                    *tv = buf.y[(py - 1) * buf.w + px + 8 + i];
                }
                Some(t)
            } else {
                None
            }
        }
        2 => {
            let mut t = [0u8; 16];
            for (i, tv) in t.iter_mut().enumerate().take(8) {
                *tv = buf.y[(py - 1) * buf.w + px + 8 + i];
            }
            Some(t)
        }
        _ => None,
    };
    nb
}

