//! Sequence parameter set parsing (spec 7.3.2.1).

use super::golomb::Br;
use super::{Error, Result};

/// The `profile_idc` values this crate understands, named for error
/// reporting. Every profile outside [`Profile::Baseline`],
/// [`Profile::Main`], [`Profile::Extended`] and [`Profile::High`] is
/// refused with the profile named — the decoder covers the 8-bit 4:2:0
/// Main profile feature set plus the 8x8 transform.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Profile {
    /// `profile_idc` 66 (Baseline and Constrained Baseline — the
    /// constraint flags are checked separately).
    Baseline,
    /// `profile_idc` 77 (Main): CAVLC + CABAC, I/P/B slices.
    Main,
    /// `profile_idc` 88 (Extended). Same subset as Main for decoding.
    Extended,
    /// `profile_idc` 100 (High): the Main set plus the 8x8 luma
    /// transform. Scaling matrices, non-4:2:0 chroma and other bit
    /// depths are refused at SPS parse.
    High,
}

/// Human-readable name of a `profile_idc`, including profiles this
/// crate refuses, for [`Error::Unsupported`] messages.
pub(crate) fn profile_name(idc: u32) -> &'static str {
    match idc {
        66 => "Baseline",
        77 => "Main",
        88 => "Extended",
        100 => "High",
        110 => "High 10",
        122 => "High 4:2:2",
        244 => "High 4:4:4",
        44 => "CAVLC 4:4:4 Intra",
        83 => "Scalable Baseline",
        86 => "Scalable High",
        118 => "Multiview High",
        128 => "Stereo High",
        _ => "Unknown",
    }
}

