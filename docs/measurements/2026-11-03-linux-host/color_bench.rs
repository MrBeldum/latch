// color_bench.rs: bgra_to_yuv420_rects timing, cross-built against two latch-codec trees (a scratch crate depending on crates/codec).
use latch_codec::color::{bgra_to_yuv420_rects, Rect};
use latch_codec::Yuv420;
use std::time::Instant;
fn main() {
    for (w, h, rw, rh) in [(1920u32, 1080u32, 1920u32, 1080u32), (1920, 1080, 960, 540), (2560, 1440, 2560, 1440), (3840, 2160, 3840, 2160)] {
        let stride = w as usize * 4;
        let src: Vec<u8> = (0..stride * h as usize).map(|i| (i * 31 % 251) as u8).collect();
        let rect = [Rect { x: 0, y: 0, width: rw, height: rh }];
        let mut dst = Yuv420::new(w, h);
        let mut best = f64::MAX;
        for _ in 0..5 {
            let reps = 100;
            let t = Instant::now();
            for _ in 0..reps { bgra_to_yuv420_rects(&src, w, h, stride, &rect, 4, &mut dst); }
            best = best.min(t.elapsed().as_secs_f64() * 1e6 / reps as f64);
        }
        println!("{w}x{h} rect {rw}x{rh}, 4 threads: {best:.0} us (best of 5x100)");
    }
}
