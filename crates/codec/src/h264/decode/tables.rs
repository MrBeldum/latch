//! Constant tables from ITU-T Rec. H.264 (CAVLC VLC tables, coded block
//! pattern maps, dequantisation scales, deblocking thresholds, scans).
//!
//! Every table is transcribed from the normative tables of
//! ITU-T H.264 (V3, 2005-03). Table provenance is noted per item.

/// Zig-zag scan for 4x4 blocks (spec Figure 8-8(a)): coefficient scan
/// position -> raster index `x + 4*y` inside the 4x4 block.
pub(crate) const ZIGZAG_4X4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// Chroma DC 2x2 scan (spec 8.5.7): scan index -> `x + 2*y`.
pub(crate) const SCAN_2X2: [u8; 4] = [0, 1, 2, 3];

pub(crate) fn block_xy(block: usize) -> (usize, usize) {
    let g = block / 4;
    let s = block % 4;
    ((g % 2) * 2 + (s % 2), (g / 2) * 2 + (s / 2))
}

/// Inverse of [`block_xy`]: 4x4-grid coordinates -> group-major index.
pub(crate) fn block_index(x4: usize, y4: usize) -> usize {
    (y4 / 2 * 2 + x4 / 2) * 4 + (y4 % 2) * 2 + (x4 % 2)
}

// coded_block_pattern (spec Table 9-4)
// ------------------------------------------------------------------

/// me(v) mapping for `coded_block_pattern` when the macroblock prediction
/// mode is Intra_4x4 (spec Table 9-4 left column). Index is the ue code
/// number; value packs `chroma << 4 | luma` bits where luma bit i marks
/// the coded 8x8 group.
pub(crate) const CBP_INTRA: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

/// Same table for Intra_16x16 and all Inter macroblock modes
/// (spec Table 9-4 right column).
pub(crate) const CBP_INTER: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34,
    36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

// ------------------------------------------------------------------
// CAVLC coefficient-token VLC (spec Table 9-5)
// ------------------------------------------------------------------

/// `coeff_token` VLCs. `COEFF_TOKEN_LENS[v][TotalCoeff*4 + TrailingOnes]`
/// is the code length and `COEFF_TOKEN_BITS` the code bits, for the four
/// nC ranges `v=0` (0<=nC<2), `v=1` (2<=nC<4), `v=2` (4<=nC<8), `v=3`
/// (FLC, nC>=8).
pub(crate) const COEFF_TOKEN_LENS: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, //
        6, 2, 0, 0, 8, 6, 3, 0, 9, 8, 7, 5, 10, 9, 8, 6, //
        11, 10, 9, 7, 13, 11, 10, 8, 13, 13, 11, 9, 13, 13, 13, 10, //
        14, 14, 13, 11, 14, 14, 14, 13, 15, 15, 14, 14, 15, 15, 15, 14, //
        16, 15, 15, 15, 16, 16, 16, 15, 16, 16, 16, 16, 16, 16, 16, 16,
    ],
    [
        2, 0, 0, 0, //
        6, 2, 0, 0, 6, 5, 3, 0, 7, 6, 6, 4, 8, 6, 6, 4, //
        8, 7, 7, 5, 9, 8, 8, 6, 11, 9, 9, 6, 11, 11, 11, 7, //
        12, 11, 11, 9, 12, 12, 12, 11, 12, 12, 12, 11, 13, 13, 13, 12, //
        13, 13, 13, 13, 13, 14, 13, 13, 14, 14, 14, 13, 14, 14, 14, 14,
    ],
    [
        4, 0, 0, 0, //
        6, 4, 0, 0, 6, 5, 4, 0, 6, 5, 5, 4, 7, 5, 5, 4, //
        7, 5, 5, 4, 7, 6, 6, 4, 7, 6, 6, 4, 8, 7, 7, 5, //
        8, 8, 7, 6, 9, 8, 8, 7, 9, 9, 8, 8, 9, 9, 9, 8, //
        10, 9, 9, 9, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
    ],
    [
        6, 0, 0, 0, //
        6, 6, 0, 0, 6, 6, 6, 0, 6, 6, 6, 6, 6, 6, 6, 6, //
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, //
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, //
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    ],
];