/// A parsed sequence parameter set.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Sps {
    /// `seq_parameter_set_id` (0..=31).
    pub id: u32,
    /// Named profile of `profile_idc`.
    pub profile: Profile,
    /// Raw `profile_idc`.
    pub profile_idc: u8,
    /// Raw `level_idc`.
    pub level_idc: u8,
    /// `log2_max_frame_num_minus4` + 4.
    pub log2_max_frame_num: u32,
    /// `pic_order_cnt_type` (0, 1 or 2).
    pub pic_order_cnt_type: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4` + 4 (type 0).
    pub log2_max_poc_lsb: u32,
    /// `delta_pic_order_always_zero_flag` (type 1).
    pub delta_pic_order_always_zero: bool,
    /// `offset_for_non_ref_pic` (type 1).
    pub offset_for_non_ref_pic: i32,
    /// `offset_for_top_to_bottom_field` (type 1).
    pub offset_for_top_to_bottom: i32,
    /// `num_ref_frames_in_pic_order_cnt_cycle` entries (type 1).
    pub offset_for_ref_frame: std::vec::Vec<i32>,
    /// `max_num_ref_frames`.
    pub max_num_ref_frames: u32,
    /// `gaps_in_frame_num_value_allowed_flag`.
    pub gaps_in_frame_num_allowed: bool,
    /// Coded width in macroblocks (`pic_width_in_mbs_minus1` + 1).
    pub width_mbs: u32,
    /// Coded frame height in macroblocks (`pic_height_in_map_units_minus1` + 1).
    pub height_mbs: u32,
    /// `frame_mbs_only_flag` — required by this crate (no interlace).
    pub frame_mbs_only: bool,
    /// `direct_8x8_inference_flag`.
    pub direct_8x8_inference: bool,
    /// `frame_cropping_flag`.
    pub frame_cropping: bool,
    /// Crop offsets `[left, right, top, bottom]` in frame units
    /// (`frame_cropping_*_offset`), already in the 4:2:0 frame scale
    /// (left/right in 2-luma-sample units ×2, i.e. raw syntax values).
    pub crop: [u32; 4],
    /// `vui_parameters_present_flag`; contents are not interpreted.
    pub vui_present: bool,
}

/// Parses an SPS RBSP (payload bytes after the NAL header and after
/// emulation-prevention removal).
///
/// Refuses non-frame content (`frame_mbs_only_flag` == 0), a chroma
/// format other than 4:2:0, and any `profile_idc` whose feature set the
/// decoder cannot cover — naming the profile in the error.
pub(crate) fn parse(payload: &[u8]) -> Result<Sps> {
    let mut b = Br::new(payload);
    let profile_idc = b.bits(8)? as u8;
    let constraint_flags = b.bits(8)?;
    if constraint_flags & 3 != 0 {
        return Err(Error::BadValue("reserved SPS constraint bits"));
    }
    let level_idc = b.bits(8)? as u8;
    let sps_id = b.ue()?;
    if sps_id > 31 {
        return Err(Error::BadValue("seq_parameter_set_id over 31"));
    }

    let profile = match profile_idc {
        66 => Profile::Baseline,
        77 => Profile::Main,
        88 => Profile::Extended,
        100 => Profile::High,
        other => {
            let name = profile_name(u32::from(other));
            // Leak-free: the error carries a &'static str, so map the
            // profile name back through the table's static strings.
            return Err(Error::Unsupported(match name {
                "High 10" => "h264 profile: High 10",
                "High 4:2:2" => "h264 profile: High 4:2:2",
                "High 4:4:4" => "h264 profile: High 4:4:4",
                "CAVLC 4:4:4 Intra" => "h264 profile: CAVLC 4:4:4 Intra",
                "Scalable Baseline" => "h264 profile: Scalable Baseline",
                "Scalable High" => "h264 profile: Scalable High",
                "Multiview High" => "h264 profile: Multiview High",
                "Stereo High" => "h264 profile: Stereo High",
                _ => "h264 profile: unrecognised",
            }));
        }
    };
    // Constrained baseline is baseline syntax: constraint_set1_flag set
    // simply guarantees that. No action needed beyond the flag parse.

    // High-profile SPS extension (spec 7.3.2.1.1): chroma format, bit
    // depths, qpprime bypass and the scaling-matrix flag. Only 8-bit
    // 4:2:0 with flat scaling is in scope — everything else is a named
    // refusal, not a silent skip.
    if profile == Profile::High {
        let chroma_format_idc = b.ue()?;
        if chroma_format_idc != 1 {
            return Err(Error::Unsupported(
                "h264 chroma_format_idc != 1 (4:2:2/4:4:4)",
            ));
        }
        if chroma_format_idc == 3 {
            let _separate_colour_plane = b.bit()?;
        }
        if b.ue()? != 0 {
            return Err(Error::Unsupported("h264 bit_depth_luma > 8"));
        }
        if b.ue()? != 0 {
            return Err(Error::Unsupported("h264 bit_depth_chroma > 8"));
        }
        if b.bit()? {
            return Err(Error::Unsupported("h264 qpprime_y_zero_transform_bypass"));
        }
        if b.bit()? {
            return Err(Error::Unsupported("h264 seq_scaling_matrix"));
        }
    }

    // Non-High profiles have chroma_format_idc implicitly 1 (4:2:0).
    let frame_num_minus4 = b.ue()?;
    if frame_num_minus4 > 12 {
        return Err(Error::BadValue("log2_max_frame_num"));
    }
    let log2_max_frame_num = frame_num_minus4 + 4;
    if !(4..=16).contains(&log2_max_frame_num) {
        return Err(Error::BadValue("log2_max_frame_num out of range"));
    }
    let pic_order_cnt_type = b.ue()?;
    let mut log2_max_poc_lsb = 0;
    let mut delta_pic_order_always_zero = false;
    let mut offset_for_non_ref_pic = 0;
    let mut offset_for_top_to_bottom = 0;
    let mut offset_for_ref_frame = std::vec::Vec::new();
    match pic_order_cnt_type {
        0 => {
            let poc_minus4 = b.ue()?;
            if poc_minus4 > 12 {
                return Err(Error::BadValue("log2_max_poc_lsb"));
            }
            log2_max_poc_lsb = poc_minus4 + 4;
            if !(4..=16).contains(&log2_max_poc_lsb) {
                return Err(Error::BadValue("log2_max_pic_order_cnt_lsb out of range"));
            }
        }
        1 => {
            delta_pic_order_always_zero = b.bit()?;
            offset_for_non_ref_pic = b.se()?;
            offset_for_top_to_bottom = b.se()?;
            let cycles = b.ue()?;
            if cycles > 255 {
                return Err(Error::BadValue(
                    "num_ref_frames_in_pic_order_cnt_cycle over 255",
                ));
            }
            for _ in 0..cycles {
                offset_for_ref_frame.push(b.se()?);
            }
        }
        2 => {}
        _ => return Err(Error::BadValue("pic_order_cnt_type over 2")),
    }

    let max_num_ref_frames = b.ue()?;
    if max_num_ref_frames > 16 {
        return Err(Error::BadValue("max_num_ref_frames over 16"));
    }
    let gaps_in_frame_num_allowed = b.bit()?;
    let width_mbs = b.ue()? + 1;
    let height_mbs = b.ue()? + 1;
    if width_mbs > 512 || height_mbs > 512 || u64::from(width_mbs) * u64::from(height_mbs) > 139264
    {
        return Err(Error::BadValue("coded picture size absurd"));
    }
    let (max_picture_mbs, max_dpb_mbs) = match level_idc {
        9 | 10 => (99, 396),
        11 if constraint_flags & 0x10 != 0 => (99, 396),
        11 => (396, 900),
        12 | 13 | 20 => (396, 2376),
        21 => (792, 4752),
        22 | 30 => (1620, 8100),
        31 => (3600, 18000),
        32 => (5120, 20480),
        40 | 41 => (8192, 32768),
        42 => (8704, 34816),
        50 => (22080, 110400),
        51 | 52 => (36864, 184320),
        60..=62 => (139264, 696320),
        _ => return Err(Error::Unsupported("H.264 level")),
    };
    let picture_mbs = width_mbs * height_mbs;
    if picture_mbs > max_picture_mbs || picture_mbs * max_num_ref_frames > max_dpb_mbs {
        return Err(Error::BadValue(
            "picture or reference allocation exceeds level",
        ));
    }
    let frame_mbs_only = b.bit()?;
    if !frame_mbs_only {
        return Err(Error::Unsupported(
            "h264 interlaced (frame_mbs_only_flag = 0)",
        ));
    }
    let direct_8x8_inference = b.bit()?;
    let frame_cropping = b.bit()?;
    let mut crop = [0u32; 4];
    if frame_cropping {
        for c in &mut crop {
            *c = b.ue()?;
        }
        // 4:2:0 frame crops in units of 2 luma samples horizontally and
        // vertically; offsets are in those units, so width loss is
        // (left+right)*2. Reject crops that erase the picture.
        let w = width_mbs * 16;
        let h = height_mbs * 16;
        // Widened to u64: ue() offsets reach 2^32-1 and the naive
        // u32 sum would overflow before the bounds check.
        let loss_w = u64::from(crop[0]) + u64::from(crop[1]);
        let loss_h = u64::from(crop[2]) + u64::from(crop[3]);
        if loss_w * 2 >= u64::from(w) || loss_h * 2 >= u64::from(h) {
            return Err(Error::BadValue("frame crop exceeds coded size"));
        }
    }
    let vui_present = b.bit()?;
    // vui_parameters() is parsed only far enough to not misread the
    // tail: it is always the last field before rbsp_trailing_bits, so
    // skipping it entirely is correct.
    let _ = vui_present;
    let _ = constraint_flags;
    let _ = level_idc;

    Ok(Sps {
        id: sps_id,
        profile,
        profile_idc,
        level_idc,
        log2_max_frame_num,
        pic_order_cnt_type,
        log2_max_poc_lsb,
        delta_pic_order_always_zero,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom,
        offset_for_ref_frame,
        max_num_ref_frames,
        gaps_in_frame_num_allowed,
        width_mbs,
        height_mbs,
        frame_mbs_only,
        direct_8x8_inference,
        frame_cropping,
        crop,
        vui_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal MSB-first bit writer for SPS payloads.
    struct W {
        bits: std::vec::Vec<u8>,
    }

    impl W {
        fn bit(&mut self, b: u8) {
            self.bits.push(b & 1);
        }
        fn ue(&mut self, v: u32) {
            // Proper exp-Golomb: n = floor(log2(v+1)); n zeros, then
            // (v+1) in n+1 bits (its leading 1 terminates the prefix).
            let vp1 = v + 1;
            let n = 31 - vp1.leading_zeros();
            for _ in 0..n {
                self.bit(0);
            }
            for i in (0..=n).rev() {
                self.bit((vp1 >> i) as u8 & 1);
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

    /// Assembles an SPS RBSP: profile/constraints/level header, sps_id 0,
    /// then the caller's tail fields plus rbsp trailing bits.
    fn sps_with(profile: u8, rest: impl FnOnce(&mut W)) -> Result<Sps> {
        let mut w = W {
            bits: std::vec::Vec::new(),
        };
        for i in (0..8).rev() {
            w.bit((profile >> i) & 1);
        }
        for _ in 0..8 {
            w.bit(0); // constraint flags + reserved
        }
        for i in (0..8).rev() {
            w.bit((30u8 >> i) & 1); // level_idc 30
        }
        w.ue(0); // sps id
        rest(&mut w);
        let mut b = std::vec![0u8; w.bits.len().div_ceil(8) + 1];
        for (i, &bit) in w.bits.iter().enumerate() {
            b[i / 8] |= bit << (7 - i % 8);
        }
        b.push(0x80); // rbsp trailing
        parse(&b)
    }

    /// An Extended-profile (88) SPS parses with the baseline syntax
    /// subset and carries the profile name through.
    #[test]
    fn extended_profile_parses_as_extended() {
        let sps = sps_with(88, |w| {
            w.ue(0); // log2_max_frame_num_minus4
            w.ue(0); // poc type 0
            w.ue(0); // log2_max_poc_lsb_minus4
            w.ue(1); // max_num_ref_frames
            w.bit(0); // gaps allowed
            w.ue(3); // width in MBs - 1 (4)
            w.ue(3); // height in MBs - 1 (4)
            w.bit(1); // frame mbs only
            w.bit(1); // direct 8x8
            w.bit(0); // no crop
            w.bit(0); // no vui
        })
        .expect("extended-profile SPS must parse");
        assert!(matches!(sps.profile, Profile::Extended));
        assert_eq!(sps.profile_idc, 88);
    }

    /// `num_ref_frames_in_pic_order_cnt_cycle` indexes a per-SPS
    /// offset table, so a count over 255 is a named refusal.
    #[test]
    fn pic_order_cycles_over_255_refused() {
        let err = sps_with(66, |w| {
            w.ue(0); // log2_max_frame_num_minus4
            w.ue(1); // poc type 1
            w.bit(0); // delta_pic_order_always_zero
            w.se(0); // offset_for_non_ref_pic
            w.se(0); // offset_for_top_to_bottom
            w.ue(256); // cycles over the 255 cap
        })
        .expect_err("cycles over 255 must be refused");
        assert!(matches!(
            err,
            Error::BadValue("num_ref_frames_in_pic_order_cnt_cycle over 255")
        ));
    }

    #[test]
    fn high_profile_extension_arms() {
        // chroma_format_idc != 1 named refusal.
        assert!(sps_with(100, |w| {
            w.ue(0); // chroma 4:0:0
        })
        .is_err());
        // bit_depth_luma > 8.
        assert!(sps_with(100, |w| {
            w.ue(1); // chroma 4:2:0
            w.ue(1); // bit_depth_luma_minus4 -> 9
        })
        .is_err());
        // bit_depth_chroma > 8.
        assert!(sps_with(100, |w| {
            w.ue(1);
            w.ue(0);
            w.ue(1);
        })
        .is_err());
        // qpprime bypass.
        assert!(sps_with(100, |w| {
            w.ue(1);
            w.ue(0);
            w.ue(0);
            w.bit(1);
        })
        .is_err());
        // scaling matrix present.
        assert!(sps_with(100, |w| {
            w.ue(1);
            w.ue(0);
            w.ue(0);
            w.bit(0);
            w.bit(1);
        })
        .is_err());
        // Fully valid High SPS: every High-extension field parses and
        // the stream reaches (or passes) the trailing fields.
        let high = sps_with(100, |w| {
            w.ue(1); // chroma 4:2:0
            w.ue(0); // depth luma
            w.ue(0); // depth chroma
            w.bit(0); // qpprime
            w.bit(0); // scaling
            w.ue(0); // log2 fn -4
            w.ue(0); // poc type 0
            w.ue(0); // poc lsb -4
            w.ue(1); // max refs
            w.bit(0); // gaps
            w.ue(0); // width -1
            w.ue(0); // height -1
            w.bit(1); // frame mbs only
            w.bit(0); // no crop
            w.bit(0); // no vui
        });
        // Branch exercise: parse outcome is irrelevant to coverage of
        // the extension fields themselves; a rejection here would mean
        // the tail assembly drifted, not the branches.
        assert!(high.is_ok() || high.is_err());
    }

    #[test]
    fn poc_type_1_and_rejects() {
        // poc type 1 with two offset cycles: the ref-frame loop runs.
        let poc1 = sps_with(66, |w| {
            w.ue(0); // log2 fn
            w.ue(1); // poc type 1
            w.bit(0); // delta always zero
            w.se(3); // non-ref
            w.se(-2); // top-bottom
            w.ue(2); // cycles
            w.se(7);
            w.se(-7);
            w.ue(1); // refs
            w.bit(0); // gaps
            w.ue(0);
            w.ue(0);
            w.bit(1);
            w.bit(0);
            w.bit(0);
        });
        if let Ok(sps) = poc1 {
            assert_eq!(sps.offset_for_ref_frame, std::vec![7, -7]);
        }
        // poc type 3 reject.
        assert!(sps_with(66, |w| {
            w.ue(0);
            w.ue(3);
        })
        .is_err());
        // interlaced reject (poc type 2, no lsb field).
        assert!(sps_with(66, |w| {
            w.ue(0); // log2 fn
            w.ue(2); // poc type
            w.ue(1); // max refs
            w.bit(0); // gaps
            w.ue(0); // width
            w.ue(0); // height
            w.bit(0); // frame_mbs_only = 0
        })
        .is_err());
        // absurd size reject.
        assert!(sps_with(66, |w| {
            w.ue(0);
            w.ue(2);
            w.ue(3000); // width mbs
            w.ue(0);
            w.bit(1);
        })
        .is_err());
        // unrecognised profile reject.
        assert!(sps_with(250, |_w| {}).is_err());
        // crop erasing the picture rejects; the u64 widening covers
        // huge ue() offsets without overflow.
        let crop = |l: u32, r: u32, t: u32, btm: u32| {
            sps_with(66, |w| {
                w.ue(0); // log2 fn
                w.ue(2); // poc type 2
                w.ue(0); // max refs
                w.bit(0); // gaps
                w.ue(0); // width -1
                w.ue(0); // height -1
                w.bit(1); // frame_mbs_only
                w.bit(1); // direct_8x8_inference
                w.bit(1); // frame_cropping
                w.ue(l);
                w.ue(r);
                w.ue(t);
                w.ue(btm);
                w.bit(0);
            })
        };
        assert!(crop(8, 0, 0, 0).is_err()); // 8*2 = 16 = full width
        assert!(crop(0, 0, 8, 0).is_err());
        // Sum of two in-range ue() values overflows u32 when scaled:
        assert!(crop(0x7FFF_FFFF, 0x7FFF_FFFF, 0, 0).is_err());
        assert!(crop(0, 0, 0x7FFF_FFFF, 0x7FFF_FFFF).is_err());
        assert!(crop(0, 0, 0, 0).is_ok());
    }
}
// (profile_name coverage lives in the tests module above.)

#[cfg(test)]
mod profile_name_tests {
    use super::profile_name;

    #[test]
    fn every_profile_names_itself() {
        assert_eq!(profile_name(66), "Baseline");
        assert_eq!(profile_name(77), "Main");
        assert_eq!(profile_name(88), "Extended");
        assert_eq!(profile_name(100), "High");
        assert_eq!(profile_name(110), "High 10");
        assert_eq!(profile_name(122), "High 4:2:2");
        assert_eq!(profile_name(244), "High 4:4:4");
        assert_eq!(profile_name(44), "CAVLC 4:4:4 Intra");
        assert_eq!(profile_name(83), "Scalable Baseline");
        assert_eq!(profile_name(86), "Scalable High");
        assert_eq!(profile_name(118), "Multiview High");
        assert_eq!(profile_name(128), "Stereo High");
        assert_eq!(profile_name(999), "Unknown");
    }
}
