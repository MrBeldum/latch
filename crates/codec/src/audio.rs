//! `lpcm-rice`: a lossless code for short frames of 16-bit stereo PCM.
//!
//! Each frame is coded on its own (a lost packet loses nothing else). The
//! format is defined in this module's documentation by the implementer and
//! is part of the protocol: `decode(encode(x)) == x` for every input, and
//! `decode` returns an error (never panics, never allocates more than
//! [`MAX_SAMPLES`] samples) on any other bytes.
//!
//! # Protocol Specification: `lpcm-rice` Wire Format
//!
//! An encoded frame begins with a 3-byte frame header:
//!
//! ```text
//!  0                   1                   2
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |Version|Res|SM |       Sample Count (u16 LE)   |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! - **Version** (bits 7..4 of byte 0): Must be `1`.
//! - **Reserved** (bits 3..2 of byte 0): Set to 0.
//! - **Stereo Mode (SM)** (bits 1..0 of byte 0):
//!   - `0`: Independent (`Ch0 = Left`, `Ch1 = Right`)
//!   - `1`: Left/Side (`Ch0 = Left`, `Ch1 = Left - Right`)
//!   - `2`: Right/Side (`Ch0 = Right`, `Ch1 = Left - Right`)
//!   - `3`: Mid/Side (`Ch1 = Left - Right`, `Ch0 = Right + (Ch1 >> 1)`)
//! - **Sample Count** (bytes 1..2, u16 little-endian): Number of stereo
//!   sample pairs `n` (0..=[`MAX_SAMPLES`]).
//!
//! Following the frame header are two subframes (Channel 0 followed by
//! Channel 1).
//!
//! ## Subframe Format
//!
//! Each subframe begins with a 1-byte subframe header:
//! - Bits 7..5: Subframe Type:
//!     - `0`: **Silence** — All samples are zero (no extra payload).
//!     - `1`: **Constant** — All samples have the same value, stored as zigzag varint.
//!     - `2`: **Verbatim** — Raw uncompressed samples stored as 16-bit LE (byte-aligned).
//!     - `3..=7`: **Fixed Predictor** of order `p = type - 3` (orders 0..=4).
//! - Bits 4..2: Reserved (0).
//! - Bits 1..0: Partition Order `po`: number of Rice residual partitions
//!   is `2^po` (1, 2, or 4 partitions).
//!
//! For fixed predictor subframes (orders 0..=4):
//! 1. `p` warm-up samples are stored uncompressed as zigzag varints.
//! 2. For each of the `2^po` partitions:
//!    - A 5-bit Rice parameter `k` (0..=30).
//!    - Each residual is mapped to an unsigned integer via zigzag encoding
//!      `u = (r << 1) ^ (r >> 31)`.
//!    - If `q = u >> k < 15`: coded as `q` zeros, followed by a `1` bit,
//!      followed by the `k` least significant bits of `u`.
//!    - If `q >= 15`: coded as 15 zeros, followed by a `0` bit, followed by
//!      a full 32-bit unsigned integer holding `u`.
//! 3. Subframe bitstreams are byte-aligned after each channel.

/// Samples per channel in Latch's 10 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 480;
pub const CHANNELS: usize = 2;
/// Most samples per channel one frame may hold.
pub const MAX_SAMPLES: usize = 4096;

const PROTOCOL_VERSION: u8 = 1;

const MODE_INDEPENDENT: u8 = 0;
const MODE_LEFT_SIDE: u8 = 1;
const MODE_RIGHT_SIDE: u8 = 2;
const MODE_MID_SIDE: u8 = 3;

const SUBFRAME_SILENCE: u8 = 0;
const SUBFRAME_CONSTANT: u8 = 1;
const SUBFRAME_VERBATIM: u8 = 2;
const SUBFRAME_PREDICTOR_BASE: u8 = 3;

const ESCAPE_LIMIT: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioError {
    Truncated,
    Invalid(&'static str),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::Truncated => write!(f, "audio frame is truncated"),
            AudioError::Invalid(what) => write!(f, "audio frame is invalid: {what}"),
        }
    }
}

impl std::error::Error for AudioError {}

#[inline(always)]
fn to_zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

#[inline(always)]
fn from_zigzag(u: u32) -> i32 {
    ((u >> 1) as i32) ^ (-((u & 1) as i32))
}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    bit_buf: u64,
    bits_in_buf: u32,
}

