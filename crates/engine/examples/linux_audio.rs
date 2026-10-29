//! Record the default sink monitor for N seconds and reject silence.
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use latch_engine::audio::{CHANNELS, FRAME};
    use std::time::{Duration, Instant};
    let seconds: u64 = std::env::args().nth(1).map_or(Ok(3), |s| s.parse())?;
    let mut capture = latch_engine::linux::open_audio()?;
    println!("{}", capture.describe());
    let start = Instant::now();
    let mut frame = [0; FRAME * CHANNELS];
    let (mut frames, mut nonzero, mut peak) = (0, 0, 0u16);
    let mut timeouts = 0;
    while start.elapsed() < Duration::from_secs(seconds) {
        if capture.read(&mut frame, Duration::from_millis(100))? {
            frames += 1;
            for &sample in &frame {
                nonzero += usize::from(sample != 0);
                peak = peak.max(sample.unsigned_abs());
            }
        } else {
            timeouts += 1;
        }
    }
    println!("frames={frames} timeouts={timeouts} nonzero_samples={nonzero} peak={peak}");
    anyhow::ensure!(
        nonzero > 0,
        "monitor was silent; play a tone while this example runs"
    );
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("linux_audio requires Linux");
}
