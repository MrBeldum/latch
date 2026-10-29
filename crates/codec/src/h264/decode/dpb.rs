//! Decoded picture buffer: reference marking (sliding window + MMCO
//! ops 1–4), PicNum derivation, and RefPicList0 initialisation plus
//! slice reordering (spec 8.2.4).
//!
//! Short-term and long-term frames are tracked: MMCO op 3 moves a
//! short-term picture to a long-term slot (spec 8.2.5.4.3, including
//! the same-`LongTermFrameIdx` displacement and sliding-window
//! interplay), and long-term pictures join the reference lists after
//! every short-term picture. The remaining long-term assignments —
//! MMCO op 6 (current picture to long-term), op 4 caps and long-term
//! list reordering — surface as [`Error::Unsupported`] rather than
//! silently marking wrong, as does `long_term_reference_flag` on IDR
//! (refused at parse).

use super::slice::SliceHeader;
use super::{Error, Result};
use std::vec::Vec;

/// One reference picture in the DPB: planes + PicNum bookkeeping.
#[derive(Clone, Debug)]
pub(crate) struct RefFrame {
    /// `frame_num` the picture was coded with.
    pub frame_num: u32,
    /// `PicNum` of the *current* decode pass: `FrameNum` adjusted by
    /// `FrameNumWrap` (spec 8.2.4.1); recomputed per slice.
    pub pic_num: i32,

    /// Luma plane (coded width × coded height, row-major).
    pub y: Vec<u8>,
    pub width: usize,
    /// Cb plane.
    pub cb: Vec<u8>,
    /// Cr plane.
    pub cr: Vec<u8>,
    /// Long-term marking (spec 8.2.5.4.3): `Some(LongTermFrameIdx)`
    /// once an MMCO op 3 moved the picture out of the short-term set;
    /// long-term pictures survive the sliding window and join the
    /// reference lists after every short-term picture.
    pub long_term: Option<u32>,
}

/// The DPB: `refs` is the set of short-term reference frames plus the
/// sliding-window/marked bookkeeping.
#[derive(Default)]
pub(crate) struct Dpb {
    /// Short-term reference frames, insertion order = decode order.
    pub refs: Vec<RefFrame>,
    /// `MaxFrameNum` = `1 << log2_max_frame_num` of the active SPS.
    pub max_frame_num: u32,
    /// `max_num_ref_frames` honoured (min of SPS and Limits).
    pub max_refs: usize,
    max_long_term_idx: Option<u32>,
}

impl Dpb {
    /// DPB state for a new SPS (or IDR): everything is dropped, sizes
    /// updated.
    pub(crate) fn reset(&mut self, max_frame_num: u32, max_refs: u32, limit: u32) {
        self.refs.clear();
        self.max_long_term_idx = None;
        self.max_frame_num = max_frame_num.max(1);
        self.max_refs = (max_refs.min(limit).max(1)) as usize;
    }

    /// IDR boundary: mark every reference unused-for-reference before
    /// the new picture (spec 8.2.5.2; `no_output_of_prior_pics`/`long
    /// term_reference_flag` variations don't apply — we reject the
    /// long-term flag at parse and output doesn't interleave with DPB).
    pub(crate) fn flush(&mut self) {
        self.refs.clear();
    }

    /// Recomputes `PicNum` for every stored reference against the
    /// current picture's `frame_num` (spec 8.2.4.1: PicNum = frame_num
    /// − MaxFrameNum when frame_num > FrameNum, else frame_num).
    /// `FrameNumWrap` applies to short-term pictures only — a long-term
    /// picture keeps `PicNum == frame_num`.
    fn set_pic_nums(&mut self, frame_num: u32) {
        for r in self.refs.iter_mut() {
            r.pic_num = if r.long_term.is_none() && r.frame_num > frame_num {
                r.frame_num as i32 - self.max_frame_num as i32
            } else {
                r.frame_num as i32
            };
        }
    }

