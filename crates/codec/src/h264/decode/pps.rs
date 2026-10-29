//! Picture parameter set parsing (spec 7.3.2.2).

use super::golomb::Br;
use super::{Error, Result};

/// A parsed picture parameter set.
#[derive(Clone, Debug)]
pub(crate) struct Pps {
    /// `pic_parameter_set_id` (0..=255).
    pub id: u32,
    /// Referenced `seq_parameter_set_id`. Parsed and consumed by the
    /// caller for the SPS↔PPS association; a single-SPS decoder keeps
    /// it for the message only.
    #[allow(dead_code)]
    pub sps_id: u32,
    /// `entropy_coding_mode_flag` — `true` selects CABAC coding for
    /// `macroblock_layer` (spec 9.3); `false` is CAVLC (9.2).
    pub cabac: bool,
    /// `pic_order_present_flag` (bottom-field POC delta in slice headers).
    pub bottom_field_pic_order: bool,
    /// `num_ref_idx_l0_active_minus1` + 1.
    pub num_ref_idx_l0_active: u32,
    /// `num_ref_idx_l1_active_minus1` + 1 (B-slices only); parsed to
    /// keep the syntax complete — this crate refuses B slices before
    /// it could be read.
    #[allow(dead_code)]
    pub num_ref_idx_l1_active: u32,
    /// `weighted_pred_flag` (P-slice weighted prediction).

    /// `weighted_bipred_idc` (B-slices only); parsed for syntax
    /// completeness.
    #[allow(dead_code)]
    pub weighted_bipred_idc: u32,
    /// `pic_init_qp_minus26` + 26.
    pub pic_init_qp: i32,
    /// `pic_init_qs_minus26` + 26 (SP/SI only); parsed for syntax
    /// completeness.
    #[allow(dead_code)]
    pub pic_init_qs: i32,
    /// `chroma_qp_index_offset` (Cb; Cr uses [`Pps::chroma_qp_index_offset_cr`]).
    pub chroma_qp_index_offset: i32,
    /// `second_chroma_qp_index_offset` when the extension tail carries
    /// it; defaults to `chroma_qp_index_offset` (spec 7.4.2.2).
    pub chroma_qp_index_offset_cr: i32,
    /// `deblocking_filter_control_present_flag`.
    pub deblocking_control: bool,
    /// `constrained_intra_pred_flag`.
    pub constrained_intra_pred: bool,
    /// `redundant_pic_cnt_present_flag` — required `false`; parsed to
    /// keep the PPS grammar complete, redundant slices are refused in
    /// the slice header instead.
    #[allow(dead_code)]
    pub redundant_pic_cnt: bool,
    /// `transform_8x8_mode_flag` (extension tail): permits the 8x8
    /// integer transform on luma residual blocks.
    pub transform_8x8_mode: bool,
}

