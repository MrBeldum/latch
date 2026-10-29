//! Macroblock model: types, per-MB decode state, and the neighbour
//! geometry every context-derivation step shares.
//!
//! Two coordinate conventions coexist in the spec and both appear here:
//!
//! * **4x4 raster index** `r = x4 + 4*y4` — the order intra-4x4 mode
//!   parsing and reconstruction use;
//! * **8x8-group-major index** `b` (see [`super::tables::block_index`])
//!   — the order `coded_block_pattern`, CAVLC `nC` derivation, MV
//!   storage and deblocking boundary strengths use. All per-4x4 arrays
//!   in [`MbState`] are group-major.

use super::{Error, Result};

/// Parsed `mb_type` semantics, covering every P-, B- and I-slice code
/// (spec Tables 7-11, 7-13, 7-14, 7-15, 7-17, 7-18).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum MbType {
    /// I_NxN: sixteen 4x4 intra predictions.
    I4x4,
    /// I_16x16 with `intra16x16_pred_mode` (0..3), chroma cbp (0..2) and
    /// luma cbp (0 or 15).
    I16x16 {
        /// `Intra16x16PredMode` 0..=3 — V, H, DC, Plane (Table 8-3 order).
        pred: u8,
        /// `CodedBlockPatternChroma` 0..=2.
        cbp_chroma: u8,
        /// `CodedBlockPatternLuma` 0 or 15.
        cbp_luma: u8,
    },
    /// I_PCM: raw 8-bit samples, no transform.
    IPcm,
    /// P_L0_16x16.
    P16x16,
    /// P_L0_L0_16x8.
    P16x8,
    /// P_L0_L0_8x16.
    P8x16,
    /// P_8x8 (any sub-mb types).
    P8x8,
    /// P_8x8ref0 (sub-mb ref index list implicitly all zero).
    P8x8Ref0,
    /// P_Skip (never parsed as an mb_type; produced by the skip run).
    PSkip,
}

impl MbType {
    /// `true` for any intra mode (4x4, 16x16, PCM).
    pub(crate) fn is_intra(self) -> bool {
        matches!(self, MbType::I4x4 | MbType::I16x16 { .. } | MbType::IPcm)
    }

    /// Decodes an I-slice `mb_type` code number (spec Table 7-11).
    pub(crate) fn i_slice(code: u32) -> Result<MbType> {
        if code == 0 {
            return Ok(MbType::I4x4);
        }
        if code == 25 {
            return Ok(MbType::IPcm);
        }
        if !(1..=24).contains(&code) {
            return Err(Error::BadValue("I-slice mb_type over 25"));
        }
        let v = code - 1;
        Ok(MbType::I16x16 {
            pred: (v % 4) as u8,
            cbp_chroma: ((v % 12) / 4) as u8,
            cbp_luma: if v < 12 { 0 } else { 15 },
        })
    }

    /// Decodes a P-slice `mb_type` code number (spec Table 7-14):
    /// 0..4 are inter, 5..30 are the I-slice table shifted by 5.
    pub(crate) fn p_slice(code: u32) -> Result<MbType> {
        match code {
            0 => Ok(MbType::P16x16),
            1 => Ok(MbType::P16x8),
            2 => Ok(MbType::P8x16),
            3 => Ok(MbType::P8x8),
            4 => Ok(MbType::P8x8Ref0),
            5..=30 => MbType::i_slice(code - 5),
            _ => Err(Error::BadValue("P-slice mb_type over 30")),
        }
    }
}

/// Sub-macroblock types: P_8x8's four codes (spec Table 7-17) and
/// B_8x8's thirteen (Table 7-18). For B types `dirs` is bit0 = L0,
/// bit1 = L1; `BDirect` is `B_Direct_8x8`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubMbType {
    /// P_L0_8x8.
    S8x8,
    /// P_L0_8x4.
    S8x4,
    /// P_L0_4x8.
    S4x8,
    /// P_L0_4x4.
    S4x4,
}

impl SubMbType {
    /// Decodes the P-slice `sub_mb_type` code (0..=3, spec Table 7-17).
    pub(crate) fn from_code(code: u32) -> Result<SubMbType> {
        match code {
            0 => Ok(SubMbType::S8x8),
            1 => Ok(SubMbType::S8x4),
            2 => Ok(SubMbType::S4x8),
            3 => Ok(SubMbType::S4x4),
            _ => Err(Error::BadValue("sub_mb_type over 3")),
        }
    }

