//! Bounded frame-picture slice headers and reference marking commands.

use super::{golomb::Br, pps::Pps, sps::Sps, Error, Result};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SliceType { P, I }

#[derive(Copy, Clone, Debug)]
pub(crate) struct Mmco {
    pub op: u8,
    pub difference_of_pic_nums: u32,
    pub long_term_pic_num: u32,
    pub long_term_frame_idx: u32,
    pub max_long_term_frame_idx: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct SliceHeader {
    pub first_mb: u32,
    pub redundant_pic_cnt: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_poc_bottom: i32,
    pub delta_poc0: i32,
    pub delta_poc1: i32,
    pub num_ref_idx_l0_active: u32,
    pub reorder_l0: Vec<(u32, u32)>,
    pub idr_marking: (bool, bool),
    pub adaptive_marking: bool,
    pub mmco: Vec<Mmco>,
    pub slice_qp_delta: i32,
    pub disable_deblock_idc: u8,
    pub offset_a: i8,
    pub offset_b: i8,
}

pub(crate) fn parse_header(br: &mut Br<'_>, idr: bool, nal_ref_idc: u8, sps: &Sps, pps: &Pps) -> Result<SliceHeader> {
    let first_mb = br.ue()?;
    if first_mb >= sps.width_mbs * sps.height_mbs { return Err(Error::BadValue("first_mb_in_slice past picture")); }
    let slice_type = match br.ue()? {
        0 | 5 if !idr => SliceType::P,
        2 | 7 => SliceType::I,
        1 | 6 => return Err(Error::Unsupported("B slices")),
        3 | 8 => return Err(Error::Unsupported("SP slices")),
        4 | 9 => return Err(Error::Unsupported("SI slices")),
        _ => return Err(Error::BadValue("slice type or IDR P slice")),
    };
    let pps_id = br.ue()?;
    if pps_id != pps.id { return Err(Error::BadValue("slice references a different PPS")); }
    let frame_num = br.bits(sps.log2_max_frame_num as usize)?;
    let idr_pic_id = if idr { br.ue()? } else { 0 };
    if idr_pic_id > 65535 || (idr && (frame_num != 0 || nal_ref_idc == 0)) { return Err(Error::BadValue("IDR identity")); }
    let (mut pic_order_cnt_lsb, mut delta_poc_bottom, mut delta_poc0, mut delta_poc1) = (0,0,0,0);
    match sps.pic_order_cnt_type {
        0 => {
            pic_order_cnt_lsb = br.bits(sps.log2_max_poc_lsb as usize)?;
            if pps.bottom_field_pic_order { delta_poc_bottom = br.se()?; }
        }
        1 if !sps.delta_pic_order_always_zero => {
            delta_poc0 = br.se()?;
            if pps.bottom_field_pic_order { delta_poc1 = br.se()?; }
        }
        _ => {}
    }
    let redundant_pic_cnt = if pps.redundant_pic_cnt { br.ue()? } else { 0 };
    if redundant_pic_cnt > 127 { return Err(Error::BadValue("redundant_pic_cnt over 127")); }
    let mut num_ref_idx_l0_active = pps.num_ref_idx_l0_active;
    if slice_type == SliceType::P && br.bit()? {
        let minus_one = br.ue()?;
        if minus_one > 31 { return Err(Error::BadValue("active reference count")); }
        num_ref_idx_l0_active = minus_one + 1;
    }
    let mut reorder_l0 = Vec::new();
    if slice_type == SliceType::P && br.bit()? {
        loop {
            let idc = br.ue()?;
            if idc == 3 { break; }
            if idc > 3 || reorder_l0.len() >= num_ref_idx_l0_active as usize { return Err(Error::BadValue("reference-list modification")); }
            reorder_l0.push((idc, br.ue()?));
        }
    }
    let mut idr_marking = (false,false);
    let mut adaptive_marking = false;
    let mut mmco = Vec::new();
    if nal_ref_idc != 0 {
        if idr { idr_marking = (br.bit()?, br.bit()?); }
        else {
            adaptive_marking = br.bit()?;
            if adaptive_marking {
                loop {
                    let op = br.ue()?;
                    if op == 0 { break; }
                    if op > 6 || mmco.len() >= 64 { return Err(Error::BadValue("MMCO command list")); }
                    let mut m = Mmco { op: op as u8, difference_of_pic_nums: 0, long_term_pic_num: 0, long_term_frame_idx: 0, max_long_term_frame_idx: 0 };
                    if op == 1 || op == 3 { m.difference_of_pic_nums = br.ue()?.checked_add(1).ok_or(Error::BadValue("MMCO difference"))?; }
                    if op == 2 { m.long_term_pic_num = br.ue()?; }
                    if op == 3 || op == 6 {
                        m.long_term_frame_idx = br.ue()?;
                        if m.long_term_frame_idx > 15 { return Err(Error::BadValue("long-term frame index")); }
                    }
                    if op == 4 {
                        m.max_long_term_frame_idx = br.ue()?;
                        if m.max_long_term_frame_idx > 16 { return Err(Error::BadValue("maximum long-term frame index")); }
                    }
                    mmco.push(m);
                }
            }
        }
    }
    let slice_qp_delta = br.se()?;
    if !(0..=51).contains(&(i64::from(pps.pic_init_qp) + i64::from(slice_qp_delta))) { return Err(Error::BadValue("slice QP out of range")); }
    let mut disable_deblock_idc = 0;
    let (mut offset_a,mut offset_b) = (0,0);
    if pps.deblocking_control {
        let idc = br.ue()?;
        if idc > 2 { return Err(Error::BadValue("deblocking idc")); }
        disable_deblock_idc = idc as u8;
        if idc != 1 {
            let a = br.se()?; let b = br.se()?;
            if !(-6..=6).contains(&a) || !(-6..=6).contains(&b) { return Err(Error::BadValue("deblocking offset")); }
            offset_a = (a * 2) as i8; offset_b = (b * 2) as i8;
        }
    }
    Ok(SliceHeader { first_mb, redundant_pic_cnt, slice_type, pps_id, frame_num, idr_pic_id, pic_order_cnt_lsb, delta_poc_bottom, delta_poc0, delta_poc1, num_ref_idx_l0_active, reorder_l0, idr_marking, adaptive_marking, mmco, slice_qp_delta, disable_deblock_idc, offset_a, offset_b })
}
