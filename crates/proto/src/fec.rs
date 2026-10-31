//! Systematic Reed–Solomon erasure coding over GF(2⁸), polynomial 0x11D.
//!
//! A block has `data` data shards and `parity` parity shards, all the same
//! length, `data + parity <= 255`. Parity shard `i` is
//! `Σ_j C[i][j] · data[j]`, where `C[i][j] = 1 / (x_i ⊕ y_j)` with
//! `x_i = data + i` and `y_j = j` (a Cauchy matrix: every square submatrix
//! is invertible, so any `data` shards of a block rebuild it). Host and
//! viewer must compute exactly this matrix; it is part of the protocol.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

#[repr(C, align(32))]
struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
    mul: [[u8; 256]; 256],
    mul_lo: [[u8; 16]; 256],
    mul_hi: [[u8; 16]; 256],
}

const fn tables() -> Tables {
    let mut exp = [0; 512];
    let mut log = [0; 256];
    let mut x = 1u16;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 256 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    let mut mul = [[0; 256]; 256];
    let mut a = 1;
    while a < 256 {
        let mut b = 1;
        while b < 256 {
            mul[a][b] = exp[log[a] as usize + log[b] as usize];
            b += 1;
        }
        a += 1;
    }
    let mut mul_lo = [[0; 16]; 256];
    let mut mul_hi = [[0; 16]; 256];
    let mut c = 0;
    while c < 256 {
        let mut n = 0;
        while n < 16 {
            mul_lo[c][n] = mul[c][n];
            mul_hi[c][n] = mul[c][n << 4];
            n += 1;
        }
        c += 1;
    }
    Tables {
        exp,
        log,
        mul,
        mul_lo,
        mul_hi,
    }
}

static TABLES: Tables = tables();

fn inv(x: u8) -> u8 {
    TABLES.exp[255 - TABLES.log[x as usize] as usize]
}

