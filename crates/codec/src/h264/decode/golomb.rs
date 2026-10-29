//! MSB-first bit reader with Exp-Golomb (`ue`/`se`/`te`/`me`) decoding.
//!
//! The reader keeps a 64-bit lookahead buffer so VLC decoding can peek a
//! fixed window before committing. Every read bounds-checks: truncation
//! and oversized code words surface as [`Error`], never as a panic or a
//! wrapped shift. H.264's Exp-Golomb `ue(v)` is unbounded in theory, but
//! the syntax caps every use well below 32 bits, so [`Br::ue`] rejects
//! prefixes longer than 32 zeros.

use super::{Error, Result};

/// Bit-level reader over an RBSP byte slice (emulation prevention already
/// removed). Bit order is the H.264 convention: bit 0 is the MSB of
/// byte 0.
#[derive(Debug)]
pub(crate) struct Br<'a> {
    #[cfg(test)]
    pub(crate) data: &'a [u8],
    #[cfg(not(test))]
    data: &'a [u8],
    /// Index of the next bit to read, counting from the MSB of byte 0.
    pos: usize,
}

impl<'a> Br<'a> {
    /// Creates a reader over `data`, positioned at bit 0.
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Br { data, pos: 0 }
    }

    /// Bits still readable.
    pub(crate) fn remaining(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// `true` when positioned on a byte boundary.
    pub(crate) fn is_byte_aligned(&self) -> bool {
        self.pos.is_multiple_of(8)
    }

    /// Discards bits up to the next byte boundary.
    pub(crate) fn byte_align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }

    /// `true` when the only bits left are the RBSP stop bit and its
    /// alignment zeros — i.e. `more_rbsp_data()` is false. The tail may
    /// also be completely empty or all-zero.
    pub(crate) fn no_more_rbsp_data(&self) -> bool {
        let rem = self.remaining();
        rem == 0 || (rem <= 8 && self.peek(rem).is_ok_and(|v| v == 1 << (rem - 1)))
    }

    pub(crate) fn trailing_bits(&mut self) -> Result<()> {
        if self.remaining() > 8 || !self.bit()? {
            return Err(Error::BadValue("rbsp_stop_one_bit"));
        }
        let n = self.remaining();
        if self.bits(n)? != 0 {
            return Err(Error::BadValue("rbsp_alignment_zero_bit"));
        }
        Ok(())
    }

    /// Peeks the next `n` bits (`n <= 32`) without consuming them.
    /// The first stream bit is the MSB of the returned value.
    pub(crate) fn peek(&self, n: usize) -> Result<u32> {
        if n > 32 {
            return Err(Error::BadValue("peek width over 32"));
        }
        if n > self.remaining() {
            return Err(Error::truncated("peek bits", n, self.remaining()));
        }
        if n == 0 {
            return Ok(0);
        }
        let start = self.pos / 8;
        let offset = self.pos % 8;
        if offset + n <= 16 {
            let word = (u32::from(self.data[start]) << 8)
                | u32::from(self.data.get(start + 1).copied().unwrap_or(0));
            return Ok((word >> (16 - offset - n)) & ((1u32 << n) - 1));
        }
        let mut bytes = [0u8; 8];
        let count = (offset + n).div_ceil(8);
        bytes[..count].copy_from_slice(&self.data[start..start + count]);
        Ok(((u64::from_be_bytes(bytes) >> (64 - offset - n)) & ((1u64 << n) - 1)) as u32)
    }

    /// Reads `n` bits (`n <= 32`), first stream bit = result MSB.
    pub(crate) fn bits(&mut self, n: usize) -> Result<u32> {
        let v = self.peek(n)?;
        self.pos += n;
        Ok(v)
    }

    /// Reads one bit.
    pub(crate) fn bit(&mut self) -> Result<bool> {
        if self.remaining() == 0 {
            return Err(Error::BadValue("truncated bit"));
        }
        let bit = self.data[self.pos / 8] & (0x80 >> (self.pos % 8)) != 0;
        self.pos += 1;
        Ok(bit)
    }

    /// Reads one whole byte at an aligned position.
    pub(crate) fn byte(&mut self) -> Result<u8> {
        if !self.is_byte_aligned() {
            return Err(Error::BadValue("unaligned byte read"));
        }
        Ok(self.bits(8)? as u8)
    }

    /// Unsigned Exp-Golomb `ue(v)`. Rejects a leading-zero prefix longer
    /// than 31 (code_num above `u32::MAX`), which no H.264 field permits.
    pub(crate) fn ue(&mut self) -> Result<u32> {
        let n = self.remaining().min(32);
        if n == 0 {
            return Err(Error::BadValue("truncated ue(v)"));
        }
        let window = self.peek(n)? << (32 - n);
        let zeros = window.leading_zeros() as usize;
        if zeros >= n {
            return Err(Error::BadValue("truncated or oversized ue(v)"));
        }
        self.pos += zeros + 1;
        let suffix = if zeros == 0 { 0 } else { self.bits(zeros)? };
        Ok((1u64 << zeros) as u32 - 1 + suffix)
    }

    /// Signed Exp-Golomb `se(v)`.
    pub(crate) fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        // code_num -> value: odd k maps to (k+1)/2, even to -(k/2).
        let v = if k % 2 == 0 { -(k / 2) } else { (k + 1) / 2 };
        Ok(v as i32)
    }

    /// Truncated Exp-Golomb `te(v)`: one bit when `range` is 1, and that
    /// single bit is INVERTED per spec 9.1 (bit `0` decodes as `1`,
    /// bit `1` as `0`); a plain `ue(v)` otherwise.
    pub(crate) fn te(&mut self, range: u32) -> Result<u32> {
        if range <= 1 {
            Ok(1 - self.bits(1)?)
        } else {
            self.ue()
        }
    }
}

#[cfg(test)]
mod br_edge_tests {
    use super::*;

    #[test]
    fn peek_rejects_over_32_bits() {
        let br = Br::new(&[0xffu8; 8]);
        assert!(br.peek(33).is_err());
        assert!(br.peek(32).is_ok());
    }

    #[test]
    fn byte_read_requires_alignment() {
        let mut br = Br::new(&[0xabu8; 4]);
        assert!(br.byte().is_ok());
        // consume one bit -> unaligned
        let _ = br.bit();
        assert!(br.byte().is_err());
    }

    #[test]
    fn ue_prefix_over_31_zeros_rejects() {
        let mut br = Br::new(&[0x00u8; 8]);
        assert!(br.ue().is_err());
    }
}
