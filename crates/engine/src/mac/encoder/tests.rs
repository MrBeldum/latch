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

#[test]
fn software_iosurface_roundtrips() {
    for codec in [Codec::H264Baseline, Codec::H264High, Codec::Hevc] {
        roundtrip(codec, false);
    }
}

/// A property as the session reports it back; None when it has no value.
fn property(encoder: &Encoder, key: Ref) -> Option<Owned> {
    let mut value = ptr::null();
    // SAFETY: the session is live; a successful copy returns one owned reference.
    let status = unsafe { VTSessionCopyProperty(encoder.session.0, key, ptr::null(), &mut value) };
    check(status, "copy VideoToolbox property").ok()?;
    Owned::new(value).ok()
}
#[test]
fn sessions_are_real_time_without_reordering_and_tagged_bt709() {
    let _serial = ENCODER_TEST.lock().unwrap_or_else(|e| e.into_inner());
    for (codec, allow_hardware) in [
        (Codec::Hevc, true),
        (Codec::H264High, true),
        (Codec::H264Baseline, true),
        (Codec::H264Baseline, false),
    ] {
        let config = VideoConfig {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            fps: FPS,
            bitrate_kbps: 10_000,
            codec,
        };
        let encoder = Encoder::create(&config, allow_hardware).unwrap();
        // SAFETY: each copied value is the documented CF type for its key.
        unsafe {
            let real_time = property(&encoder, kVTCompressionPropertyKey_RealTime);
            assert!(CFBooleanGetValue(real_time.expect("RealTime").0));
            let reordering = property(&encoder, kVTCompressionPropertyKey_AllowFrameReordering);
            assert!(!CFBooleanGetValue(
                reordering.expect("AllowFrameReordering").0
            ));
            let rate = property(&encoder, kVTCompressionPropertyKey_ExpectedFrameRate);
            let mut fps = 0i64;
            assert!(CFNumberGetValue(
                rate.expect("ExpectedFrameRate").0,
                4,
                (&mut fps as *mut i64).cast(),
            ));
            assert_eq!(fps, i64::from(FPS), "{codec:?}");
            for (key, value) in [
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                ),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ),
            ] {
                let tag = property(&encoder, key).expect("colour tag");
                assert!(CFEqual(tag.0, value), "{codec:?}");
            }
        }
    }
}
