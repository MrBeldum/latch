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

const fn tables() -> ([u8; 512], [u8; 256], [[u8; 256]; 256]) {
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
    (exp, log, mul)
}

static TABLES: ([u8; 512], [u8; 256], [[u8; 256]; 256]) = tables();

fn inv(x: u8) -> u8 {
    TABLES.0[255 - TABLES.1[x as usize] as usize]
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
    #[cfg(not(target_arch = "aarch64"))]
    {
        if c == 1 {
            for (d, &s) in dst.iter_mut().zip(src) {
                *d ^= s;
            }
            return;
        }
        let row = &TABLES.2[c as usize];
        for (d, &s) in dst.iter_mut().zip(src) {
            *d ^= row[s as usize];
        }
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

    let row = &TABLES.2[c as usize];
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
            for (j, shard) in data.iter().enumerate() {
                addmul(out, shard, inv((self.data + i) as u8 ^ j as u8));
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
        let selected: Vec<usize> = shards
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|_| i))
            .take(self.data)
            .collect();
        if selected.len() < self.data {
            return Err(FecError::TooFewShards);
        }
        if shards[..self.data].iter().all(Option::is_some) {
            return Ok(());
        }
        let d = self.data;
        let mut matrix = vec![vec![0u8; d * 2]; d];
        for (r, &idx) in selected.iter().enumerate() {
            if idx < d {
                matrix[r][idx] = 1;
            } else {
                for (j, value) in matrix[r][..d].iter_mut().enumerate() {
                    *value = inv(idx as u8 ^ j as u8);
                }
            }
            matrix[r][d + r] = 1;
        }
        for col in 0..d {
            let pivot = (col..d)
                .find(|&r| matrix[r][col] != 0)
                .ok_or(FecError::Shape)?;
            matrix.swap(col, pivot);
            let scale = inv(matrix[col][col]);
            for v in &mut matrix[col] {
                *v = TABLES.2[scale as usize][*v as usize];
            }
            let row = matrix[col].clone();
            for (r, other) in matrix.iter_mut().enumerate() {
                if r != col {
                    let c = other[col];
                    if c != 0 {
                        addmul(other, &row, c);
                    }
                }
            }
        }
        for target in 0..d {
            if shards[target].is_none() {
                let mut result = vec![0; len.unwrap_or(0)];
                for (r, &idx) in selected.iter().enumerate() {
                    if let Some(src) = &shards[idx] {
                        let c = matrix[target][d + r];
                        if c != 0 {
                            addmul(&mut result, src, c);
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

        let start = Instant::now();
        coder.reconstruct(&mut shards).unwrap();
        let recon_time = start.elapsed();

        println!(
            "FEC 64+7 x 1172: encode {:.1} MB/s ({:?} for {} iter), reconstruct {:?}",
            mb_per_sec, encode_time, iterations, recon_time
        );
    }
}