/// Parses a PPS RBSP. `have_more_rbsp` — this crate parses the
/// transform_8x8 tail unconditionally per spec (it is present when
/// `more_rbsp_data` is true).
pub(crate) fn parse(payload: &[u8]) -> Result<Pps> {
    let mut b = Br::new(payload);
    let id = b.ue()?;
    if id > 255 {
        return Err(Error::BadValue("pic_parameter_set_id over 255"));
    }
    let sps_id = b.ue()?;
    if sps_id > 31 {
        return Err(Error::BadValue("pps references sps id over 31"));
    }
    let cabac = b.bit()?;
    if cabac { return Err(Error::Unsupported("CABAC")); }
    let bottom_field_pic_order = b.bit()?;
    let num_slice_groups = b.ue()? + 1;
    if num_slice_groups > 1 {
        return Err(Error::Unsupported("h264 FMO (num_slice_groups_minus1 > 0)"));
    }
    let num_ref_idx_l0_active = b.ue()? + 1;
    let num_ref_idx_l1_active = b.ue()? + 1;
    if num_ref_idx_l0_active > 32 || num_ref_idx_l1_active > 32 {
        return Err(Error::BadValue("num_ref_idx_active over 32"));
    }
    let weighted_pred = b.bit()?;
    let weighted_bipred_idc = b.bits(2)?;
    if weighted_pred || weighted_bipred_idc != 0 { return Err(Error::Unsupported("weighted prediction")); }
    let pic_init_qp = b.se()?.checked_add(26).ok_or(Error::BadValue("initial QP"))?;
    let pic_init_qs = b.se()?.checked_add(26).ok_or(Error::BadValue("initial QS"))?;
    if !(0..=51).contains(&pic_init_qp) || !(0..=51).contains(&pic_init_qs) {
        return Err(Error::BadValue("pic_init_qp/qs out of range"));
    }
    let chroma_qp_index_offset = b.se()?;
    if !(-12..=12).contains(&chroma_qp_index_offset) {
        return Err(Error::BadValue("chroma_qp_index_offset out of range"));
    }
    let deblocking_control = b.bit()?;
    let constrained_intra_pred = b.bit()?;
    let redundant_pic_cnt = b.bit()?;

    let mut second_chroma_offset = chroma_qp_index_offset;
    // Extension tail (more_rbsp_data): transform_8x8_mode_flag,
    // pic_scaling_matrix, second chroma offset. Baseline streams do not
    // send it.
    let mut transform_8x8_mode = false;
    if !b.no_more_rbsp_data() {
        transform_8x8_mode = b.bit()?;
        if transform_8x8_mode { return Err(Error::Unsupported("transform_8x8")); }
        let scaling_matrix = b.bit()?;
        if scaling_matrix {
            // Custom scaling lists change the quantisation tables; the
            // flat-default case is still a refusal because parsing the
            // list syntax without applying it would desync.
            return Err(Error::Unsupported("h264 scaling_matrix_list"));
        }
        second_chroma_offset = b.se()?;
        if !(-12..=12).contains(&second_chroma_offset) {
            return Err(Error::BadValue(
                "second_chroma_qp_index_offset out of range",
            ));
        }
    }

    Ok(Pps {
        id,
        sps_id,
        cabac,
        bottom_field_pic_order,
        num_ref_idx_l0_active,
        num_ref_idx_l1_active,
        weighted_bipred_idc,
        pic_init_qp,
        pic_init_qs,
        chroma_qp_index_offset,
        chroma_qp_index_offset_cr: second_chroma_offset,
        deblocking_control,
        constrained_intra_pred,
        redundant_pic_cnt,
        transform_8x8_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct W {
        bits: std::vec::Vec<u8>,
    }

    impl W {
        fn bit(&mut self, b: u8) {
            self.bits.push(b & 1);
        }
        fn ue(&mut self, v: u32) {
            let vp1 = v + 1;
            let n = 31 - vp1.leading_zeros();
            for _ in 0..n {
                self.bit(0);
            }
            for i in (0..=n).rev() {
                self.bit(((vp1 >> i) & 1) as u8);
            }
        }
        fn se(&mut self, v: i32) {
            let m = if v <= 0 {
                (-v * 2) as u32
            } else {
                (v * 2 - 1) as u32
            };
            self.ue(m);
        }
    }

    fn pps_with(id: u32, sps: u32, qp: i32, chroma: i32) -> Result<Pps> {
        let mut w = W {
            bits: std::vec::Vec::new(),
        };
        w.ue(id);
        w.ue(sps);
        w.bit(0); // entropy cabac
        w.bit(0); // bottom field pic order
        w.ue(0); // slice groups -1
        w.ue(0); // l0 -1
        w.ue(0); // l1 -1
        w.bit(0); // weighted pred
        w.bit(0);
        w.bit(0); // weighted bipred
        w.se(qp); // pic_init_qp
        w.se(0); // pic_init_qs
        w.se(chroma);
        w.bit(0);
        w.bit(0);
        w.bit(0);
        let mut b = std::vec![0u8; w.bits.len().div_ceil(8)];
        for (i, &bit) in w.bits.iter().enumerate() {
            b[i / 8] |= bit << (7 - i % 8);
        }
        b.push(0x80);
        parse(&b)
    }

    #[test]
    fn pps_field_bounds_reject() {
        assert!(pps_with(256, 0, 0, 0).is_err());
        assert!(pps_with(0, 32, 0, 0).is_err());
        assert!(pps_with(0, 0, -30, 0).is_err());
        assert!(pps_with(0, 0, 0, 13).is_err());
        assert!(pps_with(0, 0, 0, 0).is_ok());
    }
}
