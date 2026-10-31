//! Benchmark for BGRA -> YUV 4:2:0 conversion.

use latch_codec::color::bgra_to_yuv420;
use latch_codec::yuv::Yuv420;
use std::{hint::black_box, time::Instant};

fn bench_resolution(width: u32, height: u32, iterations: usize) -> f64 {
    let stride = (width as usize) * 4;
    let mut src = vec![0u8; stride * (height as usize)];

    // Populate with gradient
    for y in 0..height as usize {
        for x in 0..width as usize {
            let offset = y * stride + x * 4;
            src[offset] = (x & 0xff) as u8;
            src[offset + 1] = (y & 0xff) as u8;
            src[offset + 2] = ((x + y) & 0xff) as u8;
            src[offset + 3] = 255;
        }
    }

    let mut dst = Yuv420::new(width, height);

    // Warm-up
    for _ in 0..5 {
        bgra_to_yuv420(black_box(&src), width, height, stride, black_box(&mut dst));
    }

    let start = Instant::now();
    for _ in 0..iterations {
        bgra_to_yuv420(black_box(&src), width, height, stride, black_box(&mut dst));
    }
    let elapsed = start.elapsed();

    elapsed.as_secs_f64() * 1000.0 / (iterations as f64)
}

fn main() {
    println!("width,height,iterations,ms_frame");
    for (w, h) in [(1280, 720), (1920, 1080), (3840, 2160)] {
        let ms = bench_resolution(w, h, 50);
        println!("{w},{h},50,{ms:.3}");
    }
}