impl<'a> BitWriter<'a> {
    #[inline(always)]
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            out,
            bit_buf: 0,
            bits_in_buf: 0,
        }
    }

    #[inline(always)]
    fn write_bits(&mut self, val: u32, n_bits: u32) {
        if n_bits == 0 {
            return;
        }
        self.bit_buf |= (val as u64) << (64 - self.bits_in_buf - n_bits);
        self.bits_in_buf += n_bits;
        while self.bits_in_buf >= 8 {
            self.out.push((self.bit_buf >> 56) as u8);
            self.bit_buf <<= 8;
            self.bits_in_buf -= 8;
        }
    }

    #[inline(always)]
    fn write_zeros(&mut self, count: u32) {
        let mut rem = count;
        while rem > 0 {
            let chunk = rem.min(32);
            self.write_bits(0, chunk);
            rem -= chunk;
        }
    }

    #[inline(always)]
    fn write_varint(&mut self, mut u: u32) {
        while u >= 0x80 {
            self.write_bits((u & 0x7f) | 0x80, 8);
            u >>= 7;
        }
        self.write_bits(u & 0x7f, 8);
    }

    #[inline(always)]
    fn flush(&mut self) {
        if self.bits_in_buf > 0 {
            self.out.push((self.bit_buf >> 56) as u8);
            self.bit_buf = 0;
            self.bits_in_buf = 0;
        }
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u64,
    bits_in_buf: u32,
}

impl<'a> BitReader<'a> {
    #[inline(always)]
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bit_buf: 0,
            bits_in_buf: 0,
        }
    }

    #[inline(always)]
    fn refill(&mut self) {
        while self.bits_in_buf <= 56 && self.pos < self.data.len() {
            self.bit_buf |= (self.data[self.pos] as u64) << (56 - self.bits_in_buf);
            self.pos += 1;
            self.bits_in_buf += 8;
        }
    }

    #[inline(always)]
    fn read_bits(&mut self, n_bits: u32) -> Result<u32, AudioError> {
        if n_bits == 0 {
            return Ok(0);
        }
        if self.bits_in_buf < n_bits {
            self.refill();
            if self.bits_in_buf < n_bits {
                return Err(AudioError::Truncated);
            }
        }
        let val = (self.bit_buf >> (64 - n_bits)) as u32;
        self.bit_buf <<= n_bits;
        self.bits_in_buf -= n_bits;
        Ok(val)
    }

    #[inline(always)]
    fn read_unary_zeros_and_stop_bit(&mut self, max_zeros: u32) -> Result<u32, AudioError> {
        let mut count = 0;
        loop {
            if self.bits_in_buf == 0 {
                self.refill();
                if self.bits_in_buf == 0 {
                    return Err(AudioError::Truncated);
                }
            }
            let zeros = (self.bit_buf.leading_zeros()).min(self.bits_in_buf);
            if count + zeros >= max_zeros {
                let take = max_zeros - count;
                self.bit_buf <<= take;
                self.bits_in_buf -= take;
                return Ok(max_zeros);
            }
            if zeros < self.bits_in_buf {
                count += zeros;
                self.bit_buf <<= zeros + 1;
                self.bits_in_buf -= zeros + 1;
                return Ok(count);
            } else {
                count += zeros;
                self.bit_buf = 0;
                self.bits_in_buf = 0;
            }
        }
    }

    #[inline(always)]
    fn read_varint(&mut self) -> Result<u32, AudioError> {
        let mut val = 0u32;
        let mut shift = 0;
        for _ in 0..5 {
            let b = self.read_bits(8)?;
            val |= (b & 0x7f) << shift;
            if (b & 0x80) == 0 {
                return Ok(val);
            }
            shift += 7;
        }
        Err(AudioError::Invalid("varint exceeded 5 bytes"))
    }

    #[inline(always)]
    fn align_byte(&mut self) {
        let rem = self.bits_in_buf % 8;
        if rem > 0 {
            self.bit_buf <<= rem;
            self.bits_in_buf -= rem;
        }
    }
}

#[inline(always)]
fn predict(order: usize, history: &[i32], i: usize) -> i32 {
    match order {
        0 => 0,
        1 => history[i - 1],
        2 => history[i - 1].wrapping_mul(2).wrapping_sub(history[i - 2]),
        3 => history[i - 1]
            .wrapping_mul(3)
            .wrapping_sub(history[i - 2].wrapping_mul(3))
            .wrapping_add(history[i - 3]),
        4 => history[i - 1]
            .wrapping_mul(4)
            .wrapping_sub(history[i - 2].wrapping_mul(6))
            .wrapping_add(history[i - 3].wrapping_mul(4))
            .wrapping_sub(history[i - 4]),
        _ => unreachable!(),
    }
}

