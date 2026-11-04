//! A digest of the encoder's output and reconstruction for fixed content
//! and a deterministic encoder (frame budget 0), so a change meant to be a
//! pure speed-up can be shown to leave every bit alone: run it on both
//! builds and compare the lines.
//!
//!     encode_digest [--sizes 640x368,1920x1080] [--frames 24] [--threads 4]
#[path = "../tests/encode_content.rs"]
mod content;
use latch_codec::h264::{Encoder, EncoderConfig};

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash = (*hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |key: &str, default: &str| {
        args.windows(2)
            .find(|a| a[0] == key)
            .map_or(default.to_owned(), |a| a[1].clone())
    };
    let frames: usize = arg("--frames", "24").parse().unwrap();
    let threads: usize = arg("--threads", "4").parse().unwrap();
    for size in arg("--sizes", "640x368,1920x1080").split(',') {
        let (w, h) = size.split_once('x').expect("size like 1920x1080");
        let (w, h): (u32, u32) = (w.parse().unwrap(), h.parse().unwrap());
        for kind in ["typing", "scroll", "windows", "motion", "noise", "gradient"] {
            for kbps in [2000, 20000] {
                let mut encoder = Encoder::new(
                    EncoderConfig {
                        width: w,
                        height: h,
                        fps: 60,
                        bitrate_kbps: kbps,
                        threads,
                    }
                    .with_frame_budget(0),
                )
                .unwrap();
                let (mut bits, mut rec) = (0xcbf29ce484222325u64, 0xcbf29ce484222325u64);
                let mut bytes = 0;
                for i in 0..frames {
                    let picture = content::frame(w, h, i, kind);
                    let mut out = Vec::new();
                    match encoder.encode(&picture, i == frames / 2, &mut out) {
                        Ok(_) => {}
                        Err(e) => {
                            fnv(&mut bits, e.to_string().as_bytes());
                            continue;
                        }
                    }
                    bytes += out.len();
                    fnv(&mut bits, &out);
                    let r = encoder.reconstruction();
                    fnv(&mut rec, &r.y);
                    fnv(&mut rec, &r.u);
                    fnv(&mut rec, &r.v);
                }
                println!(
                    "{w}x{h} {kind:8} {kbps:5} kbps: bytes={bytes} bits={bits:016x} rec={rec:016x}"
                );
            }
        }
    }
}