    /// Applies this slice's reference-picture marking after the picture
    /// is decoded. `cur` is the just-finished frame (already in `refs`
    /// via [`push`]).
    pub(crate) fn apply_marking(
        &mut self,
        h: &SliceHeader,
        frame_num: u32,
        nal_ref_idc: u8,
    ) -> Result<Option<u32>> {
        if nal_ref_idc == 0 {
            return Ok(None);
        }
        self.set_pic_nums(frame_num);
        let mut current_long_term = None;
        if h.idr_marking.1 {
            self.refs.clear();
            self.max_long_term_idx = Some(0);
            current_long_term = Some(0);
        } else if h.adaptive_marking {
            for m in &h.mmco {
                let target = (i64::from(frame_num) - i64::from(m.difference_of_pic_nums))
                    .rem_euclid(i64::from(self.max_frame_num)) as u32;
                match m.op {
                    1 => {
                        let pos = self
                            .refs
                            .iter()
                            .position(|r| r.long_term.is_none() && r.frame_num == target)
                            .ok_or(Error::MissingReference)?;
                        self.refs.remove(pos);
                    }
                    2 => {
                        let pos = self
                            .refs
                            .iter()
                            .position(|r| r.long_term == Some(m.long_term_pic_num))
                            .ok_or(Error::MissingReference)?;
                        self.refs.remove(pos);
                    }
                    3 => {
                        if self
                            .max_long_term_idx
                            .is_none_or(|max| m.long_term_frame_idx > max)
                        {
                            return Err(Error::BadValue("MMCO long-term index exceeds maximum"));
                        }
                        self.refs
                            .retain(|r| r.long_term != Some(m.long_term_frame_idx));
                        let r = self
                            .refs
                            .iter_mut()
                            .find(|r| r.long_term.is_none() && r.frame_num == target)
                            .ok_or(Error::MissingReference)?;
                        r.long_term = Some(m.long_term_frame_idx);
                    }
                    4 => {
                        self.max_long_term_idx = m.max_long_term_frame_idx.checked_sub(1);
                        self.refs.retain(|r| {
                            r.long_term.is_none_or(|idx| {
                                self.max_long_term_idx.is_some_and(|max| idx <= max)
                            })
                        });
                    }
                    5 => {
                        self.refs.clear();
                        self.max_long_term_idx = None;
                    }
                    6 => {
                        if self
                            .max_long_term_idx
                            .is_none_or(|max| m.long_term_frame_idx > max)
                        {
                            return Err(Error::BadValue(
                                "MMCO current long-term index exceeds maximum",
                            ));
                        }
                        self.refs
                            .retain(|r| r.long_term != Some(m.long_term_frame_idx));
                        current_long_term = Some(m.long_term_frame_idx);
                    }
                    _ => return Err(Error::BadValue("MMCO operation")),
                }
            }
        } else {
            self.make_room()?;
        }
        if self.refs.len() >= self.max_refs {
            return Err(Error::BadValue("reference marking exceeds DPB"));
        }
        Ok(current_long_term)
    }

    /// Sliding-window marking includes long-term pictures in the capacity,
    /// but evicts only the oldest short-term FrameNumWrap.
    pub(crate) fn make_room(&mut self) -> Result<()> {
        if self.refs.len() >= self.max_refs {
            let oldest = self
                .refs
                .iter()
                .enumerate()
                .filter(|(_, r)| r.long_term.is_none())
                .min_by_key(|(_, r)| r.pic_num)
                .map(|(i, _)| i)
                .ok_or(Error::BadValue("DPB has no short-term picture to evict"))?;
            self.refs.remove(oldest);
        }
        Ok(())
    }

    pub(crate) fn gap(&mut self, frame_num: u32) -> Result<()> {
        self.set_pic_nums(frame_num);
        self.make_room()?;
        self.push(frame_num, Vec::new(), Vec::new(), Vec::new(), 0);
        Ok(())
    }

