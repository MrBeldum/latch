pub(super) const SCAN: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const MF: [[i32; 3]; 6] = [
    [13107, 8066, 5243],
    [11916, 7490, 4660],
    [10082, 6554, 4194],
    [9362, 5825, 3647],
    [8192, 5243, 3355],
    [7282, 4559, 2893],
];
const V: [[i32; 3]; 6] = [
    [10, 13, 16],
    [11, 14, 18],
    [13, 16, 20],
    [14, 18, 23],
    [16, 20, 25],
    [18, 23, 29],
];
pub(super) fn chroma_qp(qp: u8) -> u8 {
    const MAP: [u8; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];
    if qp < 30 {
        qp
    } else {
        MAP[(qp - 30) as usize]
    }
}
fn class(i: usize) -> usize {
    (i & 1) + ((i >> 2) & 1)
}
pub(super) fn forward(input: [i32; 16]) -> [i32; 16] {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { neon::forward(input) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        forward_scalar(input)
    }
}
#[cfg_attr(not(test), allow(dead_code))]
fn forward_scalar(input: [i32; 16]) -> [i32; 16] {
    let mut tmp = [0; 16];
    let mut out = [0; 16];
    for y in 0..4 {
        let a = input[y * 4] + input[y * 4 + 3];
        let b = input[y * 4 + 1] + input[y * 4 + 2];
        let c = input[y * 4 + 1] - input[y * 4 + 2];
        let d = input[y * 4] - input[y * 4 + 3];
        tmp[y * 4] = a + b;
        tmp[y * 4 + 1] = 2 * d + c;
        tmp[y * 4 + 2] = a - b;
        tmp[y * 4 + 3] = d - 2 * c;
    }
    for x in 0..4 {
        let a = tmp[x] + tmp[12 + x];
        let b = tmp[4 + x] + tmp[8 + x];
        let c = tmp[4 + x] - tmp[8 + x];
        let d = tmp[x] - tmp[12 + x];
        out[x] = a + b;
        out[4 + x] = 2 * d + c;
        out[8 + x] = a - b;
        out[12 + x] = d - 2 * c;
    }
    out
}
pub(super) fn quant(value: i32, qp: u8, index: usize, intra: bool, extra: u32) -> i32 {
    let shift = 15 + qp as u32 / 6 + extra;
    let bias = if intra {
        1 << (shift - 1)
    } else {
        (1 << shift) / 6
    };
    value.signum() * ((value.abs() * MF[(qp % 6) as usize][class(index)] + bias) >> shift)
}
pub(super) fn dequant(value: i32, qp: u8, index: usize) -> i32 {
    value * V[(qp % 6) as usize][class(index)] * (1 << (qp / 6))
}
pub(super) struct Quantizer {
    mf: [[i32; 4]; 2],
    scales: [[i32; 4]; 2],
    shift: i32,
    bias: i32,
    cutoff: i32,
}
impl Quantizer {
    pub(super) fn new(qp: u8, intra: bool, suppression: u8) -> Self {
        let shift = 15 + qp as i32 / 6;
        Self {
            mf: std::array::from_fn(|r| {
                std::array::from_fn(|x| MF[(qp % 6) as usize][class(r * 4 + x)])
            }),
            scales: std::array::from_fn(|r| {
                std::array::from_fn(|x| V[(qp % 6) as usize][class(r * 4 + x)] * (1 << (qp / 6)))
            }),
            shift,
            bias: if intra {
                1 << (shift - 1)
            } else {
                (1 << shift) / 6
            },
            cutoff: 1 << suppression,
        }
    }
    pub(super) fn apply(&self, input: [i32; 16]) -> ([i32; 16], [i32; 16]) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            use std::arch::aarch64::*;
            let mut q = [0; 16];
            let mut dq = [0; 16];
            for row in 0..4 {
                let i = row * 4;
                let value = vld1q_s32(input.as_ptr().add(i));
                let magnitude = vshlq_s32(
                    vaddq_s32(
                        vmulq_s32(vabsq_s32(value), vld1q_s32(self.mf[row & 1].as_ptr())),
                        vdupq_n_s32(self.bias),
                    ),
                    vdupq_n_s32(-self.shift),
                );
                let magnitude = vreinterpretq_s32_u32(vandq_u32(
                    vreinterpretq_u32_s32(magnitude),
                    vcgeq_s32(magnitude, vdupq_n_s32(self.cutoff)),
                ));
                let signed = vbslq_s32(
                    vcltq_s32(value, vdupq_n_s32(0)),
                    vnegq_s32(magnitude),
                    magnitude,
                );
                vst1q_s32(q.as_mut_ptr().add(i), signed);
                vst1q_s32(
                    dq.as_mut_ptr().add(i),
                    vmulq_s32(signed, vld1q_s32(self.scales[row & 1].as_ptr())),
                );
            }
            (q, dq)
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            let q = std::array::from_fn(|i| {
                let magnitude =
                    (input[i].abs() * self.mf[(i / 4) & 1][i % 4] + self.bias) >> self.shift;
                if magnitude < self.cutoff {
                    0
                } else {
                    input[i].signum() * magnitude
                }
            });
            let dq = std::array::from_fn(|i| q[i] * self.scales[(i / 4) & 1][i % 4]);
            (q, dq)
        }
    }
}
#[cfg_attr(not(test), allow(dead_code))]
fn quantize_scalar(
    input: [i32; 16],
    qp: u8,
    intra: bool,
    suppression: u8,
) -> ([i32; 16], [i32; 16]) {
    let q = std::array::from_fn(|i| suppress(quant(input[i], qp, i, intra, 0), suppression));
    let dq = std::array::from_fn(|i| dequant(q[i], qp, i));
    (q, dq)
}
pub(super) fn inverse(input: [i32; 16]) -> [i32; 16] {
    if input[1..].iter().all(|&v| v == 0) {
        return [(input[0] + 32) >> 6; 16];
    }
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { neon::inverse(input) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        inverse_scalar(input)
    }
}
#[cfg_attr(not(test), allow(dead_code))]
fn inverse_scalar(input: [i32; 16]) -> [i32; 16] {
    let mut tmp = [0; 16];
    let mut out = [0; 16];
    for y in 0..4 {
        let a = input[y * 4] + input[y * 4 + 2];
        let b = input[y * 4] - input[y * 4 + 2];
        let c = (input[y * 4 + 1] >> 1) - input[y * 4 + 3];
        let d = input[y * 4 + 1] + (input[y * 4 + 3] >> 1);
        tmp[y * 4] = a + d;
        tmp[y * 4 + 1] = b + c;
        tmp[y * 4 + 2] = b - c;
        tmp[y * 4 + 3] = a - d;
    }
    for x in 0..4 {
        let a = tmp[x] + tmp[8 + x];
        let b = tmp[x] - tmp[8 + x];
        let c = (tmp[4 + x] >> 1) - tmp[12 + x];
        let d = tmp[4 + x] + (tmp[12 + x] >> 1);
        out[x] = (a + d + 32) >> 6;
        out[4 + x] = (b + c + 32) >> 6;
        out[8 + x] = (b - c + 32) >> 6;
        out[12 + x] = (a - d + 32) >> 6;
    }
    out
}
pub(super) fn block(
    src: &[u8],
    stride: usize,
    offset: usize,
    pred: &[u8; 16],
    quantizer: &Quantizer,
) -> ([i32; 16], [u8; 16]) {
    let mut input = [0; 16];
    for y in 0..4 {
        for x in 0..4 {
            input[y * 4 + x] = src[offset + y * stride + x] as i32 - pred[y * 4 + x] as i32;
        }
    }
    if input == [0; 16] {
        return ([0; 16], *pred);
    }
    let transformed = forward(input);
    let (q, dq) = quantizer.apply(transformed);
    if q == [0; 16] {
        return (q, *pred);
    }
    let residual = inverse(dq);
    let mut rec = [0; 16];
    let mut scan = [0; 16];
    for i in 0..16 {
        rec[i] = (pred[i] as i32 + residual[i]).clamp(0, 255) as u8;
        scan[i] = q[SCAN[i]];
    }
    (scan, rec)
}
pub(super) fn satd(a: &[u8], stride: usize, offset: usize, pred: &[u8; 16]) -> u32 {
    let mut v = [0i32; 16];
    for y in 0..4 {
        for x in 0..4 {
            v[4 * y + x] = a[offset + y * stride + x] as i32 - pred[4 * y + x] as i32;
        }
    }
    hadamard(v)
        .iter()
        .map(|x| x.unsigned_abs())
        .sum::<u32>()
        .div_ceil(2)
}
pub(super) fn hadamard(v: [i32; 16]) -> [i32; 16] {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { neon::hadamard(v) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        hadamard_scalar(v)
    }
}
#[cfg_attr(not(test), allow(dead_code))]
fn hadamard_scalar(v: [i32; 16]) -> [i32; 16] {
    let mut t = [0; 16];
    let mut o = [0; 16];
    for y in 0..4 {
        let i = 4 * y;
        let a = v[i] + v[i + 3];
        let b = v[i + 1] + v[i + 2];
        let c = v[i + 1] - v[i + 2];
        let d = v[i] - v[i + 3];
        t[i] = a + b;
        t[i + 1] = d + c;
        t[i + 2] = a - b;
        t[i + 3] = d - c;
    }
    for x in 0..4 {
        let a = t[x] + t[x + 12];
        let b = t[x + 4] + t[x + 8];
        let c = t[x + 4] - t[x + 8];
        let d = t[x] - t[x + 12];
        o[x] = a + b;
        o[x + 4] = d + c;
        o[x + 8] = a - b;
        o[x + 12] = d - c;
    }
    o
}
pub(super) fn chroma_dc(v: [i32; 4]) -> [i32; 4] {
    [
        v[0] + v[1] + v[2] + v[3],
        v[0] - v[1] + v[2] - v[3],
        v[0] + v[1] - v[2] - v[3],
        v[0] - v[1] - v[2] + v[3],
    ]
}
pub(super) fn chroma_dc_inverse(v: [i32; 4], qp: u8) -> [i32; 4] {
    chroma_dc(v).map(|x| (x * V[(qp % 6) as usize][0] * (1 << (qp / 6))) >> 1)
}