    /// `(w4, h4)` of each sub-partition in 4x4 units, and the count.
    /// For `BDirect` this returns the single whole-8x8 partition; the
    /// decoder may split it into 4x4 under `direct_8x8_inference`.
    pub(crate) fn parts(self) -> ([(u8, u8); 4], usize) {
        match self {
            SubMbType::S8x8 => ([(2, 2), (0, 0), (0, 0), (0, 0)], 1),
            SubMbType::S8x4 => ([(2, 1), (2, 1), (0, 0), (0, 0)], 2),
            SubMbType::S4x8 => ([(1, 2), (1, 2), (0, 0), (0, 0)], 2),
            SubMbType::S4x4 => ([(1, 1); 4], 4),
        }
    }
}

/// Per-macroblock decoded state, shared by prediction, CAVLC `nC` and
/// deblocking. All per-4x4 arrays are 8x8-group-major
/// ([`super::tables::block_index`]).
#[derive(Clone, Debug)]
pub(crate) struct MbState {
    /// Macroblock type.
    pub mb_type: MbType,
    /// Slice this macroblock belongs to (for cross-slice checks).
    pub slice_id: u32,
    /// Debug: raster index of this MB (set by the decoder loop).
    pub dbg_idx: u32,
    /// `QPy` for this macroblock.
    pub qp_y: u8,
    /// Deblocking idc of the slice that coded this MB (needed when
    /// neighbouring MBs live in different slices).
    pub disable_deblock_idc: u8,
    /// `alpha`/`beta` deblock offsets of the coding slice.
    pub filter_offset_a: i8,
    /// `beta` offset.
    pub filter_offset_b: i8,
    /// `total_coeff` per 4x4 luma block (group-major, 24 entries cover
    /// 16 luma + 8 chroma AC, in the same group-major order for the
    /// chroma 8x8 pair) — used by `nC` prediction and deblocking.
    /// Under `transform_size_8x8` each 8x8 group's four entries all
    /// carry the 8x8 `TotalCoeff` (spec 8.7.2.1's per-8x8 `nz` view).
    pub nz: [u8; 24],
    /// Luma motion vectors per 4x4 block for list L0 (quarter-pel).
    pub mv: [[i16; 2]; 16],
    /// `ref_idx_l0` per 4x4 block (0xff = not used on this list).
    pub ref_idx: [u8; 16],
    pub deblock_refs: [u8; 16],
    /// True once this block's L0 MV has been decoded/committed. `ref_idx`
    /// may be committed early (the whole sub-mb's refs decode before any
    /// mvd), so availability for MV prediction needs a separate gate —
    /// an early ref with a stale zero MV must read as *unavailable*.
    pub mv_valid: [bool; 16],
    /// Parsed intra-4x4 modes (raster order), 0..8 or 0xff unset.
    pub i4x4_modes: [u8; 16],
    /// `intra_chroma_pred_mode` 0..=3.
    pub chroma_pred: u8,
    /// `mb_skip_flag` (P_Skip or B_Skip; `BDirect` does NOT count as
    /// skipped for the CABAC `mb_skip_flag` context derivation).
    pub skip: bool,
    /// `transform_size_8x8_flag` of this macroblock.
    pub transform8x8: bool,
    /// `CodedBlockPatternLuma` (4 bits) + `CodedBlockPatternChroma`
    /// (2 bits) as parsed: `cbp = chroma<<4 | luma` — the CABAC
    /// `coded_block_pattern` context and ffmpeg's `cbp_table` layout.
    pub cbp: u8,
}

impl MbState {
    /// Fresh state for an undecoded macroblock.
    pub(crate) fn new() -> MbState {
        MbState {
            mb_type: MbType::PSkip,
            slice_id: u32::MAX,
            dbg_idx: u32::MAX,
            qp_y: 26,
            disable_deblock_idc: 0,
            filter_offset_a: 0,
            filter_offset_b: 0,
            nz: [0; 24],
            mv: [[0; 2]; 16],
            ref_idx: [0xff; 16],
            deblock_refs: [0xff; 16],
            mv_valid: [false; 16],
            i4x4_modes: [0xff; 16],
            chroma_pred: 0,
            skip: false,
            transform8x8: false,
            cbp: 0,
        }
    }
}