fn estimate_order_residuals(samples: &[i32], order: usize, residuals: &mut [i32]) -> u64 {
    let n = samples.len();
    let mut sum_abs = 0u64;
    for i in order..n {
        let pred = predict(order, samples, i);
        let res = samples[i].wrapping_sub(pred);
        residuals[i - order] = res;
        sum_abs += (res as i64).unsigned_abs();
    }
    sum_abs
}

fn find_best_rice_param(residuals: &[i32]) -> u32 {
    if residuals.is_empty() {
        return 0;
    }
    let mut sum = 0u64;
    for &r in residuals {
        sum += to_zigzag(r) as u64;
    }
    let avg = sum / (residuals.len() as u64);
    if avg == 0 {
        0
    } else {
        (63 - avg.leading_zeros()).min(30)
    }
}

fn encode_channel(samples: &[i32], out: &mut Vec<u8>) {
    let n = samples.len();
    if n == 0 {
        out.push(SUBFRAME_SILENCE << 5);
        return;
    }

    // Check for silence (all zeros)
    let is_zero = samples.iter().all(|&v| v == 0);
    if is_zero {
        out.push(SUBFRAME_SILENCE << 5);
        return;
    }

    // Check for constant
    let first = samples[0];
    let is_constant = samples.iter().all(|&v| v == first);
    if is_constant {
        out.push(SUBFRAME_CONSTANT << 5);
        let mut writer = BitWriter::new(out);
        writer.write_varint(to_zigzag(first));
        writer.flush();
        return;
    }

    // Compare predictor orders 0..=4
    let mut best_order = 0;
    let mut best_sum = u64::MAX;
    let mut temp_res = vec![0i32; n];
    let mut best_res = vec![0i32; n];

    for order in 0..=4.min(n - 1) {
        let sum = estimate_order_residuals(samples, order, &mut temp_res[..n - order]);
        if sum < best_sum {
            best_sum = sum;
            best_order = order;
            best_res[..n - order].copy_from_slice(&temp_res[..n - order]);
        }
    }

    let p = best_order;
    let residuals = &best_res[..n - p];

    // Determine partition order: 0 (1 partition) or 2 (4 partitions)
    let part_order = if residuals.len() >= 64 { 2u8 } else { 0u8 };
    let num_partitions = 1usize << part_order;

    // Tentatively encode predictor subframe into a buffer
    let mut pred_buf = Vec::with_capacity(n * 2 + 16);
    let subframe_header = ((SUBFRAME_PREDICTOR_BASE + p as u8) << 5) | (part_order & 0x03);
    pred_buf.push(subframe_header);

    {
        let mut writer = BitWriter::new(&mut pred_buf);
        // Write warm-up samples
        for &s in &samples[..p] {
            writer.write_varint(to_zigzag(s));
        }

        // Write partitions
        let part_size = residuals.len() / num_partitions;
        for part_idx in 0..num_partitions {
            let start = part_idx * part_size;
            let end = if part_idx == num_partitions - 1 {
                residuals.len()
            } else {
                start + part_size
            };
            let part_slice = &residuals[start..end];
            let k = find_best_rice_param(part_slice);
            writer.write_bits(k, 5);

            for &res in part_slice {
                let u = to_zigzag(res);
                let q = u >> k;
                let rem = u & ((1 << k) - 1);
                if q < ESCAPE_LIMIT {
                    writer.write_zeros(q);
                    writer.write_bits(1, 1);
                    writer.write_bits(rem, k);
                } else {
                    writer.write_zeros(ESCAPE_LIMIT);
                    writer.write_bits(0, 1);
                    writer.write_bits(u, 32);
                }
            }
        }
        writer.flush();
    }

    // Check if verbatim is smaller than coded predictor
    let verbatim_size = 1 + n * 2;
    let fits_i16 = samples
        .iter()
        .all(|&v| v >= i16::MIN as i32 && v <= i16::MAX as i32);
    if fits_i16 && verbatim_size < pred_buf.len() {
        out.push(SUBFRAME_VERBATIM << 5);
        for &s in samples {
            out.extend_from_slice(&(s as i16).to_le_bytes());
        }
    } else {
        out.extend_from_slice(&pred_buf);
    }
}

