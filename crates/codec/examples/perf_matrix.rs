//! Encoder/decoder/colour timing matrix used for the codec performance work.
//!
//! `cargo run --release -p latch-codec --example perf_matrix -- [--sizes 720,1080]
//! [--threads 4] [--kinds still,scroll] [--frames 40] [--kbps 20000] [--fps 60]
//! [--color] [--no-codec]`
//!
//! Content is synthetic and deterministic, so the same command gives
//! comparable numbers on two builds. Encode and decode are timed
//! separately; conversion and content generation are not timed. Every
//! decoded picture is compared with the encoder's reconstruction.
#[path = "../tests/encode_content.rs"]
mod content;
use latch_codec::{
    color,
    h264::{Decoder, Encoder, EncoderConfig},
    Yuv420,
};
use std::{hint::black_box, time::Instant};

struct Stats(Vec<f64>);
impl Stats {
    fn line(mut self) -> String {
        if self.0.is_empty() {
            return "n=0".into();
        }
        self.0.sort_by(f64::total_cmp);
        let v = &self.0;
        format!(
            "n={} mean={:.3} p50={:.3} p95={:.3} max={:.3} ms",
            v.len(),
            v.iter().sum::<f64>() / v.len() as f64,
            v[v.len() / 2],
            v[(v.len() * 95 / 100).min(v.len() - 1)],
            v[v.len() - 1]
        )
    }
}

fn grid(w: u32, h: u32, t: u32) -> Vec<u8> {
    let mut b = vec![0; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let sx = (x + t * 4) % w;
            let v = if sx % 160 < 3 || y % 32 < 2 { 30 } else { 210 };
            b[i..i + 4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    b
}

/// Smooth shapes, mid-frequency texture and a little grain: photo-like.
fn photo(w: u32, h: u32, dx: i32, dy: i32) -> Yuv420 {
    let mut p = Yuv420::new(w, h);
    let hash = |x: i32, y: i32| {
        let mut v = (x as u32).wrapping_mul(0x9e3779b1) ^ (y as u32).wrapping_mul(0x85ebca77);
        v ^= v >> 15;
        v = v.wrapping_mul(0x2c1b3c6d);
        v ^= v >> 12;
        (v & 15) as f32 - 7.5
    };
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let (sx, sy) = (x + dx, y + dy);
            let (fx, fy) = (sx as f32, sy as f32);
            let v = 110.
                + 50. * (fx * 0.004 + fy * 0.003).sin()
                + 30. * (fx * 0.021).sin() * (fy * 0.017).cos()
                + 12. * ((fx + 2. * fy) * 0.11).sin()
                + 0.6 * hash(sx, sy);
            p.y[y as usize * p.y_stride + x as usize] = v.clamp(16., 235.) as u8;
        }
    }
    for y in 0..(h / 2) as i32 {
        for x in 0..(w / 2) as i32 {
            let (fx, fy) = ((2 * x + dx) as f32, (2 * y + dy) as f32);
            let i = y as usize * p.c_stride + x as usize;
            p.u[i] = (128. + 40. * (fx * 0.006).sin() + 8. * (fy * 0.05).cos()) as u8;
            p.v[i] = (128. + 35. * (fy * 0.005).cos() + 8. * (fx * 0.04).sin()) as u8;
        }
    }
    p.pad_edges();
    p
}

fn noise(base: &mut Yuv420, rng: &mut u32) {
    for p in &mut base.y {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 17;
        *rng ^= *rng << 5;
        *p = 16 + (*rng % 220) as u8;
    }
    base.pad_edges();
}

/// Frame `t` of `kind`, in the encoder's input format.
fn frame(kind: &str, w: u32, h: u32, t: u32, rng: &mut u32) -> Yuv420 {
    let mut y = Yuv420::new(w, h);
    match kind {
        "still" | "scroll" | "noise" => {
            let b = grid(w, h, if kind == "scroll" { t } else { 0 });
            color::bgra_to_yuv420(&b, w, h, w as usize * 4, &mut y);
            if kind == "noise" {
                noise(&mut y, rng);
            }
            y
        }
        "text" => content::frame(w, h, t as usize, "typing"),
        "vscroll" => content::frame(w, h, t as usize, "scroll"),
        "photo" => photo(w, h, 0, 0),
        "pan" => photo(w, h, 5 * t as i32, 3 * t as i32),
        "cut" => match (t / 8) % 3 {
            0 => content::frame(w, h, t as usize, "typing"),
            1 => photo(w, h, 37 * (t / 8) as i32, 0),
            _ => {
                let mut p = photo(w, h, 0, 0);
                noise(&mut p, rng);
                p
            }
        },
        _ => panic!("unknown kind {kind}"),
    }
}

fn arg(args: &[String], key: &str, default: &str) -> String {
    args.windows(2)
        .find(|a| a[0] == key)
        .map_or(default.to_owned(), |a| a[1].clone())
}

fn size(name: &str) -> (u32, u32) {
    match name {
        "720" => (1280, 720),
        "1080" => (1920, 1080),
        "1440" => (2560, 1440),
        "2160" => (3840, 2160),
        other => {
            let (w, h) = other.split_once('x').expect("size like 1920x1080");
            (w.parse().unwrap(), h.parse().unwrap())
        }
    }
}