pub(super) fn suppress(value: i32, shift: u8) -> i32 {
    if value.unsigned_abs() < (1u32 << shift) {
        0
    } else {
        value
    }
}
pub(super) fn luma_dc_inverse(v: [i32; 16], qp: u8) -> [i32; 16] {
    hadamard(v).map(|x| {
        let value = x * V[(qp % 6) as usize][0];
        if qp >= 12 {
            value * (1 << (qp / 6 - 2))
        } else {
            (value + (1 << (1 - qp / 6))) >> (2 - qp / 6)
        }
    })
}

// AArch64 guarantees NEON. All arithmetic stays in i32, including high-QP inverse coefficients.
#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;
    #[inline]
    unsafe fn transpose(v: [int32x4_t; 4]) -> [int32x4_t; 4] {
        let a = vtrn1q_s32(v[0], v[1]);
        let b = vtrn2q_s32(v[0], v[1]);
        let c = vtrn1q_s32(v[2], v[3]);
        let d = vtrn2q_s32(v[2], v[3]);
        [
            vcombine_s32(vget_low_s32(a), vget_low_s32(c)),
            vcombine_s32(vget_low_s32(b), vget_low_s32(d)),
            vcombine_s32(vget_high_s32(a), vget_high_s32(c)),
            vcombine_s32(vget_high_s32(b), vget_high_s32(d)),
        ]
    }
    #[inline]
    unsafe fn forward_pass(v: [int32x4_t; 4]) -> [int32x4_t; 4] {
        let a = vaddq_s32(v[0], v[3]);
        let b = vaddq_s32(v[1], v[2]);
        let c = vsubq_s32(v[1], v[2]);
        let d = vsubq_s32(v[0], v[3]);
        [
            vaddq_s32(a, b),
            vaddq_s32(vshlq_n_s32::<1>(d), c),
            vsubq_s32(a, b),
            vsubq_s32(d, vshlq_n_s32::<1>(c)),
        ]
    }
    #[inline]
    unsafe fn inverse_pass(v: [int32x4_t; 4]) -> [int32x4_t; 4] {
        let a = vaddq_s32(v[0], v[2]);
        let b = vsubq_s32(v[0], v[2]);
        let c = vsubq_s32(vshrq_n_s32::<1>(v[1]), v[3]);
        let d = vaddq_s32(v[1], vshrq_n_s32::<1>(v[3]));
        [
            vaddq_s32(a, d),
            vaddq_s32(b, c),
            vsubq_s32(b, c),
            vsubq_s32(a, d),
        ]
    }
    #[inline]
    unsafe fn hadamard_pass(v: [int32x4_t; 4]) -> [int32x4_t; 4] {
        let a = vaddq_s32(v[0], v[3]);
        let b = vaddq_s32(v[1], v[2]);
        let c = vsubq_s32(v[1], v[2]);
        let d = vsubq_s32(v[0], v[3]);
        [
            vaddq_s32(a, b),
            vaddq_s32(d, c),
            vsubq_s32(a, b),
            vsubq_s32(d, c),
        ]
    }
    pub(super) unsafe fn hadamard(input: [i32; 16]) -> [i32; 16] {
        let rows = std::array::from_fn(|i| vld1q_s32(input.as_ptr().add(i * 4)));
        let rows = hadamard_pass(transpose(hadamard_pass(transpose(rows))));
        let mut out = [0; 16];
        for (i, row) in rows.into_iter().enumerate() {
            vst1q_s32(out.as_mut_ptr().add(i * 4), row);
        }
        out
    }
    pub(super) unsafe fn forward(input: [i32; 16]) -> [i32; 16] {
        let rows = std::array::from_fn(|i| vld1q_s32(input.as_ptr().add(i * 4)));
        let rows = forward_pass(transpose(forward_pass(transpose(rows))));
        let mut out = [0; 16];
        for (i, row) in rows.into_iter().enumerate() {
            vst1q_s32(out.as_mut_ptr().add(i * 4), row);
        }
        out
    }
    pub(super) unsafe fn inverse(input: [i32; 16]) -> [i32; 16] {
        let rows = std::array::from_fn(|i| vld1q_s32(input.as_ptr().add(i * 4)));
        let rows = inverse_pass(transpose(inverse_pass(transpose(rows))));
        let mut out = [0; 16];
        for (i, row) in rows.into_iter().enumerate() {
            vst1q_s32(
                out.as_mut_ptr().add(i * 4),
                vshrq_n_s32::<6>(vaddq_s32(row, vdupq_n_s32(32))),
            );
        }
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quantization_matches_scalar() {
        let mut seed = 33u32;
        for qp in 0..=51 {
            for intra in [false, true] {
                for suppression in 0..=10 {
                    let input = std::array::from_fn(|_| {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        (seed >> 16) as i32 % 16321 - 8160
                    });
                    assert_eq!(
                        Quantizer::new(qp, intra, suppression).apply(input),
                        quantize_scalar(input, qp, intra, suppression)
                    );
                }
            }
        }
    }
    #[test]
    fn transforms_match_scalar() {
        let mut seed = 7u32;
        for _ in 0..10000 {
            let residual = std::array::from_fn(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 16) as i32 % 511 - 255
            });
            assert_eq!(forward(residual), forward_scalar(residual));
            assert_eq!(hadamard(residual), hadamard_scalar(residual));
            let coefficients = residual.map(|v| v * 4096);
            assert_eq!(inverse(coefficients), inverse_scalar(coefficients));
        }
        for dc in [-32768, -33, -32, -1, 0, 1, 31, 32, 32768] {
            let mut c = [0; 16];
            c[0] = dc;
            assert_eq!(inverse(c), inverse_scalar(c));
        }
    }
}