    /// Inserts the finished frame as a reference (caller skips when
    /// `nal_ref_idc == 0`) and returns its index.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push(
        &mut self,
        frame_num: u32,
        y: Vec<u8>,
        cb: Vec<u8>,
        cr: Vec<u8>,
        width: usize,
    ) -> usize {
        self.refs.push(RefFrame {
            frame_num,
            pic_num: frame_num as i32,
            y,
            width,
            cb,
            cr,
            long_term: None,
        });
        self.refs.len() - 1
    }

    /// Builds the initial RefPicList0 for a P slice and applies
    /// the slice's reordering commands (spec 8.2.4.2 + 8.2.4.3.1).
    ///
    /// Returns indices into `self.refs` in list order.
    pub(crate) fn ref_list0(&mut self, h: &SliceHeader, frame_num: u32) -> Result<Vec<usize>> {
        self.set_pic_nums(frame_num);
        // Short-term first (spec 8.2.4.2.1), long-term after (spec 8.2.4.2.3):
        // P slices order the short-term set by PicNum descending; long-term pictures
        // then follow in ascending LongTermFrameIdx.
        let mut st: Vec<usize> = (0..self.refs.len())
            .filter(|&i| self.refs[i].long_term.is_none())
            .collect();
        let mut lt: Vec<usize> = (0..self.refs.len())
            .filter(|&i| self.refs[i].long_term.is_some())
            .collect();
        st.sort_by(|&a, &b| self.refs[b].pic_num.cmp(&self.refs[a].pic_num));
        lt.sort_by(|&a, &b| self.refs[a].long_term.cmp(&self.refs[b].long_term));
        let mut order = st;
        order.extend(lt);
        // The reorder machinery works on `num_ref_idx_active` slots —
        // when the DPB holds fewer pictures the tail slots repeat the
        // last entry (spec 8.2.4.1's fill rule, as the reference
        // decoder's `default_ref` does), and a command targeting a
        // slot index >= cap is malformed.
        let cap = h.num_ref_idx_l0_active as usize;
        if cap == 0 {
            return Err(Error::BadValue("num_ref_idx_l0_active zero"));
        }
        while order.len() < cap {
            order.push(usize::MAX);
        }
        // RefPicList0 has exactly `num_ref_idx_l0_active` entries
        // (spec 8.2.4.2.1); the reorder shift window (spec 8.2.4.3.1)
        // must not see beyond this cap.
        order.truncate(cap);
        self.apply_reorder(&mut order, &h.reorder_l0, frame_num, cap)?;
        Ok(order)
    }

    /// Reference-picture-list reordering (spec 8.2.4.3.1), shared by
    /// L0 and L1: `pred` starts at the current picture's PicNum and
    /// each `abs_diff_pic_num_minus1` command walks it while the
    /// picked entry is moved to `refIdxLX`.
    fn apply_reorder(
        &mut self,
        order: &mut Vec<usize>,
        cmds: &[(u32, u32)],
        frame_num: u32,
        cap: usize,
    ) -> Result<()> {
        if cmds.is_empty() {
            return Ok(());
        }
        let mut pred = i64::from(frame_num);
        for (index, &(idc, val)) in cmds.iter().enumerate() {
            if index >= cap {
                return Err(Error::BadValue("reference-list modification count"));
            }
            let slot = match idc {
                0 | 1 => {
                    let difference = i64::from(val) + 1;
                    if difference > i64::from(self.max_frame_num) {
                        return Err(Error::BadValue("reference-list difference"));
                    }
                    pred = (if idc == 0 {
                        pred - difference
                    } else {
                        pred + difference
                    })
                    .rem_euclid(i64::from(self.max_frame_num));
                    self.refs
                        .iter()
                        .position(|r| r.long_term.is_none() && r.frame_num == pred as u32)
                }
                2 => self.refs.iter().position(|r| r.long_term == Some(val)),
                _ => return Err(Error::BadValue("reference-list modification operation")),
            }
            .ok_or(Error::MissingReference)?;
            order.insert(index, slot);
            let mut tail = index + 1;
            while tail < order.len() {
                if order[tail] == slot {
                    order.remove(tail);
                } else {
                    tail += 1;
                }
            }
            order.truncate(cap);
        }
        Ok(())
    }
}