fn color_bench(sizes: &[(u32, u32)]) {
    for &(w, h) in sizes {
        let b = grid(w, h, 0);
        let mut y = Yuv420::new(w, h);
        color::bgra_to_yuv420(&b, w, h, w as usize * 4, &mut y);
        let times = (0..80)
            .map(|_| {
                let t = Instant::now();
                color::bgra_to_yuv420(black_box(&b), w, h, w as usize * 4, black_box(&mut y));
                t.elapsed().as_secs_f64() * 1000.
            })
            .collect();
        println!("BGRA->YUV {w}x{h}: {}", Stats(times).line());
    }
}

fn psnr(mse: u64, samples: u64) -> f64 {
    10. * (65025. * samples as f64 / mse.max(1) as f64).log10()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sizes: Vec<_> = arg(&args, "--sizes", "720,1080,1440,2160")
        .split(',')
        .map(size)
        .collect();
    let threads: Vec<usize> = arg(&args, "--threads", "4")
        .split(',')
        .map(|t| t.parse().unwrap())
        .collect();
    let kinds = arg(
        &args,
        "--kinds",
        "still,scroll,noise,text,vscroll,photo,pan,cut",
    );
    let frames: u32 = arg(&args, "--frames", "40").parse().unwrap();
    let kbps: u32 = arg(&args, "--kbps", "20000").parse().unwrap();
    let fps: u32 = arg(&args, "--fps", "60").parse().unwrap();
    if args.iter().any(|a| a == "--color") {
        color_bench(&sizes);
    }
    if args.iter().any(|a| a == "--no-codec") {
        return;
    }
    for &(w, h) in &sizes {
        for &threads in &threads {
            for kind in kinds.split(',') {
                run(kind, w, h, threads, frames, kbps, fps);
            }
        }
    }
}

fn run(kind: &str, w: u32, h: u32, threads: usize, frames: u32, kbps: u32, fps: u32) {
    let label = format!("{w}x{h} {threads}t {kind}");
    let mut enc = Encoder::new(EncoderConfig {
        width: w,
        height: h,
        fps,
        bitrate_kbps: kbps,
        threads,
    })
    .unwrap();
    let mut dec = Decoder::new();
    let (mut et, mut dt, mut lens) = (vec![], vec![], vec![]);
    let (mut mse, mut samples, mut mismatched) = (0u64, 0u64, 0usize);
    let mut worst = (0., 0);
    let mut rng = 1u32;
    for t in 0..frames {
        let picture = frame(kind, w, h, t, &mut rng);
        let mut out = vec![];
        let now = Instant::now();
        let info = match enc.encode(&picture, false, &mut out) {
            Ok(i) => i,
            Err(e) => {
                println!("ENCODE_ERROR {label} frame={t}: {e}");
                return;
            }
        };
        let e = now.elapsed().as_secs_f64() * 1000.;
        let now = Instant::now();
        let decoded = dec.decode(&out).unwrap().unwrap();
        let d = now.elapsed().as_secs_f64() * 1000.;
        let r = enc.reconstruction();
        for (a, b, pw, ph, sa, sb) in [
            (
                &decoded.y,
                &r.y,
                w as usize,
                h as usize,
                decoded.y_stride,
                r.y_stride,
            ),
            (
                &decoded.u,
                &r.u,
                w as usize / 2,
                h as usize / 2,
                decoded.c_stride,
                r.c_stride,
            ),
            (
                &decoded.v,
                &r.v,
                w as usize / 2,
                h as usize / 2,
                decoded.c_stride,
                r.c_stride,
            ),
        ] {
            for row in 0..ph {
                mismatched += a[row * sa..row * sa + pw]
                    .iter()
                    .zip(&b[row * sb..row * sb + pw])
                    .filter(|(x, y)| x != y)
                    .count();
            }
        }
        for row in 0..h as usize {
            for x in 0..w as usize {
                let d =
                    picture.y[row * picture.y_stride + x] as i32 - r.y[row * r.y_stride + x] as i32;
                mse += (d * d) as u64;
            }
        }
        samples += w as u64 * h as u64;
        if t == 0 {
            println!(
                "IDR {label}: enc={e:.3} dec={d:.3} bytes={} qp={}",
                out.len(),
                info.qp
            );
        } else {
            if e > worst.0 {
                worst = (e, t);
            }
            et.push(e);
            dt.push(d);
            lens.push(out.len());
        }
    }
    println!("PSNR {label}: {:.3}", psnr(mse, samples));
    if mismatched > 0 {
        println!("RECON_MISMATCH {label}: visible_different={mismatched}");
    }
    let skip = (frames as usize / 6).min(et.len().saturating_sub(1));
    println!(
        "encode {label}: {} worst_frame={}",
        Stats(et[skip..].to_vec()).line(),
        worst.1
    );
    println!("decode {label}: {}", Stats(dt[skip..].to_vec()).line());
    println!(
        "bytes {label}: mean={} max={}",
        lens.iter().sum::<usize>() / lens.len().max(1),
        lens.iter().max().unwrap_or(&0)
    );
}