/// Code bits paired with [`COEFF_TOKEN_LENS`].
pub(crate) const COEFF_TOKEN_BITS: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, //
        5, 1, 0, 0, 7, 4, 1, 0, 7, 6, 5, 3, 7, 6, 5, 3, //
        7, 6, 5, 4, 15, 6, 5, 4, 11, 14, 5, 4, 8, 10, 13, 4, //
        15, 14, 9, 4, 11, 10, 13, 12, 15, 14, 9, 12, 11, 10, 13, 8, //
        15, 1, 9, 12, 11, 14, 13, 8, 7, 10, 9, 12, 4, 6, 5, 8,
    ],
    [
        3, 0, 0, 0, //
        11, 2, 0, 0, 7, 7, 3, 0, 7, 10, 9, 5, 7, 6, 5, 4, //
        4, 6, 5, 6, 7, 6, 5, 8, 15, 6, 5, 4, 11, 14, 13, 4, //
        15, 10, 9, 4, 11, 14, 13, 12, 8, 10, 9, 8, 15, 14, 13, 12, //
        11, 10, 9, 12, 7, 11, 6, 8, 9, 8, 10, 1, 7, 6, 5, 4,
    ],
    [
        15, 0, 0, 0, //
        15, 14, 0, 0, 11, 15, 13, 0, 8, 12, 14, 12, 15, 10, 11, 11, //
        11, 8, 9, 10, 9, 14, 13, 9, 8, 10, 9, 8, 15, 14, 13, 13, //
        11, 14, 10, 12, 15, 10, 13, 12, 11, 14, 9, 12, 8, 10, 13, 8, //
        13, 7, 9, 12, 9, 12, 11, 10, 5, 8, 7, 6, 1, 4, 3, 2,
    ],
    [
        3, 0, 0, 0, //
        0, 1, 0, 0, 4, 5, 6, 0, 8, 9, 10, 11, 12, 13, 14, 15, //
        16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, //
        32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, //
        48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63,
    ],
];

/// `coeff_token` VLC for chroma DC 2x2 blocks (spec Table 9-5, last
/// column "chroma DC"). Index `TotalCoeff*4 + TrailingOnes`, 5 rows.
pub(crate) const CHROMA_DC_COEFF_TOKEN_LENS: [u8; 20] = [
    2, 0, 0, 0, //
    6, 1, 0, 0, //
    6, 6, 3, 0, //
    6, 7, 7, 6, //
    6, 8, 8, 7,
];

/// Code bits paired with [`CHROMA_DC_COEFF_TOKEN_LENS`].
pub(crate) const CHROMA_DC_COEFF_TOKEN_BITS: [u8; 20] = [
    1, 0, 0, 0, //
    7, 1, 0, 0, //
    4, 6, 1, 0, //
    3, 3, 2, 5, //
    2, 3, 2, 0,
];

// ------------------------------------------------------------------
// total_zeros VLC (spec Tables 9-7 and 9-8)
// ------------------------------------------------------------------

