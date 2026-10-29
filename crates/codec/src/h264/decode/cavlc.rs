//! CAVLC residual-block decoding (spec 9.2).
//!
//! One [`decode_block`] call produces the coefficient list of a 4x4
//! block (or the 2x2 chroma-DC block, or the 15-coefficient AC list of
//! an Intra-16x16 / chroma block), following spec 9.2.1 – 9.2.4
//! exactly: coeff_token VLC selection by the predicted neighbour count
//! `nC`, level decoding with the adaptive suffix length, `total_zeros`
//! and `run_before` VLCs, then scatter along the zig-zag scan.
//!
//! The VLC tables live in [`super::tables`]; decoding is a linear match
//! on (length, bits) pairs, which is slower than a tree but keeps the
//! table verbatim from the spec, eliminating a transcription layer.
//!
//! Every malformed code surfaces as [`Error`]: a coeff_token or
//! total_zeros index past the table, `total_coeff` above `max_coeff`,
//! a level prefix longer than 28, or a truncated suffix all `Err` out
//! — no path panics or wraps silently.

use super::golomb::Br;
use super::tables::{
    CHROMA_DC_COEFF_TOKEN_BITS, CHROMA_DC_COEFF_TOKEN_LENS, CHROMA_DC_TOTAL_ZEROS_BITS,
    CHROMA_DC_TOTAL_ZEROS_LENS, COEFF_TOKEN_BITS, COEFF_TOKEN_LENS, RUN_BEFORE_BITS,
    RUN_BEFORE_LENS, TOTAL_ZEROS_BITS, TOTAL_ZEROS_LENS,
};
use super::{Error, Result};

/// Maximum `level_prefix` accepted (beyond 28 the level's magnitude
/// exceeds anything a legal stream needs — longer prefixes are stream
/// corruption, not big coefficients).
const MAX_LEVEL_PREFIX: u32 = 28;

/// One decoded residual block: coefficients in scan order inside the
/// block (`levels[s]` is the coefficient at scan position s; unused
/// entries are zero).
#[derive(Clone, Debug, Default)]
pub(crate) struct Residual {
    /// The `total_coeff` count (non-zero coefficient count), also used
    /// as the `nC` contributor for later neighbours.
    pub total_coeff: u8,
    /// Significant coefficients placed at *scan* positions.
    pub levels: [i32; 16],
}

/// Which VLC-table family a block belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlockKind {
    /// 16-coefficient 4x4 (luma DC+AC of an Intra-4x4 block, or a whole
    /// non-16x16 luma block): coeff_token selected by `nC`, total_zeros
    /// from Table 9-7, zig-zag 4x4.
    LumaOrChromaAc4x4,
    /// 15-coefficient 4x4 AC (Intra-16x16 luma AC, chroma AC): same
    /// VLCs, scan starts at position 1 of the zig-zag.
    Ac15,
    /// 2x2 chroma DC: chroma-DC coeff_token table and Table 9-9(a)
    /// total_zeros; `nC` is not used.
    ChromaDc,
}

impl BlockKind {
    fn max_coeff(self) -> usize {
        match self {
            BlockKind::LumaOrChromaAc4x4 => 16,
            BlockKind::Ac15 => 15,
            BlockKind::ChromaDc => 4,
        }
    }
}

const LOOKAHEAD: usize = 10;
const TABLE_SIZE: usize = 1 << LOOKAHEAD;

const fn lookup(lens: &[u8], bits: &[u8]) -> [u16; TABLE_SIZE] {
    let mut table = [0; TABLE_SIZE];
    let mut symbol = 0;
    while symbol < lens.len() {
        let length = lens[symbol] as usize;
        if length > 0 && length <= LOOKAHEAD {
            let start = (bits[symbol] as usize) << (LOOKAHEAD-length);
            let end = start + (1 << (LOOKAHEAD-length));
            let mut i = start;
            while i < end { table[i] = ((symbol as u16) << 5) | length as u16; i += 1; }
        }
        symbol += 1;
    }
    table
}

static COEFF_FAST: [[u16; TABLE_SIZE];3] = [lookup(&COEFF_TOKEN_LENS[0],&COEFF_TOKEN_BITS[0]), lookup(&COEFF_TOKEN_LENS[1],&COEFF_TOKEN_BITS[1]), lookup(&COEFF_TOKEN_LENS[2],&COEFF_TOKEN_BITS[2])];
static CHROMA_FAST: [u16; TABLE_SIZE] = lookup(&CHROMA_DC_COEFF_TOKEN_LENS,&CHROMA_DC_COEFF_TOKEN_BITS);
static ZERO_FAST: [[u16; TABLE_SIZE];15] = {
    let mut tables = [[0;TABLE_SIZE];15]; let mut i=0;
    while i<15 { tables[i]=lookup(&TOTAL_ZEROS_LENS[i], &TOTAL_ZEROS_BITS[i]); i+=1; } tables
};
static CHROMA_ZERO_FAST: [[u16; TABLE_SIZE];3] = {
    let mut tables = [[0;TABLE_SIZE];3]; let mut i=0;
    while i<3 { tables[i]=lookup(&CHROMA_DC_TOTAL_ZEROS_LENS[i], &CHROMA_DC_TOTAL_ZEROS_BITS[i]); i+=1; } tables
};
static RUN_FAST: [[u16; TABLE_SIZE];7] = {
    let mut tables = [[0;TABLE_SIZE];7]; let mut i=0;
    while i<7 { tables[i]=lookup(&RUN_BEFORE_LENS[i], &RUN_BEFORE_BITS[i]); i+=1; } tables
};

