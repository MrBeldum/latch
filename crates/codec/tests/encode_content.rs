#![allow(dead_code)]
use latch_codec::Yuv420;

pub fn frame(width: u32, height: u32, index: usize, kind: &str) -> Yuv420 {
    let mut p = Yuv420::new(width, height);
    let (w, h) = (width as usize, height as usize);
    let mut rng = 0x13579bdu32.wrapping_add(index as u32 * 977);
    for y in 0..h {
        for x in 0..w {
            p.y[y * p.y_stride + x] = match kind {
                "noise" => {
                    rng ^= rng << 13;
                    rng ^= rng >> 17;
                    rng ^= rng << 5;
                    16 + (rng % 220) as u8
                }
                "gradient" => 16 + ((x * 219 / w + y * 53 / h) % 220) as u8,
                "motion" => {
                    16 + (((x + index * 7) / 3
                        + (y + index * 3) / 2
                        + ((x + index * 17) ^ (y + index * 11)) % 47)
                        % 220) as u8
                }
                _ => {
                    if y < 24 {
                        48
                    } else {
                        28
                    }
                }
            };
        }
    }
    if !matches!(kind, "noise" | "gradient" | "motion") {
        let scroll = if kind == "scroll" { index * 64 } else { 0 };
        for row in 0..h / 12 + 1 {
            let cy = row * 12;
            let line = row + scroll / 12;
            let text = format!("LATCH 5  RUST DESKTOP  {line:05}  TEXT 0123456789");
            for (i, ch) in text.bytes().enumerate() {
                draw_shifted(&mut p, 12 + i * 7, cy, scroll % 12, ch, 220);
            }
        }
        if kind == "typing" {
            for i in 0..index % 80 {
                draw(
                    &mut p,
                    12 + i * 7,
                    (h / 2 / 12) * 12,
                    b'A' + (i % 26) as u8,
                    160,
                );
            }
        }
        if kind == "windows" {
            let left = (index * 19) % (w.saturating_sub(100).max(1));
            for y in h / 3..(h / 3 + 80).min(h) {
                for x in left..(left + 100).min(w) {
                    p.y[y * p.y_stride + x] = 150;
                }
            }
        }
    }
    if kind == "video" {
        for y in h / 2..h {
            for x in w / 2..w {
                p.y[y * p.y_stride + x] = 16
                    + (((x + index * 7) / 3
                        + (y + index * 3) / 2
                        + ((x + index * 17) ^ (y + index * 11)) % 47)
                        % 220) as u8;
            }
        }
    }
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let i = y * p.c_stride + x;
            if kind == "noise" {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                p.u[i] = 16 + (rng % 225) as u8;
                p.v[i] = 16 + ((rng >> 8) % 225) as u8;
            } else if matches!(kind, "motion" | "gradient") {
                p.u[i] = 80 + ((x / 7 + index * 2) % 90) as u8;
                p.v[i] = 80 + ((y / 5 + index) % 90) as u8;
            }
        }
    }
    p.pad_edges();
    p
}
fn draw(p: &mut Yuv420, x: usize, y: usize, c: u8, value: u8) {
    draw_shifted(p, x, y, 0, c, value);
}
fn draw_shifted(p: &mut Yuv420, x: usize, y: usize, shift: usize, c: u8, value: u8) {
    const FONT: [[u8; 7]; 36] = [
        [14, 17, 19, 21, 25, 17, 14],
        [4, 12, 4, 4, 4, 4, 14],
        [14, 17, 1, 2, 4, 8, 31],
        [30, 1, 1, 14, 1, 1, 30],
        [2, 6, 10, 18, 31, 2, 2],
        [31, 16, 16, 30, 1, 1, 30],
        [14, 16, 16, 30, 17, 17, 14],
        [31, 1, 2, 4, 8, 8, 8],
        [14, 17, 17, 14, 17, 17, 14],
        [14, 17, 17, 15, 1, 1, 14],
        [14, 17, 17, 31, 17, 17, 17],
        [30, 17, 17, 30, 17, 17, 30],
        [14, 17, 16, 16, 16, 17, 14],
        [30, 17, 17, 17, 17, 17, 30],
        [31, 16, 16, 30, 16, 16, 31],
        [31, 16, 16, 30, 16, 16, 16],
        [14, 17, 16, 23, 17, 17, 15],
        [17, 17, 17, 31, 17, 17, 17],
        [14, 4, 4, 4, 4, 4, 14],
        [7, 2, 2, 2, 18, 18, 12],
        [17, 18, 20, 24, 20, 18, 17],
        [16, 16, 16, 16, 16, 16, 31],
        [17, 27, 21, 21, 17, 17, 17],
        [17, 25, 21, 19, 17, 17, 17],
        [14, 17, 17, 17, 17, 17, 14],
        [30, 17, 17, 30, 16, 16, 16],
        [14, 17, 17, 17, 21, 18, 13],
        [30, 17, 17, 30, 20, 18, 17],
        [15, 16, 16, 14, 1, 1, 30],
        [31, 4, 4, 4, 4, 4, 4],
        [17, 17, 17, 17, 17, 17, 14],
        [17, 17, 17, 17, 17, 10, 4],
        [17, 17, 17, 21, 21, 21, 10],
        [17, 17, 10, 4, 10, 17, 17],
        [17, 17, 10, 4, 4, 4, 4],
        [31, 1, 2, 4, 8, 16, 31],
    ];
    let glyph = match c {
        b'0'..=b'9' => FONT[(c - b'0') as usize],
        b'A'..=b'Z' => FONT[(c - b'A' + 10) as usize],
        _ => [0; 7],
    };
    for (yy, row) in glyph.into_iter().enumerate() {
        for xx in 0..5 {
            if x + xx < p.width as usize
                && y + yy >= shift
                && y + yy - shift < p.height as usize
                && row & (1 << (4 - xx)) != 0
            {
                p.y[(y + yy - shift) * p.y_stride + x + xx] = value;
            }
        }
    }
}
pub fn raw(p: &Yuv420) -> Vec<u8> {
    let mut out = Vec::with_capacity((p.width * p.height * 3 / 2) as usize);
    for (plane, stride, n) in [
        (&p.y, p.y_stride, 1),
        (&p.u, p.c_stride, 2),
        (&p.v, p.c_stride, 2),
    ] {
        for y in 0..p.height as usize / n {
            out.extend_from_slice(&plane[y * stride..y * stride + p.width as usize / n]);
        }
    }
    out
}