/// Position of the MB, 4x4-grid coordinates of its top-left sample and
/// neighbour MB indices within a `mb_width` × `mb_height` map.
#[derive(Copy, Clone, Debug)]
pub(crate) struct MbMap {
    /// MB raster index.
    pub idx: usize,
    /// `mb_x`, `mb_y` in macroblocks.
    pub x: usize,
    /// Row.
    pub y: usize,
    /// Picture width in MBs.
    pub width: usize,
    /// Slice sequence number of the slice currently being decoded.
    /// Foreign MBs count as neighbours only when their `slice_id`
    /// matches — spec 7.4.3 / 9.2.1: blocks in a different slice are
    /// not available as prediction context.
    pub sid: u32,
}

impl MbMap {
    /// Index of the left neighbour MB (mbAddrA), if any.
    pub(crate) fn mb_a(self) -> Option<usize> {
        if self.x > 0 {
            Some(self.idx - 1)
        } else {
            None
        }
    }
    /// Index of the top neighbour MB (mbAddrB), if any.
    pub(crate) fn mb_b(self) -> Option<usize> {
        if self.y > 0 {
            Some(self.idx - self.width)
        } else {
            None
        }
    }
    /// Index of the top-right neighbour MB (mbAddrC), if any.
    pub(crate) fn mb_c(self) -> Option<usize> {
        if self.y > 0 && self.x + 1 < self.width {
            Some(self.idx - self.width + 1)
        } else {
            None
        }
    }
    /// Index of the top-left neighbour MB (mbAddrD), if any.
    pub(crate) fn mb_d(self) -> Option<usize> {
        if self.y > 0 && self.x > 0 {
            Some(self.idx - self.width - 1)
        } else {
            None
        }
    }
}

/// Neighbours of the *raster*-index 4x4 block `r` used by intra-4x4
/// prediction (spec 6.4.11.4). Returns
/// `(a, b, c, d)` where each is `(mb, x4, y4)` — `mb` is one of
/// `NeighbourMb`. `None` means unavailable by construction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Nb {
    /// Same macroblock.
    Curr,
    /// mbAddrA (left).
    A,
    /// mbAddrB (above).
    B,
    /// mbAddrC (above-right).
    C,
    /// mbAddrD (above-left).
    D,
    /// Unavailable (out of picture or not yet coded).
    None,
}

/// `(mb, x4, y4)` triple naming a 4x4 block for prediction contexts.
pub(crate) type BlockRef = (Nb, usize, usize);

/// Neighbour 4x4 blocks of raster block `r` inside the MB
/// (spec 6.4.11.4 + 8.3.1.1 availability by construction):
/// `(A, B, C, D)` as luma-block coordinates.
pub(crate) fn intra4x4_neighbours(blk: usize) -> [BlockRef; 4] {
    // `blk` is the group-major (luma4x4BlkIdx) index. Neighbourhood is
    // derived on the 4x4 raster grid, then mapped back to group-major
    // indices so same-MB members carry `Curr` + group-major id.
    let (x, y) = super::tables::block_xy(blk);
    let (xi, yi) = (x as i32, y as i32);
    let map = |nx: i32, ny: i32| -> BlockRef {
        if (0..4).contains(&nx) && (0..4).contains(&ny) {
            (Nb::Curr, nx as usize, ny as usize)
        } else {
            // Foreign block: which neighbour MB by the crossed edges.
            let who = match (nx < 0, ny < 0, nx >= 4) {
                (true, false, false) => Nb::A,
                (false, true, false) => Nb::B,
                (false, true, true) => Nb::C,
                (true, true, false) => Nb::D,
                (true, false, true) => Nb::A,
                _ => Nb::None,
            };
            if who == Nb::A && nx < 0 && ny < 0 {
                (Nb::D, 3, 3)
            } else {
                let lx = nx.rem_euclid(4);
                let ly = ny.rem_euclid(4);
                (who, lx as usize, ly as usize)
            }
        }
    };
    let a = map(xi - 1, yi);
    let b = map(xi, yi - 1);
    let c = map(xi + 1, yi - 1);
    let d = map(xi - 1, yi - 1);
    [a, b, c, d]
}
