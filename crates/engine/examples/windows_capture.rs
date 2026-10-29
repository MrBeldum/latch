//! Run on an interactive Windows desktop: windows_capture [seconds] [out.h264].
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use latch_engine::video::{Codec, VideoConfig};
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    let mut args = std::env::args().skip(1);
    let seconds: f64 = args.next().unwrap_or_else(|| "10".into()).parse()?;
    anyhow::ensure!(
        seconds.is_finite() && seconds > 0.0 && seconds <= 86400.0,
        "seconds must be in (0, 86400]"
    );
    let path = args.next().unwrap_or_else(|| "capture.h264".into());
    let config = VideoConfig {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_kbps: 12000,
        codec: Codec::H264High,
    };
    let mut pipeline = latch_engine::windows::open_video(&config)?;
    eprintln!("{:?}", pipeline.info());
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let start = Instant::now();
    let mut frames = 0;
    while start.elapsed().as_secs_f64() < seconds {
        let before = Instant::now();
        let frame = pipeline.next(frames == 0, Duration::from_millis(100))?;
        file.write_all(&frame.data)?;
        eprintln!(
            "frame={frames} bytes={} idr={} next_ms={:.3} capture_age_ms={:.3}",
            frame.data.len(),
            frame.keyframe,
            before.elapsed().as_secs_f64() * 1000.0,
            frame.captured.elapsed().as_secs_f64() * 1000.0
        );
        frames += 1;
    }
    file.flush()?;
    eprintln!("{frames} frames in {:.3}s", start.elapsed().as_secs_f64());
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Run this example on Windows with an interactive desktop.");
}
