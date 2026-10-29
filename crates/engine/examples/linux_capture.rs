//! `DISPLAY=:9 linux_capture 5 out.h264 [--capture-only] [--unpaced]`.
//! Capture-only never calls the codec, so it works while that is being built.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use latch_engine::linux::X11Capture;
    use latch_engine::software::{Bgra, CpuCapture, SoftwarePipeline};
    use latch_engine::video::{Codec, VideoConfig, VideoPipeline};
    use std::io::Write;
    use std::time::{Duration, Instant};

    fn cpu() -> f64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: getrusage initializes the supplied rusage on success.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return 0.0;
        }
        let usage = unsafe { usage.assume_init() };
        usage.ru_utime.tv_sec as f64
            + usage.ru_utime.tv_usec as f64 / 1e6
            + usage.ru_stime.tv_sec as f64
            + usage.ru_stime.tv_usec as f64 / 1e6
    }
    fn report(label: &str, values: &mut [f64]) {
        values.sort_by(f64::total_cmp);
        if values.is_empty() {
            return;
        }
        println!(
            "{label}: p50={:.3} ms p95={:.3} ms",
            values[values.len() / 2],
            values[(values.len() * 95 / 100).min(values.len() - 1)]
        );
    }
    let args: Vec<_> = std::env::args().collect();
    let seconds: f64 = args.get(1).map_or(Ok(5.0), |s| s.parse())?;
    anyhow::ensure!(
        seconds.is_finite() && seconds > 0.0,
        "duration must be positive"
    );
    let capture_only = args.iter().any(|s| s == "--capture-only");
    let unpaced = args.iter().any(|s| s == "--unpaced");
    let mut capture = X11Capture::new()?;
    let (width, height) = capture.size();
    println!("{width}x{height}, SHM={}", capture.uses_shm());
    let mut capture_ms = Vec::new();
    let mut convert_ms = Vec::new();
    let mut encode_ms = Vec::new();
    let start_cpu = cpu();
    let start = Instant::now();
    if capture_only {
        let mut frame = Bgra::default();
        while start.elapsed().as_secs_f64() < seconds {
            let t = Instant::now();
            capture.capture_now(&mut frame)?;
            capture_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            if !unpaced {
                std::thread::sleep(Duration::from_secs_f64(1.0 / 60.0).saturating_sub(t.elapsed()));
            }
        }
        let checksum: u64 = frame.data.iter().map(|&v| u64::from(v)).sum();
        println!("BGRA checksum={checksum}");
    } else {
        let path = args.get(2).context("supply an output .h264 path")?;
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut pipeline = SoftwarePipeline::new(
            capture,
            &VideoConfig {
                width,
                height,
                fps: 60,
                bitrate_kbps: 12000,
                codec: Codec::H264Baseline,
            },
        )?;
        while start.elapsed().as_secs_f64() < seconds {
            let frame = pipeline.next(false, Duration::from_secs_f64(1.0 / 60.0))?;
            out.write_all(&frame.data)?;
            let t = pipeline.timings();
            capture_ms.push(t.capture.as_secs_f64() * 1000.0);
            convert_ms.push(t.convert.as_secs_f64() * 1000.0);
            encode_ms.push(t.encode.as_secs_f64() * 1000.0);
        }
        out.flush()?;
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "frames={} elapsed={elapsed:.3}s process CPU={:.2}% (100%=one core)",
        capture_ms.len(),
        (cpu() - start_cpu) / elapsed * 100.0
    );
    report("capture", &mut capture_ms);
    report("convert", &mut convert_ms);
    report("encode", &mut encode_ms);
    Ok(())
}
#[cfg(target_os = "linux")]
use anyhow::Context;
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("linux_capture requires Linux/X11");
}
