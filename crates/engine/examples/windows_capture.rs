//! Run on an interactive Windows desktop: windows_capture [seconds] [output] [width] [height] [fps] [kbps] [h264|baseline|hevc|av1].
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
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
        color: Default::default(),
        width: args.next().unwrap_or_else(|| "1920".into()).parse()?,
        height: args.next().unwrap_or_else(|| "1080".into()).parse()?,
        fps: args.next().unwrap_or_else(|| "60".into()).parse()?,
        bitrate_kbps: args.next().unwrap_or_else(|| "12000".into()).parse()?,
        codec: match args.next().as_deref() {
            Some("hevc") => Codec::Hevc,
            Some("av1") => Codec::Av1,
            Some("baseline") => Codec::H264Baseline,
            _ => Codec::H264High,
        },
    };
    struct Restore(Option<latch_engine::display::Previous>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = &self.0 {
                if let Err(e) = latch_engine::display::restore(previous) {
                    eprintln!("restore display: {e:#}");
                }
            }
        }
    }
    let _restore = Restore(latch_engine::display::prepare(
        config.width,
        config.height,
        config.fps,
    )?);
    let mut pipeline = latch_engine::windows::open_video(&config)?;
    eprintln!("{:?}", pipeline.info());
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let start = Instant::now();
    let mut frames = 0;
    let mut ages = Vec::new();
    while start.elapsed().as_secs_f64() < seconds {
        let before = Instant::now();
        let frame = pipeline.next(frames == 0, Duration::ZERO)?;
        ages.push(frame.captured.elapsed().as_secs_f64() * 1000.0);
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
    ages.sort_by(f64::total_cmp);
    eprintln!(
        "summary {}",
        serde_json::json!({
            "config": [config.width, config.height, config.fps, config.bitrate_kbps],
            "codec": config.codec, "encoder": pipeline.info().encoder,
            "fps": frames as f64 / start.elapsed().as_secs_f64(),
            "capture_to_encode_ms": {"p50": ages[ages.len()/2], "p95": ages[ages.len()*95/100]},
        })
    );
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Run this example on Windows with an interactive desktop.");
}
