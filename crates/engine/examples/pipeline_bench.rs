//! The host's video pipeline without a network: the synthetic desktop (or,
//! on Linux, the X11 screen) through the pipeline `video::open` would pick,
//! paced as a session paces it, with per-stage times and the frame rate it
//! actually kept.
//!
//!     pipeline_bench [--width 1920] [--height 1080] [--fps 60] [--kbps 20000]
//!                    [--seconds 10] [--motion sweep|noise|still|x11]
//!                    [--software] [--out stream.h264]
//!
//! `--software` forces `latch-codec` where a hardware encoder would be
//! chosen. Encode includes conversion when the pipeline does not report it
//! separately (hardware pipelines).

use latch_engine::pattern::{Motion, Pattern};
use latch_engine::software::SoftwarePipeline;
use latch_engine::video::{Codec, VideoConfig, VideoPipeline};
use std::io::Write;
use std::time::{Duration, Instant};

fn cpu_seconds() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "cputime=", "-p", &std::process::id().to_string()])
        .output();
    let Ok(out) = out else { return f64::NAN };
    let s = String::from_utf8_lossy(&out.stdout);
    let mut total = 0.0;
    for part in s.trim().split(':') {
        total = total * 60.0 + part.parse::<f64>().unwrap_or(0.0);
    }
    total
}

fn line(label: &str, values: &mut [f64]) -> String {
    if values.is_empty() {
        return format!("{label} n=0");
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    format!(
        "{label} mean={:.2} p50={:.2} p95={:.2} max={:.2} ms",
        values.iter().sum::<f64>() / n as f64,
        values[n / 2],
        values[(n * 95 / 100).min(n - 1)],
        values[n - 1]
    )
}

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    if latch_engine::linux::run_helper() {
        return Ok(());
    }
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str, default: &str| {
        args.windows(2)
            .find(|w| w[0] == name)
            .map_or(default.to_string(), |w| w[1].clone())
    };
    let width: u32 = arg("--width", "1920").parse()?;
    let height: u32 = arg("--height", "1080").parse()?;
    let fps: u32 = arg("--fps", "60").parse()?;
    let kbps: u32 = arg("--kbps", "20000").parse()?;
    let seconds: f64 = arg("--seconds", "10").parse()?;
    let motion = arg("--motion", "sweep");
    let software = args.iter().any(|a| a == "--software");
    let out = arg("--out", "");
    let config = VideoConfig {
        width,
        height,
        fps,
        bitrate_kbps: kbps,
        codec: Codec::H264Baseline,
    };
    let motion = match motion.as_str() {
        "still" => Some(Motion::Still),
        "sweep" => Some(Motion::Sweep),
        "noise" => Some(Motion::Noise),
        "x11" => None,
        other => anyhow::bail!("unknown motion {other}"),
    };
    // Timings are only exposed by the software pipeline; others report the
    // whole `next` call as encode.
    enum Run {
        Software(Box<SoftwarePipeline<Pattern>>),
        #[cfg(target_os = "linux")]
        X11(Box<SoftwarePipeline<latch_engine::linux::X11Capture>>),
        Other(Box<dyn VideoPipeline>),
    }
    let mut run = match motion {
        Some(motion) if software || !hardware_available(&config) => Run::Software(Box::new(
            SoftwarePipeline::new(Pattern::new(width, height, fps, motion), &config)?,
        )),
        #[cfg(target_os = "linux")]
        None if software || !hardware_available(&config) => Run::X11(Box::new(
            SoftwarePipeline::new(latch_engine::linux::X11Capture::new()?, &config)?,
        )),
        _ => {
            if motion.is_some() {
                std::env::set_var(
                    "LATCH_TEST_PATTERN",
                    match motion {
                        Some(Motion::Still) => "still",
                        Some(Motion::Noise) => "noise",
                        _ => "sweep",
                    },
                );
            }
            Run::Other(latch_engine::video::open(&config)?)
        }
    };
    let pipeline: &mut dyn VideoPipeline = match &mut run {
        Run::Software(p) => p.as_mut(),
        #[cfg(target_os = "linux")]
        Run::X11(p) => p.as_mut(),
        Run::Other(p) => p.as_mut(),
    };
    let info = pipeline.info();
    eprintln!(
        "{} {}x{} {:?}, asked {width}x{height}@{fps} {kbps} kbps",
        info.encoder, info.width, info.height, info.codec
    );
    let mut file = if out.is_empty() {
        None
    } else {
        Some(std::io::BufWriter::new(std::fs::File::create(&out)?))
    };
    let (mut capture, mut convert, mut encode, mut total, mut latency) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut bytes = 0usize;
    let mut frames = 0usize;
    let cpu0 = cpu_seconds();
    let start = Instant::now();
    let mut first = true;
    while start.elapsed().as_secs_f64() < seconds {
        let pipeline: &mut dyn VideoPipeline = match &mut run {
            Run::Software(p) => p.as_mut(),
            #[cfg(target_os = "linux")]
            Run::X11(p) => p.as_mut(),
            Run::Other(p) => p.as_mut(),
        };
        let t = Instant::now();
        let frame = pipeline.next(first, Duration::from_millis(100))?;
        let elapsed = t.elapsed().as_secs_f64() * 1000.0;
        latency.push(frame.captured.elapsed().as_secs_f64() * 1000.0);
        let timings = match &run {
            Run::Software(p) => Some(p.timings()),
            #[cfg(target_os = "linux")]
            Run::X11(p) => Some(p.timings()),
            Run::Other(_) => None,
        };
        // The first frame is the IDR and the encoder's start-up; keep it out
        // of the steady-state numbers but count its bytes.
        if !first {
            if let Some(t) = timings {
                capture.push(t.capture.as_secs_f64() * 1000.0);
                convert.push(t.convert.as_secs_f64() * 1000.0);
                encode.push(t.encode.as_secs_f64() * 1000.0);
            } else {
                encode.push(elapsed);
            }
            total.push(elapsed);
        }
        first = false;
        frames += 1;
        bytes += frame.data.len();
        if let Some(file) = &mut file {
            file.write_all(&frame.data)?;
        }
    }
    let wall = start.elapsed().as_secs_f64();
    let cpu = (cpu_seconds() - cpu0) / wall * 100.0;
    println!(
        "{} {}x{}@{fps} {:?}: {:.1} fps, {:.2} Mbps, cpu {:.0}% of one core",
        info.encoder,
        info.width,
        info.height,
        motion,
        frames as f64 / wall,
        bytes as f64 * 8.0 / wall / 1e6,
        cpu
    );
    println!("{}", line("  capture", &mut capture));
    println!("{}", line("  convert", &mut convert));
    println!("{}", line("  encode ", &mut encode));
    println!("{}", line("  next() ", &mut total));
    println!("{}", line("  capture→encoded", &mut latency));
    Ok(())
}

/// Whether `video::open` would pick something other than `latch-codec`.
fn hardware_available(_config: &VideoConfig) -> bool {
    latch_engine::video::encodable()
        .first()
        .is_some_and(|c| *c != Codec::H264Baseline)
}