fn decode_channel(reader: &mut BitReader, n: usize, dst: &mut [i32]) -> Result<(), AudioError> {
    if n == 0 {
        let _ = reader.read_bits(8)?;
        return Ok(());
    }

    let header = reader.read_bits(8)? as u8;
    let subframe_type = header >> 5;
    let part_order = header & 0x03;

    match subframe_type {
        SUBFRAME_SILENCE => {
            dst[..n].fill(0);
            Ok(())
        }
        SUBFRAME_CONSTANT => {
            let u = reader.read_varint()?;
            let val = from_zigzag(u);
            dst[..n].fill(val);
            Ok(())
        }
        SUBFRAME_VERBATIM => {
            reader.align_byte();
            for item in dst.iter_mut().take(n) {
                let lo = reader.read_bits(8)? as u8;
                let hi = reader.read_bits(8)? as u8;
                let s = i16::from_le_bytes([lo, hi]) as i32;
                *item = s;
            }
            Ok(())
        }
        t if (SUBFRAME_PREDICTOR_BASE..=SUBFRAME_PREDICTOR_BASE + 4).contains(&t) => {
            let p = (t - SUBFRAME_PREDICTOR_BASE) as usize;
            if p > n {
                return Err(AudioError::Invalid("predictor order exceeds sample count"));
            }

            // Read warm-up samples
            for item in dst.iter_mut().take(p) {
                let u = reader.read_varint()?;
                *item = from_zigzag(u);
            }

            let num_partitions = 1usize << part_order;
            let residuals_count = n - p;
            let part_size = residuals_count / num_partitions;

            let mut res_idx = 0;
            for part_idx in 0..num_partitions {
                let k = reader.read_bits(5)?;
                if k > 30 {
                    return Err(AudioError::Invalid("rice parameter too large"));
                }
                let count = if part_idx == num_partitions - 1 {
                    residuals_count - res_idx
                } else {
                    part_size
                };

                for _ in 0..count {
                    let zeros = reader.read_unary_zeros_and_stop_bit(ESCAPE_LIMIT)?;
                    let u = if zeros < ESCAPE_LIMIT {
                        let rem = reader.read_bits(k)?;
                        (zeros << k) | rem
                    } else {
                        let bit = reader.read_bits(1)?;
                        if bit == 1 {
                            let rem = reader.read_bits(k)?;
                            (ESCAPE_LIMIT << k) | rem
                        } else {
                            reader.read_bits(32)?
                        }
                    };
                    let residual = from_zigzag(u);
                    let i = p + res_idx;
                    let pred = predict(p, dst, i);
                    dst[i] = pred.wrapping_add(residual);
                    res_idx += 1;
                }
            }
            reader.align_byte();
            Ok(())
        }
        _ => Err(AudioError::Invalid("unknown subframe type")),
    }
}

/// Codes interleaved stereo `pcm` (an even number of samples, at most
/// `2 * MAX_SAMPLES`), appending to `out`
pub fn encode(pcm: &[i16], out: &mut Vec<u8>) {
    let n = (pcm.len() / 2).min(MAX_SAMPLES);
    if n == 0 {
        // Empty frame
        out.push((PROTOCOL_VERSION << 4) | MODE_INDEPENDENT);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.push(SUBFRAME_SILENCE << 5);
        out.push(SUBFRAME_SILENCE << 5);
        return;
    }

    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        l.push(pcm[2 * i] as i32);
        r.push(pcm[2 * i + 1] as i32);
    }

    // Evaluate stereo modes and pick the one with lowest estimated residual entropy
    let mut mode = MODE_INDEPENDENT;
    let mut best_diff = 0u64;
    for i in 0..n {
        best_diff += l[i].unsigned_abs() as u64 + r[i].unsigned_abs() as u64;
    }

    let mut ls_diff = 0u64;
    for i in 0..n {
        let side = l[i] - r[i];
        ls_diff += l[i].unsigned_abs() as u64 + side.unsigned_abs() as u64;
    }
    if ls_diff < best_diff {
        best_diff = ls_diff;
        mode = MODE_LEFT_SIDE;
    }

    let mut rs_diff = 0u64;
    for i in 0..n {
        let side = l[i] - r[i];
        rs_diff += r[i].unsigned_abs() as u64 + side.unsigned_abs() as u64;
    }
    if rs_diff < best_diff {
        best_diff = rs_diff;
        mode = MODE_RIGHT_SIDE;
    }

    let mut ms_diff = 0u64;
    for i in 0..n {
        let side = l[i] - r[i];
        let mid = r[i] + (side >> 1);
        ms_diff += mid.unsigned_abs() as u64 + side.unsigned_abs() as u64;
    }
    if ms_diff < best_diff {
        mode = MODE_MID_SIDE;
    }

    // Transform channels according to chosen stereo mode
    let mut ch0 = Vec::with_capacity(n);
    let mut ch1 = Vec::with_capacity(n);

    match mode {
        MODE_INDEPENDENT => {
            ch0.extend_from_slice(&l);
            ch1.extend_from_slice(&r);
        }
        MODE_LEFT_SIDE => {
            for i in 0..n {
                ch0.push(l[i]);
                ch1.push(l[i] - r[i]);
            }
        }
        MODE_RIGHT_SIDE => {
            for i in 0..n {
                ch0.push(r[i]);
                ch1.push(l[i] - r[i]);
            }
        }
        MODE_MID_SIDE => {
            for i in 0..n {
                let side = l[i] - r[i];
                let mid = r[i] + (side >> 1);
                ch0.push(mid);
                ch1.push(side);
            }
        }
        _ => unreachable!(),
    }

    // Frame header
    out.push((PROTOCOL_VERSION << 4) | (mode & 0x03));
    out.extend_from_slice(&(n as u16).to_le_bytes());

    // Encode subframes
    encode_channel(&ch0, out);
    encode_channel(&ch1, out);
    // Side channels can exceed i16; independent verbatim is a hard wire bound.
    if out.len() > 3 + 2 * (1 + 2 * n) {
        out.clear();
        out.push(PROTOCOL_VERSION << 4);
        out.extend_from_slice(&(n as u16).to_le_bytes());
        for channel in 0..2 {
            out.push(SUBFRAME_VERBATIM << 5);
            for pair in pcm.as_chunks::<2>().0 {
                out.extend_from_slice(&pair[channel].to_le_bytes());
            }
        }
    }
}

