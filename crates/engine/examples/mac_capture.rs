//! Run `mac_capture SECONDS OUTPUT.h264` (or `.hevc`) on a Mac authorized for Screen Recording.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use anyhow::{bail, ensure};
    use latch_engine::{
        audio, mac,
        video::{Codec, VideoConfig},
    };
    use std::{
        fs::File,
        io::{Seek, SeekFrom, Write},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 3,
        "usage: mac_capture SECONDS OUTPUT.h264|OUTPUT.hevc"
    );
    let seconds: f64 = args[1].parse()?;
    ensure!(
        seconds.is_finite() && seconds > 0.0 && seconds <= 3600.0,
        "duration must be in 0..=3600 seconds"
    );
    let path = std::path::Path::new(&args[2]);
    let codec = match path.extension().and_then(|s| s.to_str()) {
        Some("hevc") => Codec::Hevc,
        Some("h264") => Codec::H264High,
        _ => bail!("output extension must be .h264 or .hevc"),
    };
    // Open before creating output files, so an authorization error leaves no empty recording.
    let mut video = mac::open_video(&VideoConfig {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_kbps: 10_000,
        codec,
    })?;
    let mut audio = mac::open_audio()?;
    let mut stream = File::create(path)?;
    let mut wav = File::create(path.with_extension("wav"))?;
    wav.write_all(&[0; 44])?;
    let running = Arc::new(AtomicBool::new(true));
    let mut times = Vec::new();
    let mut count = 0;
    let start = Instant::now();
    let result = std::thread::scope(|scope| -> anyhow::Result<()> {
        let flag = running.clone();
        let sound = scope.spawn(move || -> anyhow::Result<u32> {
            let mut samples = [0; audio::FRAME * audio::CHANNELS];
            let mut bytes = 0u32;
            while flag.load(Ordering::Relaxed) {
                if audio.read(&mut samples, Duration::from_millis(20))? {
                    for sample in samples {
                        wav.write_all(&sample.to_le_bytes())?;
                    }
                    bytes += (samples.len() * 2) as u32;
                }
            }
            wav.seek(SeekFrom::Start(0))?;
            wav.write_all(b"RIFF")?;
            wav.write_all(&(bytes + 36).to_le_bytes())?;
            wav.write_all(b"WAVEfmt ")?;
            wav.write_all(&16u32.to_le_bytes())?;
            wav.write_all(&1u16.to_le_bytes())?;
            wav.write_all(&2u16.to_le_bytes())?;
            wav.write_all(&48_000u32.to_le_bytes())?;
            wav.write_all(&192_000u32.to_le_bytes())?;
            wav.write_all(&4u16.to_le_bytes())?;
            wav.write_all(&16u16.to_le_bytes())?;
            wav.write_all(b"data")?;
            wav.write_all(&bytes.to_le_bytes())?;
            Ok(bytes)
        });
        let result = (|| -> anyhow::Result<()> {
            while start.elapsed() < Duration::from_secs_f64(seconds) {
                let frame = video.next(count == 0, Duration::from_millis(100))?;
                let ms = frame.captured.elapsed().as_secs_f64() * 1000.0;
                eprintln!(
                    "frame={count} capture_to_encoded_ms={ms:.3} bytes={} keyframe={}",
                    frame.data.len(),
                    frame.keyframe
                );
                times.push(ms);
                stream.write_all(&frame.data)?;
                count += 1;
            }
            Ok(())
        })();
        running.store(false, Ordering::Relaxed);
        let bytes = sound
            .join()
            .map_err(|_| anyhow::anyhow!("audio recording worker panicked"))??;
        eprintln!("audio={} bytes, {}", bytes, audio::RATE);
        result
    });
    result?;
    times.sort_by(f64::total_cmp);
    if !times.is_empty() {
        eprintln!(
            "frames={count} capture_to_encoded_ms p50={:.3} p95={:.3} encoder={:?}",
            times[(times.len() - 1) / 2],
            times[(times.len() - 1) * 95 / 100],
            video.info()
        );
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_capture requires macOS");
}
