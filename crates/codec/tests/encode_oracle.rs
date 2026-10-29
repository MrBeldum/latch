//! Set LATCH_CODEC_DECODER_ORACLE=1 to also check each access unit with the Rust decoder.
mod encode_content;
use latch_codec::h264::{Encoder, EncoderConfig};
use std::{
    fs,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn ffmpeg() -> Option<String> {
    for path in ["/opt/homebrew/bin/ffmpeg", "ffmpeg"] {
        if Command::new(path)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Some(path.to_owned());
        }
    }
    eprintln!("SKIP: ffmpeg is unavailable");
    None
}
fn compare(
    width: u32,
    height: u32,
    threads: usize,
    kbps: u32,
    kind: &str,
    frames: usize,
    hardware: bool,
) {
    let Some(ffmpeg) = ffmpeg() else { return };
    let mut e = Encoder::new(EncoderConfig {
        width,
        height,
        fps: 30,
        bitrate_kbps: kbps,
        threads,
    })
    .unwrap();
    e.set_subpixel_refinement(threads == 3);
    let mut native =
        std::env::var_os("LATCH_CODEC_DECODER_ORACLE").map(|_| latch_codec::h264::Decoder::new());
    let mut bitstream = Vec::new();
    let mut expected = Vec::new();
    for i in 0..frames {
        let p = encode_content::frame(width, height, i, kind);
        let start = bitstream.len();
        e.encode(&p, i == frames / 2, &mut bitstream)
            .unwrap_or_else(|err| {
                panic!("{kind} {width}x{height} threads={threads} kbps={kbps} frame={i}: {err}")
            });
        let rec = encode_content::raw(e.reconstruction());
        if let Some(native) = &mut native {
            let decoded = native
                .decode(&bitstream[start..])
                .expect("Rust decoder")
                .expect("whole access unit");
            assert_frames(
                &encode_content::raw(decoded),
                &rec,
                width,
                height,
                "Rust decoder",
            );
        }
        expected.extend(rec);
    }
    let path = std::env::temp_dir().join(format!(
        "latch-encode-{}-{}.h264",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, &bitstream).unwrap();
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-v", "error"]);
    if hardware {
        cmd.args([
            "-hwaccel",
            "videotoolbox",
            "-hwaccel_output_format",
            "videotoolbox_vld",
        ]);
    }
    cmd.args(["-f", "h264", "-i"]).arg(&path);
    if hardware {
        cmd.args(["-vf", "hwdownload,format=nv12,format=yuv420p"]);
    }
    let output = cmd
        .args([
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-",
        ])
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_frames(
        &output.stdout,
        &expected,
        width,
        height,
        &format!("{kind} threads={threads} kbps={kbps}"),
    );
}
fn assert_frames(actual: &[u8], expected: &[u8], width: u32, height: u32, label: &str) {
    assert_eq!(actual.len(), expected.len(), "{label}: decoded frame count");
    if let Some(i) = actual.iter().zip(expected).position(|(a, b)| a != b) {
        let size = (width * height * 3 / 2) as usize;
        panic!(
            "{label} {width}x{height} frame={} byte={} decoder={} encoder={}",
            i / size,
            i % size,
            actual[i],
            expected[i]
        );
    }
}
#[test]
fn software_oracle_small() {
    for kind in [
        "still", "typing", "scroll", "windows", "gradient", "noise", "motion", "video",
    ] {
        compare(96, 64, 1, 2000, kind, 6, false);
        compare(82, 50, 4, 2000, kind, 5, false);
    }
}
#[test]
#[ignore = "full resolution FFmpeg matrix; run with --ignored"]
fn software_oracle_desktop_matrix() {
    for (w, h) in [(1366, 768), (1920, 1080), (640, 360), (2560, 1600)] {
        for threads in [1, 4] {
            for kbps in [8000, 20000] {
                for kind in [
                    "still", "typing", "scroll", "windows", "gradient", "noise", "motion", "video",
                ] {
                    compare(w, h, threads, kbps, kind, 6, false);
                }
            }
        }
    }
}
#[test]
#[ignore = "requires macOS VideoToolbox; run explicitly"]
fn videotoolbox_oracle() {
    if !cfg!(target_os = "macos") {
        eprintln!("SKIP: VideoToolbox requires macOS");
        return;
    }
    for (w, h) in [(1366, 768), (1920, 1080), (640, 360), (2560, 1600)] {
        for threads in [1, 4] {
            for kind in [
                "still", "typing", "scroll", "windows", "gradient", "noise", "motion", "video",
            ] {
                compare(w, h, threads, 20000, kind, 4, true);
            }
        }
    }
}
#[test]
fn validation_and_append() {
    let c = EncoderConfig {
        width: 18,
        height: 18,
        fps: 30,
        bitrate_kbps: 2000,
        threads: 1,
    };
    assert!(Encoder::new(EncoderConfig {
        width: 17,
        ..c.clone()
    })
    .is_err());
    let mut e = Encoder::new(c).unwrap();
    let mut out = vec![42];
    let mut p = encode_content::frame(18, 18, 0, "still");
    let i = e.encode(&p, false, &mut out).unwrap();
    assert!(i.keyframe);
    assert_eq!(out.len(), i.bytes + 1);
    assert_eq!(out[0], 42);
    let i = e.encode(&p, false, &mut out).unwrap();
    assert!(!i.keyframe);
    e.set_bitrate(4000);
    assert!(e.achieved_bitrate_kbps() > 0.);
    p.u.clear();
    let before = out.clone();
    let rec = e.reconstruction().clone();
    assert!(e.encode(&p, false, &mut out).is_err());
    assert_eq!(out, before);
    assert_eq!(e.reconstruction(), &rec);
}

#[test]
fn software_oracle_refinement_and_scene_changes() {
    for kind in ["still", "typing", "scroll", "noise", "motion"] {
        compare(640, 360, 2, 2000, kind, 32, false);
    }
}

#[test]
fn settled_desktop_skips_and_bitrate_changes_apply() {
    let mut e = Encoder::new(EncoderConfig {
        width: 640,
        height: 360,
        fps: 30,
        bitrate_kbps: 8000,
        threads: 4,
    })
    .unwrap();
    let p = encode_content::frame(640, 360, 0, "still");
    let mut out = Vec::new();
    let first = e.encode(&p, false, &mut out).unwrap();
    let initial = encode_content::raw(e.reconstruction());
    for _ in 1..15 {
        e.encode(&p, false, &mut out).unwrap();
    }
    let before = out.len();
    let info = e.encode(&p, false, &mut out).unwrap();
    assert!(!info.keyframe);
    assert_eq!(info.skipped_mbs, info.total_mbs);
    assert!(out.len() - before < 100);
    let error = |raw: Vec<u8>| {
        raw.into_iter()
            .zip(encode_content::raw(&p))
            .map(|(a, b)| {
                let d = a as i64 - b as i64;
                d * d
            })
            .sum::<i64>()
    };
    assert!(error(encode_content::raw(e.reconstruction())) < error(initial));
    assert!(first.qp >= info.qp);
    e.set_bitrate(1000);
    let mut max = 0.;
    for i in 0..32 {
        let before = out.len();
        e.encode(
            &encode_content::frame(640, 360, i, "motion"),
            false,
            &mut out,
        )
        .unwrap();
        let ratio = (out.len() - before) as f64 * 8. * 30. / 1_000_000.;
        max = f64::max(max, ratio);
    }
    assert!(max <= 8.);
    assert!(
        e.achieved_bitrate_kbps() <= 1100.,
        "{}",
        e.achieved_bitrate_kbps()
    );
}

#[test]
fn fractional_motion_oracle() {
    compare(130, 98, 3, 2000, "motion", 20, false);
}