/// Decodes one frame into `pcm` (cleared first), interleaved stereo.
pub fn decode(data: &[u8], pcm: &mut Vec<i16>) -> Result<(), AudioError> {
    pcm.clear();

    if data.len() < 3 {
        return Err(AudioError::Truncated);
    }

    let header_byte = data[0];
    let version = header_byte >> 4;
    if version != PROTOCOL_VERSION {
        return Err(AudioError::Invalid("unsupported protocol version"));
    }

    let mode = header_byte & 0x03;
    let n = u16::from_le_bytes([data[1], data[2]]) as usize;
    if n > MAX_SAMPLES {
        return Err(AudioError::Invalid("sample count exceeds MAX_SAMPLES"));
    }

    if n == 0 {
        return Ok(());
    }

    let mut ch0 = vec![0i32; n];
    let mut ch1 = vec![0i32; n];

    let mut reader = BitReader::new(&data[3..]);
    decode_channel(&mut reader, n, &mut ch0)?;
    decode_channel(&mut reader, n, &mut ch1)?;

    pcm.reserve(2 * n);
    match mode {
        MODE_INDEPENDENT => {
            for i in 0..n {
                pcm.push(ch0[i] as i16);
                pcm.push(ch1[i] as i16);
            }
        }
        MODE_LEFT_SIDE => {
            for i in 0..n {
                let left = ch0[i] as i16;
                let right = ch0[i].wrapping_sub(ch1[i]) as i16;
                pcm.push(left);
                pcm.push(right);
            }
        }
        MODE_RIGHT_SIDE => {
            for i in 0..n {
                let right = ch0[i] as i16;
                let left = ch0[i].wrapping_add(ch1[i]) as i16;
                pcm.push(left);
                pcm.push(right);
            }
        }
        MODE_MID_SIDE => {
            for i in 0..n {
                let mid = ch0[i];
                let side = ch1[i];
                let right = mid.wrapping_sub(side >> 1) as i16;
                let left = right.wrapping_add(side as i16);
                pcm.push(left);
                pcm.push(right);
            }
        }
        _ => return Err(AudioError::Invalid("unknown stereo mode")),
    }

    Ok(())
}

#[cfg(test)]
mod transport_bounds {
    #[test]
    fn full_range_stereo_always_fits_verbatim_bound() {
        let mut seed = 7u32;
        for pairs in [1, 240, 480] {
            for kind in 0..8 {
                let pcm: Vec<i16> = (0..pairs * 2)
                    .map(|i| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        match kind {
                            0 => {
                                if i % 2 == 0 {
                                    i16::MIN
                                } else {
                                    i16::MAX
                                }
                            }
                            1 => {
                                if i % 2 == 0 {
                                    i16::MAX
                                } else {
                                    i16::MIN
                                }
                            }
                            _ => seed as i16,
                        }
                    })
                    .collect();
                let mut encoded = Vec::new();
                super::encode(&pcm, &mut encoded);
                assert!(encoded.len() <= 3 + 2 * (1 + pairs * 2));
                let mut decoded = Vec::new();
                super::decode(&encoded, &mut decoded).unwrap();
                assert_eq!(decoded, pcm);
            }
        }
    }
}
