//! Benchmark for BGRA -> YUV 4:2:0 conversion.

use latch_codec::color::bgra_to_yuv420;
use latch_codec::yuv::Yuv420;
use std::time::Instant;

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
        bgra_to_yuv420(&src, width, height, stride, &mut dst);
    }

    let start = Instant::now();
    for _ in 0..iterations {
        bgra_to_yuv420(&src, width, height, stride, &mut dst);
    }
    let elapsed = start.elapsed();

    elapsed.as_secs_f64() * 1000.0 / (iterations as f64)
}

fn main() {
    println!("=== Latch Codec Color Conversion Benchmark ===");

    let ms_1080p = bench_resolution(1920, 1080, 100);
    println!("1080p (1920x1080): {ms_1080p:.3} ms / frame");
    assert!(
        ms_1080p <= 1.5,
        "Performance regression: 1080p conversion took {ms_1080p:.3} ms (> 1.5 ms)"
    );

    let ms_4k = bench_resolution(3840, 2160, 50);
    println!("4K (3840x2160):    {ms_4k:.3} ms / frame");

    println!("All performance targets met!");
}