#[inline(always)]
#[allow(dead_code)]
fn addmul_scalar(dst: &mut [u8], src: &[u8], c: u8) {
    if c == 0 {
        return;
    }
    if c == 1 {
        for (d, &s) in dst.iter_mut().zip(src) {
            *d ^= s;
        }
        return;
    }
    let row = &TABLES.mul[c as usize];
    for (d, &s) in dst.iter_mut().zip(src) {
        *d ^= row[s as usize];
    }
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum X86SimdLevel {
    Scalar,
    Ssse3,
    Avx2,
}

#[cfg(target_arch = "x86_64")]
#[inline]
pub(crate) fn x86_simd_level() -> X86SimdLevel {
    use std::sync::atomic::{AtomicU8, Ordering};
    static LEVEL: AtomicU8 = AtomicU8::new(0);
    let val = LEVEL.load(Ordering::Relaxed);
    if val != 0 {
        return match val {
            3 => X86SimdLevel::Avx2,
            2 => X86SimdLevel::Ssse3,
            _ => X86SimdLevel::Scalar,
        };
    }
    let detected = if is_x86_feature_detected!("avx2") {
        X86SimdLevel::Avx2
    } else if is_x86_feature_detected!("ssse3") {
        X86SimdLevel::Ssse3
    } else {
        X86SimdLevel::Scalar
    };
    let val = match detected {
        X86SimdLevel::Avx2 => 3,
        X86SimdLevel::Ssse3 => 2,
        X86SimdLevel::Scalar => 1,
    };
    LEVEL.store(val, Ordering::Relaxed);
    detected
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
pub(crate) unsafe fn addmul_ssse3(dst: &mut [u8], src: &[u8], c: u8) {
    use std::arch::x86_64::*;

    if c == 0 {
        return;
    }

    let mut len = dst.len();
    let mut d_ptr = dst.as_mut_ptr();
    let mut s_ptr = src.as_ptr();

    if c == 1 {
        while len >= 64 {
            let s0 = _mm_loadu_si128(s_ptr as *const __m128i);
            let s1 = _mm_loadu_si128(s_ptr.add(16) as *const __m128i);
            let s2 = _mm_loadu_si128(s_ptr.add(32) as *const __m128i);
            let s3 = _mm_loadu_si128(s_ptr.add(48) as *const __m128i);
            let d0 = _mm_loadu_si128(d_ptr as *const __m128i);
            let d1 = _mm_loadu_si128(d_ptr.add(16) as *const __m128i);
            let d2 = _mm_loadu_si128(d_ptr.add(32) as *const __m128i);
            let d3 = _mm_loadu_si128(d_ptr.add(48) as *const __m128i);
            _mm_storeu_si128(d_ptr as *mut __m128i, _mm_xor_si128(d0, s0));
            _mm_storeu_si128(d_ptr.add(16) as *mut __m128i, _mm_xor_si128(d1, s1));
            _mm_storeu_si128(d_ptr.add(32) as *mut __m128i, _mm_xor_si128(d2, s2));
            _mm_storeu_si128(d_ptr.add(48) as *mut __m128i, _mm_xor_si128(d3, s3));
            d_ptr = d_ptr.add(64);
            s_ptr = s_ptr.add(64);
            len -= 64;
        }
        while len >= 16 {
            let s = _mm_loadu_si128(s_ptr as *const __m128i);
            let d = _mm_loadu_si128(d_ptr as *const __m128i);
            _mm_storeu_si128(d_ptr as *mut __m128i, _mm_xor_si128(d, s));
            d_ptr = d_ptr.add(16);
            s_ptr = s_ptr.add(16);
            len -= 16;
        }
        if len > 0 {
            let d_slice = std::slice::from_raw_parts_mut(d_ptr, len);
            let s_slice = std::slice::from_raw_parts(s_ptr, len);
            for (d, &s) in d_slice.iter_mut().zip(s_slice) {
                *d ^= s;
            }
        }
        return;
    }

    let tbl_lo = _mm_loadu_si128(TABLES.mul_lo[c as usize].as_ptr() as *const __m128i);
    let tbl_hi = _mm_loadu_si128(TABLES.mul_hi[c as usize].as_ptr() as *const __m128i);
    let mask_lo = _mm_set1_epi8(0x0f);

    while len >= 64 {
        let s0 = _mm_loadu_si128(s_ptr as *const __m128i);
        let s1 = _mm_loadu_si128(s_ptr.add(16) as *const __m128i);
        let s2 = _mm_loadu_si128(s_ptr.add(32) as *const __m128i);
        let s3 = _mm_loadu_si128(s_ptr.add(48) as *const __m128i);

        let lo0 = _mm_and_si128(s0, mask_lo);
        let hi0 = _mm_and_si128(_mm_srli_epi16(s0, 4), mask_lo);
        let p0 = _mm_xor_si128(_mm_shuffle_epi8(tbl_lo, lo0), _mm_shuffle_epi8(tbl_hi, hi0));

        let lo1 = _mm_and_si128(s1, mask_lo);
        let hi1 = _mm_and_si128(_mm_srli_epi16(s1, 4), mask_lo);
        let p1 = _mm_xor_si128(_mm_shuffle_epi8(tbl_lo, lo1), _mm_shuffle_epi8(tbl_hi, hi1));

        let lo2 = _mm_and_si128(s2, mask_lo);
        let hi2 = _mm_and_si128(_mm_srli_epi16(s2, 4), mask_lo);
        let p2 = _mm_xor_si128(_mm_shuffle_epi8(tbl_lo, lo2), _mm_shuffle_epi8(tbl_hi, hi2));

        let lo3 = _mm_and_si128(s3, mask_lo);
        let hi3 = _mm_and_si128(_mm_srli_epi16(s3, 4), mask_lo);
        let p3 = _mm_xor_si128(_mm_shuffle_epi8(tbl_lo, lo3), _mm_shuffle_epi8(tbl_hi, hi3));

        let d0 = _mm_loadu_si128(d_ptr as *const __m128i);
        let d1 = _mm_loadu_si128(d_ptr.add(16) as *const __m128i);
        let d2 = _mm_loadu_si128(d_ptr.add(32) as *const __m128i);
        let d3 = _mm_loadu_si128(d_ptr.add(48) as *const __m128i);

        _mm_storeu_si128(d_ptr as *mut __m128i, _mm_xor_si128(d0, p0));
        _mm_storeu_si128(d_ptr.add(16) as *mut __m128i, _mm_xor_si128(d1, p1));
        _mm_storeu_si128(d_ptr.add(32) as *mut __m128i, _mm_xor_si128(d2, p2));
        _mm_storeu_si128(d_ptr.add(48) as *mut __m128i, _mm_xor_si128(d3, p3));

        d_ptr = d_ptr.add(64);
        s_ptr = s_ptr.add(64);
        len -= 64;
    }

    while len >= 16 {
        let s = _mm_loadu_si128(s_ptr as *const __m128i);
        let lo = _mm_and_si128(s, mask_lo);
        let hi = _mm_and_si128(_mm_srli_epi16(s, 4), mask_lo);
        let p = _mm_xor_si128(_mm_shuffle_epi8(tbl_lo, lo), _mm_shuffle_epi8(tbl_hi, hi));
        let d = _mm_loadu_si128(d_ptr as *const __m128i);
        _mm_storeu_si128(d_ptr as *mut __m128i, _mm_xor_si128(d, p));
        d_ptr = d_ptr.add(16);
        s_ptr = s_ptr.add(16);
        len -= 16;
    }

    if len > 0 {
        let row = &TABLES.mul[c as usize];
        let d_slice = std::slice::from_raw_parts_mut(d_ptr, len);
        let s_slice = std::slice::from_raw_parts(s_ptr, len);
        for (d, &s) in d_slice.iter_mut().zip(s_slice) {
            *d ^= row[s as usize];
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn addmul_avx2(dst: &mut [u8], src: &[u8], c: u8) {
    use std::arch::x86_64::*;

    if c == 0 {
        return;
    }

    let mut len = dst.len();
    let mut d_ptr = dst.as_mut_ptr();
    let mut s_ptr = src.as_ptr();

    if c == 1 {
        while len >= 64 {
            let s0 = _mm256_loadu_si256(s_ptr as *const __m256i);
            let s1 = _mm256_loadu_si256(s_ptr.add(32) as *const __m256i);
            let d0 = _mm256_loadu_si256(d_ptr as *const __m256i);
            let d1 = _mm256_loadu_si256(d_ptr.add(32) as *const __m256i);
            _mm256_storeu_si256(d_ptr as *mut __m256i, _mm256_xor_si256(d0, s0));
            _mm256_storeu_si256(d_ptr.add(32) as *mut __m256i, _mm256_xor_si256(d1, s1));
            d_ptr = d_ptr.add(64);
            s_ptr = s_ptr.add(64);
            len -= 64;
        }
        while len >= 32 {
            let s = _mm256_loadu_si256(s_ptr as *const __m256i);
            let d = _mm256_loadu_si256(d_ptr as *const __m256i);
            _mm256_storeu_si256(d_ptr as *mut __m256i, _mm256_xor_si256(d, s));
            d_ptr = d_ptr.add(32);
            s_ptr = s_ptr.add(32);
            len -= 32;
        }
        while len >= 16 {
            let s = _mm_loadu_si128(s_ptr as *const __m128i);
            let d = _mm_loadu_si128(d_ptr as *const __m128i);
            _mm_storeu_si128(d_ptr as *mut __m128i, _mm_xor_si128(d, s));
            d_ptr = d_ptr.add(16);
            s_ptr = s_ptr.add(16);
            len -= 16;
        }
        if len > 0 {
            let d_slice = std::slice::from_raw_parts_mut(d_ptr, len);
            let s_slice = std::slice::from_raw_parts(s_ptr, len);
            for (d, &s) in d_slice.iter_mut().zip(s_slice) {
                *d ^= s;
            }
        }
        return;
    }

    let tbl_lo128 = _mm_loadu_si128(TABLES.mul_lo[c as usize].as_ptr() as *const __m128i);
    let tbl_hi128 = _mm_loadu_si128(TABLES.mul_hi[c as usize].as_ptr() as *const __m128i);
    let tbl_lo256 = _mm256_broadcastsi128_si256(tbl_lo128);
    let tbl_hi256 = _mm256_broadcastsi128_si256(tbl_hi128);
    let mask_lo256 = _mm256_set1_epi8(0x0f);

    while len >= 64 {
        let s0 = _mm256_loadu_si256(s_ptr as *const __m256i);
        let s1 = _mm256_loadu_si256(s_ptr.add(32) as *const __m256i);

        let lo0 = _mm256_and_si256(s0, mask_lo256);
        let hi0 = _mm256_and_si256(_mm256_srli_epi16(s0, 4), mask_lo256);
        let p0 = _mm256_xor_si256(
            _mm256_shuffle_epi8(tbl_lo256, lo0),
            _mm256_shuffle_epi8(tbl_hi256, hi0),
        );

        let lo1 = _mm256_and_si256(s1, mask_lo256);
        let hi1 = _mm256_and_si256(_mm256_srli_epi16(s1, 4), mask_lo256);
        let p1 = _mm256_xor_si256(
            _mm256_shuffle_epi8(tbl_lo256, lo1),
            _mm256_shuffle_epi8(tbl_hi256, hi1),
        );

        let d0 = _mm256_loadu_si256(d_ptr as *const __m256i);
        let d1 = _mm256_loadu_si256(d_ptr.add(32) as *const __m256i);

        _mm256_storeu_si256(d_ptr as *mut __m256i, _mm256_xor_si256(d0, p0));
        _mm256_storeu_si256(d_ptr.add(32) as *mut __m256i, _mm256_xor_si256(d1, p1));

        d_ptr = d_ptr.add(64);
        s_ptr = s_ptr.add(64);
        len -= 64;
    }

    while len >= 32 {
        let s = _mm256_loadu_si256(s_ptr as *const __m256i);
        let lo = _mm256_and_si256(s, mask_lo256);
        let hi = _mm256_and_si256(_mm256_srli_epi16(s, 4), mask_lo256);
        let p = _mm256_xor_si256(
            _mm256_shuffle_epi8(tbl_lo256, lo),
            _mm256_shuffle_epi8(tbl_hi256, hi),
        );
        let d = _mm256_loadu_si256(d_ptr as *const __m256i);
        _mm256_storeu_si256(d_ptr as *mut __m256i, _mm256_xor_si256(d, p));
        d_ptr = d_ptr.add(32);
        s_ptr = s_ptr.add(32);
        len -= 32;
    }

    while len >= 16 {
        let s = _mm_loadu_si128(s_ptr as *const __m128i);
        let mask_lo128 = _mm_set1_epi8(0x0f);
        let lo = _mm_and_si128(s, mask_lo128);
        let hi = _mm_and_si128(_mm_srli_epi16(s, 4), mask_lo128);
        let p = _mm_xor_si128(
            _mm_shuffle_epi8(tbl_lo128, lo),
            _mm_shuffle_epi8(tbl_hi128, hi),
        );
        let d = _mm_loadu_si128(d_ptr as *const __m128i);
        _mm_storeu_si128(d_ptr as *mut __m128i, _mm_xor_si128(d, p));
        d_ptr = d_ptr.add(16);
        s_ptr = s_ptr.add(16);
        len -= 16;
    }

    if len > 0 {
        let row = &TABLES.mul[c as usize];
        let d_slice = std::slice::from_raw_parts_mut(d_ptr, len);
        let s_slice = std::slice::from_raw_parts(s_ptr, len);
        for (d, &s) in d_slice.iter_mut().zip(s_slice) {
            *d ^= row[s as usize];
        }
    }
}

#[inline(always)]
fn addmul(dst: &mut [u8], src: &[u8], c: u8) {
    if c == 0 {
        return;
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        addmul_neon(dst, src, c);
    }
    #[cfg(target_arch = "x86_64")]
    {
        match x86_simd_level() {
            X86SimdLevel::Avx2 => unsafe { addmul_avx2(dst, src, c) },
            X86SimdLevel::Ssse3 => unsafe { addmul_ssse3(dst, src, c) },
            X86SimdLevel::Scalar => addmul_scalar(dst, src, c),
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        addmul_scalar(dst, src, c);
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn addmul_neon(dst: &mut [u8], src: &[u8], c: u8) {
    use std::arch::aarch64::*;

    let mut d_ptr = dst.as_mut_ptr();
    let mut s_ptr = src.as_ptr();
    let mut len = dst.len();

    if c == 1 {
        while len >= 16 {
            let s = vld1q_u8(s_ptr);
            let d = vld1q_u8(d_ptr);
            vst1q_u8(d_ptr, veorq_u8(d, s));
            d_ptr = d_ptr.add(16);
            s_ptr = s_ptr.add(16);
            len -= 16;
        }
        if len > 0 {
            let d_slice = std::slice::from_raw_parts_mut(d_ptr, len);
            let s_slice = std::slice::from_raw_parts(s_ptr, len);
            for (d, &s) in d_slice.iter_mut().zip(s_slice) {
                *d ^= s;
            }
        }
        return;
    }

    let row = &TABLES.mul[c as usize];
    let mut lo_arr = [0u8; 16];
    let mut hi_arr = [0u8; 16];
    for i in 0..16 {
        lo_arr[i] = row[i];
        hi_arr[i] = row[i << 4];
    }
    let tbl_lo = vld1q_u8(lo_arr.as_ptr());
    let tbl_hi = vld1q_u8(hi_arr.as_ptr());
    let mask_lo = vdupq_n_u8(0x0f);

    while len >= 32 {
        let s0 = vld1q_u8(s_ptr);
        let s1 = vld1q_u8(s_ptr.add(16));
        let p0 = veorq_u8(
            vqtbl1q_u8(tbl_lo, vandq_u8(s0, mask_lo)),
            vqtbl1q_u8(tbl_hi, vshrq_n_u8(s0, 4)),
        );
        let p1 = veorq_u8(
            vqtbl1q_u8(tbl_lo, vandq_u8(s1, mask_lo)),
            vqtbl1q_u8(tbl_hi, vshrq_n_u8(s1, 4)),
        );
        let d0 = vld1q_u8(d_ptr);
        let d1 = vld1q_u8(d_ptr.add(16));
        vst1q_u8(d_ptr, veorq_u8(d0, p0));
        vst1q_u8(d_ptr.add(16), veorq_u8(d1, p1));
        d_ptr = d_ptr.add(32);
        s_ptr = s_ptr.add(32);
        len -= 32;
    }

    while len >= 16 {
        let s = vld1q_u8(s_ptr);
        let p = veorq_u8(
            vqtbl1q_u8(tbl_lo, vandq_u8(s, mask_lo)),
            vqtbl1q_u8(tbl_hi, vshrq_n_u8(s, 4)),
        );
        let d = vld1q_u8(d_ptr);
        vst1q_u8(d_ptr, veorq_u8(d, p));
        d_ptr = d_ptr.add(16);
        s_ptr = s_ptr.add(16);
        len -= 16;
    }

    if len > 0 {
        let d_slice = std::slice::from_raw_parts_mut(d_ptr, len);
        let s_slice = std::slice::from_raw_parts(s_ptr, len);
        for (d, &s) in d_slice.iter_mut().zip(s_slice) {
            *d ^= row[s as usize];
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecError {
    /// `data` is 0, or `data + parity` is over 255.
    Shape,
    /// Shards of different lengths.
    Length,
    /// Fewer than `data` shards present.
    TooFewShards,
}

impl std::fmt::Display for FecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FecError::Shape => write!(f, "bad FEC block shape"),
            FecError::Length => write!(f, "FEC shards differ in length"),
            FecError::TooFewShards => write!(f, "too few shards to rebuild the FEC block"),
        }
    }
}

impl std::error::Error for FecError {}

/// Chunk size for processing shard data in L1-cache-friendly blocks.
const CHUNK_SIZE: usize = 2048;

/// The coder for one block shape. Cheap to build; [`ReedSolomon::cached`]
/// keeps the recently used shapes.
pub struct ReedSolomon {
    data: usize,
    parity: usize,
}

impl ReedSolomon {
    pub fn new(data: usize, parity: usize) -> Result<Self, FecError> {
        if data == 0 || data.checked_add(parity).is_none_or(|n| n > 255) {
            return Err(FecError::Shape);
        }
        Ok(Self { data, parity })
    }

    /// A shared coder for this shape from a small cache.
    pub fn cached(data: usize, parity: usize) -> Result<std::sync::Arc<Self>, FecError> {
        static CACHE: OnceLock<Mutex<VecDeque<Arc<ReedSolomon>>>> = OnceLock::new();
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(VecDeque::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(i) = cache
            .iter()
            .position(|c| c.data == data && c.parity == parity)
        {
            let coder = cache.remove(i).expect("cache index exists");
            cache.push_front(coder.clone());
            return Ok(coder);
        }
        let coder = Arc::new(Self::new(data, parity)?);
        cache.push_front(coder.clone());
        if cache.len() > 32 {
            cache.pop_back();
        }
        Ok(coder)
    }

    pub fn data(&self) -> usize {
        self.data
    }

    pub fn parity(&self) -> usize {
        self.parity
    }

    /// Fills `parity` from `data`. Every slice has the same length.
    pub fn encode(&self, data: &[&[u8]], parity: &mut [&mut [u8]]) -> Result<(), FecError> {
        if data.len() != self.data || parity.len() != self.parity {
            return Err(FecError::Shape);
        }
        if self.parity == 0 {
            return Ok(());
        }
        let len = data[0].len();
        if data.iter().any(|s| s.len() != len) || parity.iter().any(|s| s.len() != len) {
            return Err(FecError::Length);
        }
        for (i, out) in parity.iter_mut().enumerate() {
            out.fill(0);
            for offset in (0..len).step_by(CHUNK_SIZE) {
                let chunk_len = (len - offset).min(CHUNK_SIZE);
                let out_chunk = &mut out[offset..offset + chunk_len];
                for (j, shard) in data.iter().enumerate() {
                    let c = inv((self.data + i) as u8 ^ j as u8);
                    addmul(out_chunk, &shard[offset..offset + chunk_len], c);
                }
            }
        }
        Ok(())
    }

    /// `shards` holds the block's `data + parity` shards in order, `None`
    /// for each missing one. Rebuilds every missing data shard in place
    /// (missing parity shards may stay `None`).
    pub fn reconstruct(&self, shards: &mut [Option<Vec<u8>>]) -> Result<(), FecError> {
        if shards.len() != self.data + self.parity {
            return Err(FecError::Shape);
        }
        let mut len = None;
        for s in shards.iter().flatten() {
            if len.is_some_and(|n| n != s.len()) {
                return Err(FecError::Length);
            }
            len = Some(s.len());
        }
        let mut selected = [0usize; 256];
        let mut selected_count = 0;
        for (i, s) in shards.iter().enumerate() {
            if s.is_some() {
                selected[selected_count] = i;
                selected_count += 1;
                if selected_count == self.data {
                    break;
                }
            }
        }
        if selected_count < self.data {
            return Err(FecError::TooFewShards);
        }
        if shards[..self.data].iter().all(Option::is_some) {
            return Ok(());
        }
        let d = self.data;
        let stride = d * 2;
        let mut matrix = vec![0u8; d * stride];
        let selected = &selected[..selected_count];
        for (r, &idx) in selected.iter().enumerate() {
            let row = &mut matrix[r * stride..r * stride + stride];
            if idx < d {
                row[idx] = 1;
            } else {
                for (j, value) in row[..d].iter_mut().enumerate() {
                    *value = inv(idx as u8 ^ j as u8);
                }
            }
            row[d + r] = 1;
        }

        let mut pivot_row = [0u8; 512];
        for col in 0..d {
            let pivot = (col..d)
                .find(|&r| matrix[r * stride + col] != 0)
                .ok_or(FecError::Shape)?;
            if pivot != col {
                let (first, second) = if col < pivot {
                    let (left, right) = matrix.split_at_mut(pivot * stride);
                    (
                        &mut left[col * stride..col * stride + stride],
                        &mut right[..stride],
                    )
                } else {
                    let (left, right) = matrix.split_at_mut(col * stride);
                    (
                        &mut right[..stride],
                        &mut left[pivot * stride..pivot * stride + stride],
                    )
                };
                first.swap_with_slice(second);
            }
            let col_row = &mut matrix[col * stride..col * stride + stride];
            let scale = inv(col_row[col]);
            let scale_mul = &TABLES.mul[scale as usize];
            for v in col_row.iter_mut() {
                *v = scale_mul[*v as usize];
            }
            pivot_row[..stride].copy_from_slice(col_row);
            let pivot_slice = &pivot_row[..stride];

            for r in 0..d {
                if r != col {
                    let other = &mut matrix[r * stride..r * stride + stride];
                    let c = other[col];
                    if c != 0 {
                        addmul(other, pivot_slice, c);
                    }
                }
            }
        }

        let shard_len = len.unwrap_or(0);
        for target in 0..d {
            if shards[target].is_none() {
                let mut result = vec![0u8; shard_len];
                let target_row = &matrix[target * stride..target * stride + stride];
                for offset in (0..shard_len).step_by(CHUNK_SIZE) {
                    let chunk_len = (shard_len - offset).min(CHUNK_SIZE);
                    let res_chunk = &mut result[offset..offset + chunk_len];
                    for (r, &idx) in selected.iter().enumerate() {
                        let c = target_row[d + r];
                        if c != 0 {
                            if let Some(src) = &shards[idx] {
                                addmul(res_chunk, &src[offset..offset + chunk_len], c);
                            }
                        }
                    }
                }
                shards[target] = Some(result);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn encode_scalar(
        data_len: usize,
        data: &[&[u8]],
        parity: &mut [&mut [u8]],
    ) -> Result<(), FecError> {
        for (i, out) in parity.iter_mut().enumerate() {
            out.fill(0);
            for (j, shard) in data.iter().enumerate() {
                addmul_scalar(out, shard, inv((data_len + i) as u8 ^ j as u8));
            }
        }
        Ok(())
    }

    #[test]
    fn test_simd_addmul_matches_scalar() {
        let mut seed = 0x854378129a3e2101u64;
        let mut prng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for len in 0..=200 {
            let mut src = vec![0u8; len];
            let mut dst_orig = vec![0u8; len];
            for i in 0..len {
                src[i] = prng() as u8;
                dst_orig[i] = prng() as u8;
            }

            for c in 0..=255u8 {
                let mut dst_scalar = dst_orig.clone();
                addmul_scalar(&mut dst_scalar, &src, c);

                let mut dst_simd = dst_orig.clone();
                addmul(&mut dst_simd, &src, c);
                assert_eq!(
                    dst_simd, dst_scalar,
                    "addmul dispatch mismatch for c={}, len={}",
                    c, len
                );

                #[cfg(target_arch = "x86_64")]
                if is_x86_feature_detected!("ssse3") {
                    let mut dst_ssse3 = dst_orig.clone();
                    unsafe {
                        addmul_ssse3(&mut dst_ssse3, &src, c);
                    }
                    assert_eq!(
                        dst_ssse3, dst_scalar,
                        "addmul_ssse3 mismatch for c={}, len={}",
                        c, len
                    );
                }

                #[cfg(target_arch = "x86_64")]
                if is_x86_feature_detected!("avx2") {
                    let mut dst_avx2 = dst_orig.clone();
                    unsafe {
                        addmul_avx2(&mut dst_avx2, &src, c);
                    }
                    assert_eq!(
                        dst_avx2, dst_scalar,
                        "addmul_avx2 mismatch for c={}, len={}",
                        c, len
                    );
                }
            }
        }
    }

    #[test]
    fn test_encode_matches_scalar() {
        let mut seed = 0x9876543210fedcbau64;
        let mut prng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let cases = [
            (1, 1, 10),
            (2, 2, 20),
            (4, 3, 31),
            (8, 4, 100),
            (16, 8, 250),
            (64, 7, 1172),
            (64, 16, 4096),
        ];

        for (d, p, len) in cases {
            let coder = ReedSolomon::new(d, p).unwrap();
            let data: Vec<Vec<u8>> = (0..d)
                .map(|_| (0..len).map(|_| prng() as u8).collect())
                .collect();
            let refs: Vec<_> = data.iter().map(Vec::as_slice).collect();

            let mut parity_scalar = vec![vec![0; len]; p];
            let mut out_scalar: Vec<_> = parity_scalar.iter_mut().map(Vec::as_mut_slice).collect();
            encode_scalar(d, &refs, &mut out_scalar).unwrap();

            let mut parity_simd = vec![vec![0; len]; p];
            let mut out_simd: Vec<_> = parity_simd.iter_mut().map(Vec::as_mut_slice).collect();
            coder.encode(&refs, &mut out_simd).unwrap();

            assert_eq!(
                parity_simd, parity_scalar,
                "encode mismatch for shape {}x{}, len={}",
                d, p, len
            );
        }
    }

    #[test]
    fn multiply_accumulate_matches_scalar_for_all_coefficients() {
        let mut seed = 0x12345678u32;
        for len in [0, 1, 15, 16, 17, 31, 32, 33, 255, 1172] {
            for offset in 0..16 {
                let mut src = vec![0; len + offset];
                let mut original = vec![0; len + offset + 17];
                for v in src.iter_mut().chain(original.iter_mut()) {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    *v = seed as u8;
                }
                for c in 0..=255 {
                    let mut actual = original.clone();
                    let mut expected = original.clone();
                    addmul(&mut actual[offset..offset + len], &src[offset..], c);
                    addmul_scalar(&mut expected[offset..offset + len], &src[offset..], c);
                    assert_eq!(actual, expected, "len={len} offset={offset} c={c}");
                }
            }
        }
    }

    #[test]
    fn exhaustive_small() {
        for d in 1..=6 {
            for p in 1..=3 {
                let coder = ReedSolomon::new(d, p).unwrap();
                let data: Vec<Vec<u8>> = (0..d)
                    .map(|i| (0..31).map(|j| (i * 31 + j) as u8).collect())
                    .collect();
                let refs: Vec<_> = data.iter().map(Vec::as_slice).collect();
                let mut parity = vec![vec![0; 31]; p];
                let mut out: Vec<_> = parity.iter_mut().map(Vec::as_mut_slice).collect();
                coder.encode(&refs, &mut out).unwrap();
                let all: Vec<_> = data.iter().chain(&parity).cloned().collect();
                for mask in 0usize..(1 << (d + p)) {
                    if mask.count_ones() as usize > p {
                        continue;
                    }
                    let mut shards: Vec<_> = all
                        .iter()
                        .enumerate()
                        .map(|(i, s)| {
                            if mask & (1 << i) == 0 {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    coder.reconstruct(&mut shards).unwrap();
                    for i in 0..d {
                        assert_eq!(shards[i].as_ref().unwrap(), &data[i]);
                    }
                }
            }
        }
    }

    #[test]
    fn random_patterns_large() {
        let mut seed = 0x123456789abcdef0u64;
        let mut prng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for (d, p) in [(64, 64), (200, 55)] {
            let coder = ReedSolomon::new(d, p).unwrap();
            let len = 100;
            let data: Vec<Vec<u8>> = (0..d)
                .map(|_| (0..len).map(|_| prng() as u8).collect())
                .collect();
            let refs: Vec<_> = data.iter().map(Vec::as_slice).collect();
            let mut parity = vec![vec![0; len]; p];
            let mut out: Vec<_> = parity.iter_mut().map(Vec::as_mut_slice).collect();
            coder.encode(&refs, &mut out).unwrap();

            let all: Vec<_> = data.iter().chain(&parity).cloned().collect();

            // Run 5 different random erasure patterns of size p
            for _ in 0..5 {
                let mut indices: Vec<usize> = (0..d + p).collect();
                for i in (1..indices.len()).rev() {
                    let j = (prng() as usize) % (i + 1);
                    indices.swap(i, j);
                }
                let erased = &indices[..p];
                let mut shards: Vec<Option<Vec<u8>>> = all.iter().cloned().map(Some).collect();
                for &idx in erased {
                    shards[idx] = None;
                }
                coder.reconstruct(&mut shards).unwrap();
                for i in 0..d {
                    assert_eq!(shards[i].as_ref().unwrap(), &data[i]);
                }
            }
        }
    }

    #[test]
    fn mismatched_lengths() {
        let coder = ReedSolomon::new(2, 2).unwrap();
        let s1 = vec![1u8; 10];
        let s2 = vec![2u8; 11];
        let mut p1 = vec![0u8; 10];
        let mut p2 = vec![0u8; 10];
        assert_eq!(
            coder.encode(&[&s1, &s2], &mut [&mut p1, &mut p2]),
            Err(FecError::Length)
        );

        let mut shards = vec![Some(vec![0; 10]), Some(vec![0; 11]), None, None];
        assert_eq!(coder.reconstruct(&mut shards), Err(FecError::Length));
    }

    #[test]
    fn too_few_shards() {
        let coder = ReedSolomon::new(4, 2).unwrap();
        let mut shards = vec![
            Some(vec![0; 10]),
            Some(vec![0; 10]),
            Some(vec![0; 10]),
            None,
            None,
            None,
        ];
        assert_eq!(coder.reconstruct(&mut shards), Err(FecError::TooFewShards));
    }

    #[test]
    fn parity_only_reconstruction() {
        let coder = ReedSolomon::new(4, 4).unwrap();
        let data: Vec<Vec<u8>> = (0..4).map(|i| vec![i as u8 + 1; 20]).collect();
        let refs: Vec<_> = data.iter().map(Vec::as_slice).collect();
        let mut parity = vec![vec![0; 20]; 4];
        let mut out: Vec<_> = parity.iter_mut().map(Vec::as_mut_slice).collect();
        coder.encode(&refs, &mut out).unwrap();

        // Erase all data shards, keep all parity shards
        let mut shards: Vec<Option<Vec<u8>>> = vec![None, None, None, None];
        shards.extend(parity.into_iter().map(Some));

        coder.reconstruct(&mut shards).unwrap();
        for i in 0..4 {
            assert_eq!(shards[i].as_ref().unwrap(), &data[i]);
        }
    }

    #[test]
    #[ignore]
    fn benchmark_fec() {
        let (d, p, shard_len) = (64, 7, 1172);
        let coder = ReedSolomon::new(d, p).unwrap();
        let data: Vec<Vec<u8>> = (0..d).map(|_| vec![0xaa; shard_len]).collect();
        let refs: Vec<_> = data.iter().map(Vec::as_slice).collect();
        let mut parity = vec![vec![0; shard_len]; p];
        let mut out: Vec<_> = parity.iter_mut().map(Vec::as_mut_slice).collect();

        let iterations = 1000;
        let start = Instant::now();
        for _ in 0..iterations {
            coder.encode(&refs, &mut out).unwrap();
        }
        let encode_time = start.elapsed();
        let total_bytes = iterations as f64 * (d * shard_len) as f64;
        let mb_per_sec = (total_bytes / (1024.0 * 1024.0)) / encode_time.as_secs_f64();

        let all: Vec<_> = data.iter().chain(&parity).cloned().collect();
        let mut shards: Vec<Option<Vec<u8>>> = all.into_iter().map(Some).collect();
        for s in shards.iter_mut().take(p) {
            *s = None;
        }

        let mut single_shards = shards.clone();
        let start = Instant::now();
        coder.reconstruct(&mut single_shards).unwrap();
        let recon_single_time = start.elapsed();

        let recon_iterations = 1000;
        let start = Instant::now();
        for _ in 0..recon_iterations {
            let mut s = shards.clone();
            coder.reconstruct(&mut s).unwrap();
        }
        let recon_time = start.elapsed();
        let recon_total_bytes = recon_iterations as f64 * (d * shard_len) as f64;
        let recon_mb_per_sec = (recon_total_bytes / (1024.0 * 1024.0)) / recon_time.as_secs_f64();

        println!(
            "FEC 64+7 x 1172: encode {:.1} MB/s ({:?} for {} iter), reconstruct {:.1} MB/s ({:?} for 1 iter, {:?} for {} iter)",
            mb_per_sec, encode_time, iterations, recon_mb_per_sec, recon_single_time, recon_time, recon_iterations
        );
    }
}