/// A short-prefix lookup handles common codes; the original tables also
/// cover long escape codes and truncated input without speculative reads.
fn vlc(br: &mut Br<'_>, lens: &[u8], bits: &[u8], fast: &[u16; TABLE_SIZE]) -> Result<usize> {
    let n = LOOKAHEAD.min(br.remaining());
    if n == 0 { return Err(Error::BadValue("truncated VLC")); }
    let entry = fast[(br.peek(n)? as usize) << (LOOKAHEAD-n)];
    let length = (entry & 31) as usize;
    if length > 0 && length <= n { br.bits(length)?; return Ok((entry >> 5) as usize); }
    let n = lens.iter().copied().max().unwrap_or(0) as usize;
    let n = n.min(br.remaining());
    let window = br.peek(n)?;
    for (symbol, (&length,&code)) in lens.iter().zip(bits).enumerate() {
        let length = length as usize;
        if length > 0 && length <= n && window >> (n-length) == u32::from(code) { br.bits(length)?; return Ok(symbol); }
    }
    Err(Error::BadValue("unrecognised CAVLC prefix"))
}

/// coeff_token VLC for a given `nC` (Table 9-5); `nC < 0` selects the
/// chroma-DC table. Returns `(total_coeff, trailing_ones)`.
fn coeff_token(br: &mut Br<'_>, nc: i32) -> Result<(u8, u8)> {
    if nc < 0 {
        let idx = vlc(br, &CHROMA_DC_COEFF_TOKEN_LENS, &CHROMA_DC_COEFF_TOKEN_BITS, &CHROMA_FAST)?;
        return Ok(((idx / 4) as u8, (idx % 4) as u8));
    }
    if nc >= 8 {
        // Fixed-length code (spec 9.2.1, Table 9-5 last column): a
        // 6-bit field where 0 -> (TotalCoeff 1, T1 0), 1 -> (1,1),
        // 3 -> (0,0), and code >= 4 maps as `code + 4` onto the
        // `TotalCoeff * 4 + TrailingOnes` index. Code 2 is reserved and
        // never emitted by a conforming encoder.
        let code = br.bits(6)?;
        return match code {
            0 => Ok((1, 0)),
            1 => Ok((1, 1)),
            3 => Ok((0, 0)),
            2 => Err(Error::BadValue("h264 CAVLC: reserved coeff_token FLC")),
            _ => Ok((((code + 4) / 4) as u8, ((code + 4) % 4) as u8)),
        };
    }
    let table = match nc {
        0 | 1 => 0,
        2 | 3 => 1,
        _ => 2,
    };
    let idx = vlc(br, &COEFF_TOKEN_LENS[table], &COEFF_TOKEN_BITS[table], &COEFF_FAST[table])?;
    Ok(((idx / 4) as u8, (idx % 4) as u8))
}

/// Leading-zero count consumed as `level_prefix`; prefixes longer than
/// [`MAX_LEVEL_PREFIX`] are stream errors.
fn level_prefix(br: &mut Br<'_>) -> Result<u32> {
    let mut n = 0u32;
    loop {
        if br.remaining() == 0 {
            return Err(Error::truncated("level_prefix", (n + 1) as usize, 0));
        }
        if br.bits(1)? == 1 {
            return Ok(n);
        }
        n += 1;
        if n > MAX_LEVEL_PREFIX {
            return Err(Error::BadValue("h264 CAVLC: level_prefix over 28"));
        }
    }
}