/// `total_zeros` VLC for 4x4 blocks, indexed `[total_coeff - 1]` then by
/// decoded value 0..(15-total_coeff). Row `total_coeff-1` has
/// `16 - total_coeff` valid entries (the remaining entries, if any, are
/// padding zeros with length 0 and are never selected).
pub(crate) const TOTAL_ZEROS_LENS: [[u8; 16]; 15] = [
    [1, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 9],
    [3, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 6, 6, 6, 6, 0],
    [4, 3, 3, 3, 4, 4, 3, 3, 4, 5, 5, 6, 5, 6, 0, 0],
    [5, 3, 4, 4, 3, 3, 3, 4, 3, 4, 5, 5, 5, 0, 0, 0],
    [4, 4, 4, 3, 3, 3, 3, 3, 4, 5, 4, 5, 0, 0, 0, 0],
    [6, 5, 3, 3, 3, 3, 3, 3, 4, 3, 6, 0, 0, 0, 0, 0],
    [6, 5, 3, 3, 3, 2, 3, 4, 3, 6, 0, 0, 0, 0, 0, 0],
    [6, 4, 5, 3, 2, 2, 3, 3, 6, 0, 0, 0, 0, 0, 0, 0],
    [6, 6, 4, 2, 2, 3, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0],
    [5, 5, 3, 2, 2, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [4, 4, 3, 3, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [4, 4, 2, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];

/// Code bits paired with [`TOTAL_ZEROS_LENS`].
pub(crate) const TOTAL_ZEROS_BITS: [[u8; 16]; 15] = [
    [1, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 1],
    [7, 6, 5, 4, 3, 5, 4, 3, 2, 3, 2, 3, 2, 1, 0, 0],
    [5, 7, 6, 5, 4, 3, 4, 3, 2, 3, 2, 1, 1, 0, 0, 0],
    [3, 7, 5, 4, 6, 5, 4, 3, 3, 2, 2, 1, 0, 0, 0, 0],
    [5, 4, 3, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0],
    [1, 1, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0],
    [1, 1, 5, 4, 3, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 1, 3, 3, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 3, 2, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 2, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];

/// `total_zeros` VLC for 2x2 chroma DC blocks (spec Table 9-9(a)),
/// indexed `[total_coeff - 1][zeros]` for total_coeff 1..=3
/// (total_coeff == 4 leaves no zeros to code).
pub(crate) const CHROMA_DC_TOTAL_ZEROS_LENS: [[u8; 4]; 3] =
    [[1, 2, 3, 3], [1, 2, 2, 0], [1, 1, 0, 0]];

/// Code bits paired with [`CHROMA_DC_TOTAL_ZEROS_LENS`].
pub(crate) const CHROMA_DC_TOTAL_ZEROS_BITS: [[u8; 4]; 3] =
    [[1, 1, 1, 0], [1, 1, 0, 0], [1, 0, 0, 0]];

// ------------------------------------------------------------------
// run_before VLC (spec Tables 9-10)
// ------------------------------------------------------------------

/// `run_before` VLC rows indexed by `zeros_left - 1` for
/// zeros_left 1..=6; `zeros_left >= 7` (and 7..14) all share
/// [`RUN_BEFORE_LENS_7`]. Row length is `zeros_left + 1` entries for
/// rows < 6 and 15 entries for the last row.
pub(crate) const RUN_BEFORE_LENS: [[u8; 16]; 7] = [
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 3, 3, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 3, 3, 3, 3, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0],
];

/// Code bits paired with [`RUN_BEFORE_LENS`].
pub(crate) const RUN_BEFORE_BITS: [[u8; 16]; 7] = [
    [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 0, 1, 3, 2, 5, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [7, 6, 5, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0],
];

// ------------------------------------------------------------------
// Inverse quantisation (spec 8.5.12.1, Table 8-13 LevelScale4x4)
// ------------------------------------------------------------------

/// `LevelScale4x4(qP % 6, c)` (spec Table 8-12 / eq. 8-335): `c` 0 is
/// the even/even positions `{(0,0),(0,2),(2,0),(2,2)}`, `c` 1 the
/// mixed-parity positions, `c` 2 the odd/odd positions.
pub(crate) const LEVEL_SCALE_4X4: [[u16; 3]; 6] = [
    [10, 13, 16],
    [11, 14, 18],
    [13, 16, 20],
    [14, 18, 23],
    [16, 20, 25],
    [18, 23, 29],
];

/// Coefficient class index for a raster position `(x, y)` inside a 4x4
/// block: 0 when both coordinates are even, 2 when both are odd, 1
/// otherwise.
pub(crate) fn scale_class(x: usize, y: usize) -> usize {
    match (x % 2, y % 2) {
        (0, 0) => 0,
        (1, 1) => 2,
        _ => 1,
    }
}

/// `qP % 6` lookup (`qP` is in `0..52`).
pub(crate) const QP_MOD6: [u8; 52] = {
    let mut t = [0u8; 52];
    let mut i = 0;
    while i < 52 {
        t[i] = (i % 6) as u8;
        i += 1;
    }
    t
};

/// `qP / 6` lookup (`qP` is in `0..52`).
pub(crate) const QP_DIV6: [u8; 52] = {
    let mut t = [0u8; 52];
    let mut i = 0;
    while i < 52 {
        t[i] = (i / 6) as u8;
        i += 1;
    }
    t
};

/// Chroma quantisation parameter mapping (spec Table 8-15): `qPc` from
/// `clip(0, 51, qPy + chroma_qp_index_offset)`.
pub(crate) const QPC_TABLE: [u8; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39,
    39, 39,
];

// ------------------------------------------------------------------
// Deblocking filter tables (spec Tables 8-16, 8-17)
// ------------------------------------------------------------------

/// `alpha` indexed by `indexA = clip(0, 51, qP + filter_offset_a)`
/// (spec Table 8-16).
pub(crate) const ALPHA_TABLE: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20, 22, 25, 28, //
    32, 36, 40, 45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182, //
    203, 226, 255, 255,
];

/// `beta` indexed by `indexB = clip(0, 51, qP + filter_offset_b)`
/// (spec Table 8-16).
pub(crate) const BETA_TABLE: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8, //
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, //
    17, 17, 18, 18,
];

/// `tc0` indexed by `indexA` and `bS - 1` (spec Table 8-17); bS is 1..=3
/// when this table applies (bS == 4 uses the strong intra filter).
pub(crate) const TC0_TABLE: [[u8; 3]; 52] = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 1],
    [0, 0, 1],
    [0, 0, 1],
    [0, 0, 1],
    [0, 1, 1],
    [0, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 2],
    [1, 1, 2],
    [1, 1, 2],
    [1, 1, 2],
    [1, 2, 3],
    [1, 2, 3],
    [2, 2, 3],
    [2, 2, 4],
    [2, 3, 4],
    [2, 3, 4],
    [3, 3, 5],
    [3, 4, 6],
    [3, 4, 6],
    [4, 5, 7],
    [4, 5, 8],
    [4, 6, 9],
    [5, 7, 10],
    [6, 8, 11],
    [6, 8, 13],
    [7, 10, 14],
    [8, 11, 16],
    [9, 12, 18],
    [10, 13, 20],
    [11, 15, 23],
    [13, 17, 25],
];
