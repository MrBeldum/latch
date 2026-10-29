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
    let bias = (1 << shift) / if intra { 3 } else { 6 };
    value.signum() * ((value.abs() * MF[(qp % 6) as usize][class(index)] + bias) >> shift)
}
pub(super) fn dequant(value: i32, qp: u8, index: usize) -> i32 {
    value * V[(qp % 6) as usize][class(index)] * (1 << (qp / 6))
}
pub(super) fn inverse(input: [i32; 16]) -> [i32; 16] {
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
    qp: u8,
    intra: bool,
    suppression: u8,
) -> ([i32; 16], [u8; 16]) {
    let mut input = [0; 16];
    for y in 0..4 {
        for x in 0..4 {
            input[y * 4 + x] = src[offset + y * stride + x] as i32 - pred[y * 4 + x] as i32;
        }
    }
    let transformed = forward(input);
    let mut q = [0; 16];
    let mut dq = [0; 16];
    for i in 0..16 {
        q[i] = suppress(quant(transformed[i], qp, i, intra, 0), suppression);
        dq[i] = dequant(q[i], qp, i);
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