/// Decodes one residual block per spec 9.2. `nc` is the context value
/// (spec 9.2.1); it is ignored for [`BlockKind::ChromaDc`].
pub(crate) fn decode_block(br: &mut Br<'_>, nc: i32, kind: BlockKind) -> Result<Residual> {
    let max_coeff = kind.max_coeff();
    let (total_coeff, trailing_ones) = coeff_token(br, nc)?;
    if total_coeff as usize > max_coeff {
        return Err(Error::BadValue(
            "h264 CAVLC: total_coeff exceeds block size",
        ));
    }
    let mut out = Residual {
        total_coeff,
        ..Residual::default()
    };
    if total_coeff == 0 {
        return Ok(out);
    }
    let total = total_coeff as usize;
    let t1 = trailing_ones as usize;
    if t1 > total {
        return Err(Error::BadValue(
            "h264 CAVLC: trailing_ones exceeds total_coeff",
        ));
    }

    let mut level = [0i32; 16];
    // Trailing-one signs, first bit = sign of the last coefficient.
    for l in level.iter_mut().take(t1) {
        *l = if br.bit()? { -1 } else { 1 };
    }

    if t1 < total {
        // Spec 9.2.2: for each remaining level — prefix, then a suffix
        // of levelSuffixSize bits, the levelCode → signed-level map,
        // then the suffixLength update rule. The first non-trailing
        // level (i == trailing_ones) bumps levelCode by 2 when fewer
        // than 3 trailing ones were seen.
        let mut suffix_length: u32 = u32::from(total > 10 && t1 < 3);
        for (i, l) in level.iter_mut().enumerate().take(total).skip(t1) {
            let prefix = level_prefix(br)? as i64;
            let sl = suffix_length;
            let suffix_size: u32 = match prefix.cmp(&14) {
                core::cmp::Ordering::Less => sl,
                core::cmp::Ordering::Equal => {
                    if sl == 0 {
                        4
                    } else {
                        sl
                    }
                }
                // prefix 15 -> 12, prefix >= 16 -> prefix - 3 (the
                // "escape" case); unified as prefix - 3.
                core::cmp::Ordering::Greater => (prefix - 3) as u32,
            };
            let mut level_code: i64 = prefix.min(15) << sl;
            if suffix_size > 0 {
                level_code += i64::from(br.bits(suffix_size as usize)?);
            }
            if prefix >= 15 && sl == 0 {
                level_code += 15;
            }
            if prefix >= 16 {
                level_code += (1i64 << (prefix - 3)) - 4096;
            }
            if i == t1 && t1 < 3 {
                level_code += 2;
            }
            let lvl = if level_code % 2 == 0 {
                (level_code + 2) / 2
            } else {
                (-level_code - 1) / 2
            };
            *l = lvl as i32;
            if suffix_length == 0 {
                suffix_length = 1;
            }
            // suffixLimit(suffixLength) = 3 << (suffixLength - 1).
            if l.abs() as i64 > (3i64 << (suffix_length - 1)) && suffix_length < 6 {
                suffix_length += 1;
            }
        }
    }

    // total_zeros / run_before scatter (spec 9.2.3 - 9.2.4).
    let zeros_left = if total == max_coeff {
        0usize
    } else {
        let (lens, bits) = match kind {
            BlockKind::ChromaDc => (
                &CHROMA_DC_TOTAL_ZEROS_LENS[total - 1][..],
                &CHROMA_DC_TOTAL_ZEROS_BITS[total - 1][..],
            ),
            _ => (
                &TOTAL_ZEROS_LENS[total - 1][..],
                &TOTAL_ZEROS_BITS[total - 1][..],
            ),
        };
        let fast = if kind == BlockKind::ChromaDc { &CHROMA_ZERO_FAST[total-1] } else { &ZERO_FAST[total-1] };
        let idx = vlc(br, lens, bits, fast)?;
        if idx > max_coeff - total {
            return Err(Error::BadValue("h264 CAVLC: total_zeros out of range"));
        }
        idx
    };

    // Place levels back-to-front over the scan positions.
    let mut scan_pos = zeros_left + total - 1;
    out.levels[scan_pos] = level[0];
    let mut zeros = zeros_left;
    for &lv in level.iter().take(total).skip(1) {
        let run_before = if zeros > 0 {
            let row = (zeros - 1).min(6);

            vlc(br, &RUN_BEFORE_LENS[row], &RUN_BEFORE_BITS[row], &RUN_FAST[row])?
        } else {
            0
        };
        if run_before > zeros {
            return Err(Error::BadValue("h264 CAVLC: run_before exceeds zeros_left"));
        }
        zeros -= run_before;
        if scan_pos < 1 + run_before {
            return Err(Error::BadValue(
                "h264 CAVLC: coefficient position underflow",
            ));
        }
        scan_pos -= 1 + run_before;
        out.levels[scan_pos] = lv;
    }
    Ok(out)
}

/// Maps a scan position inside a residual block to the raster index
/// used by the rest of the decoder.
///
/// * `LumaOrChromaAc4x4`: 4x4 zig-zag positions 0..15 → raster 0..15.
/// * `Ac15`: scan positions 0..14 map to zig-zag positions 1..15
///   (the DC slot is index 0 and coded elsewhere).
/// * `ChromaDc`: 2x2 raster order (the spec's 2x2 scan).
pub(crate) fn raster_index(kind: BlockKind, scan_pos: usize) -> usize {
    match kind {
        BlockKind::LumaOrChromaAc4x4 => super::tables::ZIGZAG_4X4[scan_pos] as usize,
        BlockKind::Ac15 => super::tables::ZIGZAG_4X4[scan_pos + 1] as usize,
        BlockKind::ChromaDc => super::tables::SCAN_2X2[scan_pos] as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `level_prefix` refuses prefixes beyond [`MAX_LEVEL_PREFIX`]
    /// instead of decoding an astronomically large level: a long run
    /// of zero bits with no terminating 1 is stream corruption.
    #[test]
    fn level_prefix_over_28_is_refused() {
        // 64 zero bits: the terminating 1 never arrives, so the
        // prefix counter crosses the cap while bits remain.
        let mut br = Br::new(&[0u8; 8]);
        assert!(matches!(
            level_prefix(&mut br),
            Err(Error::BadValue("h264 CAVLC: level_prefix over 28"))
        ));
    }
}
