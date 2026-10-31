use super::*;
#[test]
fn parses_lengths_and_rejects_truncated_output() {
    let mut out = Vec::new();
    append_nals(&[0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41], 4, &mut out).unwrap();
    assert_eq!(out, [0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x41]);
    for data in [&[0, 0, 0][..], &[0, 0, 0, 2, 1], &[0, 0, 0, 0]] {
        assert!(append_nals(data, 4, &mut Vec::new()).is_err());
    }
}

use std::{fs, io::Write, process::Command, sync::Mutex as StdMutex};
static ENCODER_TEST: StdMutex<()> = StdMutex::new(());
const WIDTH: usize = 960;
const HEIGHT: usize = 540;
const FPS: u32 = 30;
const FRAMES: usize = 60;

fn pattern(frame: usize) -> Vec<u8> {
    let mut out = vec![128; WIDTH * HEIGHT * 3 / 2];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let sx = (x + frame * 7) % WIDTH;
            let sy = (y + frame * 3) % HEIGHT;
            let mut hash =
                (sx as u32).wrapping_mul(0x45d9f3b) ^ (sy as u32).wrapping_mul(0x119de1f3);
            hash ^= hash >> 16;
            hash = hash.wrapping_mul(0x45d9f3b);
            hash ^= hash >> 16;
            let noise = (hash % 81) as i32 - 40;
            let gradient = 48 + ((sx / 8 + sy / 8) % 128) as i32;
            out[y * WIDTH + x] = (gradient + noise).clamp(16, 235) as u8;
        }
    }
    out
}
fn surface() -> Owned {
    // SAFETY: the IOSurface property dictionary requests shareable storage. The Create is owned.
    unsafe {
        let empty = dictionary(&[]);
        let attrs = dictionary(&[(kCVPixelBufferIOSurfacePropertiesKey, empty.0)]);
        let mut image = ptr::null();
        check(
            CVPixelBufferCreate(ptr::null(), WIDTH, HEIGHT, NV12, attrs.0, &mut image),
            "create synthetic NV12 IOSurface",
        )
        .unwrap();
        let image = Owned::new(image).unwrap();
        assert!(!CVPixelBufferGetIOSurface(image.0).is_null());
        CVBufferSetAttachment(
            image.0,
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            1,
        );
        image
    }
}
fn fill(image: &Owned, source: &[u8]) {
    // SAFETY: exclusive synthetic buffer is locked while both planes are written with their real strides.
    unsafe {
        check(
            CVPixelBufferLockBaseAddress(image.0, 0),
            "lock synthetic surface",
        )
        .unwrap();
        for (plane, height, offset) in [(0, HEIGHT, 0), (1, HEIGHT / 2, WIDTH * HEIGHT)] {
            let stride = CVPixelBufferGetBytesPerRowOfPlane(image.0, plane);
            let base = CVPixelBufferGetBaseAddressOfPlane(image.0, plane);
            assert!(!base.is_null());
            for row in 0..height {
                ptr::copy_nonoverlapping(
                    source.as_ptr().add(offset + row * WIDTH),
                    base.add(row * stride),
                    WIDTH,
                );
            }
        }
        check(
            CVPixelBufferUnlockBaseAddress(image.0, 0),
            "unlock synthetic surface",
        )
        .unwrap();
    }
}
fn nal_types(bytes: &[u8], hevc: bool) -> Vec<u8> {
    bytes
        .windows(4)
        .enumerate()
        .filter_map(|(i, w)| {
            if w == [0, 0, 0, 1] {
                bytes
                    .get(i + 4)
                    .map(|b| if hevc { (b >> 1) & 63 } else { b & 31 })
            } else {
                None
            }
        })
        .collect()
}
/// FFmpeg, the independent decoder these tests check against: from PATH
/// or Homebrew's usual place.
fn ffmpeg() -> Option<std::path::PathBuf> {
    [
        "ffmpeg",
        "/opt/homebrew/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .find(|p| {
        Command::new(p)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn roundtrip(codec: Codec, allow_hardware: bool) {
    let _serial = ENCODER_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("no ffmpeg on this machine; skipping the independent decode of {codec:?}");
        return;
    };
    let config = VideoConfig {
        width: WIDTH as u32,
        height: HEIGHT as u32,
        fps: FPS,
        bitrate_kbps: 10_000,
        codec,
    };
    let mut encoder = Encoder::create(&config, allow_hardware).unwrap();
    let image = surface();
    let dir = std::env::temp_dir().join(format!("latch-mac-{}-{codec:?}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let stream = dir.join(if codec == Codec::Hevc {
        "stream.hevc"
    } else {
        "stream.h264"
    });
    let decoded = dir.join("decoded.nv12");
    let mut file = fs::File::create(&stream).unwrap();
    let mut sources = Vec::new();
    let mut bytes = 0;
    let mut captured = Instant::now();
    for i in 0..FRAMES {
        // Re-submit the retained IOSurface without writing it for an idle picture and a forced IDR.
        let source = if i == 15 || i == 16 {
            pattern(14)
        } else {
            pattern(i)
        };
        if i != 15 && i != 16 {
            fill(&image, &source);
            captured = Instant::now();
        }
        let forced = i == 16 || i == 40;
        let mut encoded = None;
        for _ in 0..30 {
            encoded = encoder.encode(image.0, captured, forced).unwrap();
            if encoded.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs_f64(1.0 / f64::from(FPS)));
        }
        let encoded = encoded.expect("encoder kept dropping the picture for one second");
        assert_eq!(encoded.captured, captured);
        if i == 0 || forced {
            assert!(encoded.keyframe, "{codec:?}: requested keyframe at {i}");
            let types = nal_types(&encoded.data, codec == Codec::Hevc);
            if codec == Codec::Hevc {
                assert!(types.starts_with(&[32, 33, 34]), "{types:?}");
                assert!(
                    types.contains(&19) || types.contains(&20),
                    "HEVC IDR expected: {types:?}"
                );
            } else {
                assert!(types.starts_with(&[7, 8]), "{types:?}");
                assert!(types.contains(&5), "H264 IDR expected: {types:?}");
            }
        }
        bytes += encoded.data.len();
        file.write_all(&encoded.data).unwrap();
        sources.extend(source);
    }
    drop(file);
    let output = Command::new(&ffmpeg)
        .args(["-v", "error", "-y", "-threads", "1", "-i"])
        .arg(&stream)
        .args(["-pix_fmt", "nv12", "-f", "rawvideo"])
        .arg(&decoded)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pixels = fs::read(&decoded).unwrap();
    assert_eq!(pixels.len(), sources.len(), "decoded frame count");
    let mse = pixels
        .iter()
        .zip(&sources)
        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
        .sum::<f64>()
        / pixels.len() as f64;
    let psnr = 10.0 * (255.0 * 255.0 / mse).log10();
    let mbps = bytes as f64 * 8.0 / 2.0 / 1_000_000.0;
    eprintln!(
        "{codec:?}: {} frames, hardware={}, PSNR={psnr:.2} dB, {mbps:.3} Mbps, artifacts={}",
        FRAMES,
        encoder.hardware,
        dir.display()
    );
    assert!(psnr > 35.0, "{codec:?}: PSNR {psnr:.2}");
    assert!(
        (5.0..=15.0).contains(&mbps),
        "{codec:?}: bitrate {mbps:.3} Mbps"
    );
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn hevc_iosurface_roundtrip() {
    roundtrip(Codec::Hevc, true);
}
#[test]
fn h264_high_iosurface_roundtrip() {
    roundtrip(Codec::H264High, true);
}
#[test]
fn h264_baseline_iosurface_roundtrip() {
    roundtrip(Codec::H264Baseline, true);
}

#[test]
fn dropped_callback_is_no_output_but_status_errors_propagate() {
    assert!(callback_frame(0, true, || panic!("dropped sample must not be read")).is_none());
    assert!(
        callback_frame(-12915, true, || panic!("failed sample must not be read"))
            .unwrap()
            .is_err()
    );
}

#[test]
fn software_session_encodes_an_iosurface() {
    let _serial = ENCODER_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let config = VideoConfig {
        width: WIDTH as u32,
        height: HEIGHT as u32,
        fps: FPS,
        bitrate_kbps: 10_000,
        codec: Codec::H264Baseline,
    };
    let mut encoder = Encoder::create(&config, false).unwrap();
    let image = surface();
    fill(&image, &pattern(0));
    let frame = encoder
        .encode(image.0, Instant::now(), true)
        .unwrap()
        .expect("software IDR");
    assert!(frame.keyframe);
    assert!(!encoder.hardware);
    encoder.bitrate(8_000).unwrap();
    let captured = Instant::now();
    // Model a session that was advertised as hardware at creation but becomes
    // unavailable at encode time (as on the virtual Mac CI runner).
    encoder.hardware = true;
    // SAFETY: invalidate our owned session, forcing the production encode error path.
    unsafe {
        VTCompressionSessionInvalidate(encoder.session.0);
    }
    let retried = encoder
        .encode(image.0, captured, false)
        .unwrap()
        .expect("replacement software IDR");
    assert!(retried.keyframe);
    assert_eq!(retried.captured, captured);
    assert_eq!(encoder.config.bitrate_kbps, 8_000);
    assert!(!encoder.hardware);
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub frames_encoded: usize,
    pub keyframes_count: usize,
    pub pframes_count: usize,
    pub latency_mean_ms: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_max_ms: f64,
    pub mean_frame_bytes: f64,
    pub max_frame_bytes: usize,
    pub max_to_mean_frame_ratio: f64,
    pub mean_keyframe_bytes: f64,
    pub mean_pframe_bytes: f64,
    pub keyframe_to_pframe_ratio: f64,
    pub psnr_db: f64,
}

fn create_surface_sized(width: usize, height: usize) -> Owned {
    unsafe {
        let empty = dictionary(&[]);
        let attrs = dictionary(&[(kCVPixelBufferIOSurfacePropertiesKey, empty.0)]);
        let mut image = ptr::null();
        check(
            CVPixelBufferCreate(ptr::null(), width, height, NV12, attrs.0, &mut image),
            "create synthetic NV12 IOSurface",
        )
        .unwrap();
        let image = Owned::new(image).unwrap();
        assert!(!CVPixelBufferGetIOSurface(image.0).is_null());
        CVBufferSetAttachment(
            image.0,
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            1,
        );
        image
    }
}

fn fill_surface_pattern(
    image: &Owned,
    base: &[u8],
    frame: usize,
    width: usize,
    height: usize,
) -> Vec<u8> {
    let mut nv12 = vec![128; width * height * 3 / 2];
    let shift_x = (frame * 7) % width;
    let shift_y = (frame * 3) % height;
    for y in 0..height {
        let base_y = (y + shift_y) % height;
        let dst_row = y * width;
        let src_row = base_y * width;
        nv12[dst_row..dst_row + (width - shift_x)]
            .copy_from_slice(&base[src_row + shift_x..src_row + width]);
        nv12[dst_row + (width - shift_x)..dst_row + width]
            .copy_from_slice(&base[src_row..src_row + shift_x]);
    }
    unsafe {
        check(
            CVPixelBufferLockBaseAddress(image.0, 0),
            "lock synthetic surface",
        )
        .unwrap();
        for (plane, h, offset) in [(0, height, 0), (1, height / 2, width * height)] {
            let stride = CVPixelBufferGetBytesPerRowOfPlane(image.0, plane);
            let base_ptr = CVPixelBufferGetBaseAddressOfPlane(image.0, plane);
            assert!(!base_ptr.is_null());
            for row in 0..h {
                ptr::copy_nonoverlapping(
                    nv12.as_ptr().add(offset + row * width),
                    base_ptr.add(row * stride),
                    width,
                );
            }
        }
        check(
            CVPixelBufferUnlockBaseAddress(image.0, 0),
            "unlock synthetic surface",
        )
        .unwrap();
    }
    nv12
}

pub fn run_encoder_benchmark_case(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    num_frames: usize,
) -> BenchmarkResult {
    let _serial = ENCODER_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let w = width as usize;
    let h = height as usize;

    // Generate base Y plane with synthetic gradient + hash noise
    let mut base_y = vec![128u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut hash = (x as u32).wrapping_mul(0x45d9f3b) ^ (y as u32).wrapping_mul(0x119de1f3);
            hash ^= hash >> 16;
            hash = hash.wrapping_mul(0x45d9f3b);
            hash ^= hash >> 16;
            let noise = (hash % 81) as i32 - 40;
            let gradient = 48 + ((x / 8 + y / 8) % 128) as i32;
            base_y[y * w + x] = (gradient + noise).clamp(16, 235) as u8;
        }
    }

    let config = VideoConfig {
        width,
        height,
        fps,
        bitrate_kbps,
        codec,
    };
    let mut encoder = Encoder::create(&config, true).expect("create hardware encoder");
    assert!(encoder.hardware, "expected hardware encoder");

    let image = create_surface_sized(w, h);
    let mut latencies_ms = Vec::with_capacity(num_frames);
    let mut keyframe_bytes = Vec::new();
    let mut pframe_bytes = Vec::new();
    let mut encoded_packets = Vec::with_capacity(num_frames);
    let mut sources = Vec::with_capacity(num_frames);

    let start_all = Instant::now();
    for f in 0..num_frames {
        let raw = fill_surface_pattern(&image, &base_y, f, w, h);
        sources.push(raw);
        let force_keyframe = f == 0 || f == num_frames / 2;
        let submit_time = Instant::now();
        let encoded = encoder
            .encode(image.0, submit_time, force_keyframe)
            .expect("encode failed")
            .expect("frame dropped");
        let lat_ms = submit_time.elapsed().as_secs_f64() * 1000.0;
        latencies_ms.push(lat_ms);

        let sz = encoded.data.len();
        if encoded.keyframe {
            keyframe_bytes.push(sz);
        } else {
            pframe_bytes.push(sz);
        }
        encoded_packets.push(encoded);
    }
    let _total_elapsed = start_all.elapsed();

    // Verify decode with VideoToolbox decoder
    let mut vt_decoder = latch_stream::video::new_decoder(codec).expect("VT decoder");
    for packet in &encoded_packets {
        let dec = vt_decoder
            .decode(&packet.data, packet.keyframe, None)
            .expect("VT decode failed");
        assert!(dec.is_some(), "VideoToolbox returned no picture");
    }

    // Verify decode with ffmpeg and compute PSNR
    let dir = std::env::temp_dir().join(format!(
        "latch-bench-{}-{}-{}",
        std::process::id(),
        codec.name(),
        width
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let bitstream_path = dir.join(if codec == Codec::Hevc {
        "stream.h265"
    } else {
        "stream.h264"
    });
    let mut file = std::fs::File::create(&bitstream_path).unwrap();
    for packet in &encoded_packets {
        file.write_all(&packet.data).unwrap();
    }
    file.flush().unwrap();
    drop(file);

    let raw_out_path = dir.join("decoded.yuv");
    let ffmpeg_bin = ffmpeg().expect("ffmpeg");
    let status = Command::new(&ffmpeg_bin)
        .args([
            "-y",
            "-i",
            bitstream_path.to_str().unwrap(),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            raw_out_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "ffmpeg decode failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    let decoded_bytes = std::fs::read(&raw_out_path).unwrap();
    let frame_bytes = w * h * 3 / 2;
    let decoded_frames = decoded_bytes.len() / frame_bytes;
    assert_eq!(decoded_frames, num_frames, "decoded frames count mismatch");

    let mut mse_sum = 0.0;
    for (i, source) in sources.iter().enumerate() {
        let dec_frame = &decoded_bytes[i * frame_bytes..(i + 1) * frame_bytes];
        let mut frame_mse = 0u64;
        for (a, b) in source.iter().zip(dec_frame) {
            let diff = (*a as i64) - (*b as i64);
            frame_mse += (diff * diff) as u64;
        }
        mse_sum += frame_mse as f64 / frame_bytes as f64;
    }
    let mean_mse = mse_sum / num_frames as f64;
    let psnr_db = if mean_mse > 0.0 {
        10.0 * (255.0f64 * 255.0f64 / mean_mse).log10()
    } else {
        99.0
    };

    let _ = std::fs::remove_dir_all(&dir);

    // Compute statistics
    let mut sorted_lat = latencies_ms.clone();
    sorted_lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let lat_mean = latencies_ms.iter().sum::<f64>() / num_frames as f64;
    let lat_p50 = sorted_lat[(num_frames as f64 * 0.50) as usize];
    let lat_p95 = sorted_lat[(num_frames as f64 * 0.95).min((num_frames - 1) as f64) as usize];
    let lat_max = sorted_lat[num_frames - 1];

    let all_sizes: Vec<usize> = encoded_packets.iter().map(|p| p.data.len()).collect();
    let mean_size = all_sizes.iter().sum::<usize>() as f64 / num_frames as f64;
    let max_size = *all_sizes.iter().max().unwrap();
    let mean_kf = keyframe_bytes.iter().sum::<usize>() as f64 / keyframe_bytes.len() as f64;
    let mean_pf = if pframe_bytes.is_empty() {
        0.0
    } else {
        pframe_bytes.iter().sum::<usize>() as f64 / pframe_bytes.len() as f64
    };

    BenchmarkResult {
        codec,
        width,
        height,
        fps,
        bitrate_kbps,
        frames_encoded: num_frames,
        keyframes_count: keyframe_bytes.len(),
        pframes_count: pframe_bytes.len(),
        latency_mean_ms: lat_mean,
        latency_p50_ms: lat_p50,
        latency_p95_ms: lat_p95,
        latency_max_ms: lat_max,
        mean_frame_bytes: mean_size,
        max_frame_bytes: max_size,
        max_to_mean_frame_ratio: max_size as f64 / mean_size,
        mean_keyframe_bytes: mean_kf,
        mean_pframe_bytes: mean_pf,
        keyframe_to_pframe_ratio: if mean_pf > 0.0 {
            mean_kf / mean_pf
        } else {
            1.0
        },
        psnr_db,
    }
}

#[test]
#[ignore]
fn benchmark_mac_encoder() {
    let cases = [
        // 1920x1080 @ 60fps, 20 Mbps & 50 Mbps
        (Codec::H264High, 1920, 1080, 60, 20_000),
        (Codec::Hevc, 1920, 1080, 60, 20_000),
        (Codec::H264High, 1920, 1080, 60, 50_000),
        (Codec::Hevc, 1920, 1080, 60, 50_000),
        // 3024x1964 @ 60fps, 20 Mbps & 50 Mbps
        (Codec::H264High, 3024, 1964, 60, 20_000),
        (Codec::Hevc, 3024, 1964, 60, 20_000),
        (Codec::H264High, 3024, 1964, 60, 50_000),
        (Codec::Hevc, 3024, 1964, 60, 50_000),
    ];

    println!("\n=== ENCODER BENCHMARK RESULTS ===");
    println!(
        "{:<8} {:<10} {:<7} {:<8} | {:<7} {:<7} {:<7} {:<7} | {:<8} {:<8} {:<6} | {:<8} {:<8} {:<6} | {:<6}",
        "Codec", "Resolution", "Bitrate", "Frames",
        "LatMean", "LatP50", "LatP95", "LatMax",
        "MeanBytes", "MaxBytes", "Max/Avg",
        "KFBytes", "PFrameB", "KF/P", "PSNR"
    );

    for (codec, w, h, fps, br) in cases {
        let res = run_encoder_benchmark_case(codec, w, h, fps, br, 60);
        println!(
            "{:<8} {}x{:<5} {:>2} Mbps {:>3} fr | {:>5.2}ms {:>5.2}ms {:>5.2}ms {:>5.2}ms | {:>8.0} {:>8} {:>5.2}x | {:>8.0} {:>8.0} {:>5.2}x | {:>5.2}dB",
            res.codec.name(),
            res.width,
            res.height,
            res.bitrate_kbps / 1000,
            res.frames_encoded,
            res.latency_mean_ms,
            res.latency_p50_ms,
            res.latency_p95_ms,
            res.latency_max_ms,
            res.mean_frame_bytes,
            res.max_frame_bytes,
            res.max_to_mean_frame_ratio,
            res.mean_keyframe_bytes,
            res.mean_pframe_bytes,
            res.keyframe_to_pframe_ratio,
            res.psnr_db,
        );
    }
}

#[test]
fn software_iosurface_roundtrips() {
    for codec in [Codec::H264Baseline, Codec::H264High, Codec::Hevc] {
        roundtrip(codec, false);
    }
}
