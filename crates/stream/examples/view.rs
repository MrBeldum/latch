//! A headless Latch viewer for measurements: connects, decodes, prints a
//! line a second and a JSON summary at the end. Against a host streaming
//! the test pattern, `--keys N` times N key presses to the decoded picture
//! (the pattern's top-left square flips on each press) on this machine's
//! own clock.
//!
//!     view HOST [--port 47860] [--width 1280] [--height 720] [--fps 60]
//!          [--kbps 10000] [--seconds 30] [--keys 0] [--hevc] [--audio]
//!          [--point X,Y]   (one absolute pointer position, in picture pixels)

use latch_stream::{Event, Frame, FrameSlot, Session, Settings};
use std::net::IpAddr;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn cpu_seconds() -> f64 {
    // getrusage through std is not available; /proc is Linux only. `ps`
    // works on every Unix this runs on.
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

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output();
    out.ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map_or(f64::NAN, |kb| kb / 1024.0)
}

fn square_is_white(f: &Frame) -> bool {
    let mut sum = 0u64;
    for y in 16..48 {
        for x in 16..48 {
            sum += f.y[y * f.y_stride + x] as u64;
        }
    }
    sum / (32 * 32) > 128
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let host: IpAddr = args
        .get(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or("127.0.0.1".parse()?);
    let arg = |name: &str, default: &str| {
        args.windows(2)
            .find(|w| w[0] == name)
            .map_or(default.to_string(), |w| w[1].clone())
    };
    let flag = |name: &str| args.iter().any(|a| a == name);
    let port: u16 = arg("--port", "47860").parse()?;
    let width: u32 = arg("--width", "1280").parse()?;
    let height: u32 = arg("--height", "720").parse()?;
    let fps: u32 = arg("--fps", "60").parse()?;
    let kbps: u32 = arg("--kbps", "10000").parse()?;
    let seconds: u64 = arg("--seconds", "30").parse()?;
    let keys: usize = arg("--keys", "0").parse()?;

    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(
        latch_stream::Server {
            address: host,
            port,
        },
        Settings {
            width,
            height,
            fps,
            bitrate_kbps: kbps,
            hevc: flag("--hevc"),
            audio: flag("--audio"),
            name: "view example".into(),
        },
        frames.clone(),
        tx,
        || {},
    );
    let asked = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Event::Connected) => break,
            Ok(Event::Failed { stage, message }) => anyhow::bail!("failed at {stage}: {message}"),
            Ok(other) => eprintln!("{other:?}"),
            Err(_) => anyhow::bail!("no stream after 20 s"),
        }
    }
    eprintln!("connected in {:?}", asked.elapsed());
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();

    if let Some((x, y)) = arg("--point", "")
        .split_once(',')
        .and_then(|(x, y)| Some((x.parse::<i16>().ok()?, y.parse::<i16>().ok()?)))
    {
        std::thread::sleep(Duration::from_millis(500));
        session.input().mouse_position(x, y, width as i16, height as i16);
        eprintln!("pointer sent to {x},{y} of {width}x{height}");
    }

    let mut presses = Vec::new();
    if keys > 0 {
        let input = session.input();
        let mut seq = frames.seq();
        let start = Instant::now();
        while frames.seq() == 0 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut white = frames.take().map(|f| {
            let w = square_is_white(&f);
            frames.recycle(f);
            w
        });
        for _ in 0..keys {
            std::thread::sleep(Duration::from_millis(100));
            seq = seq.max(frames.seq());
            let pressed = Instant::now();
            input.key(0x41, true, 0);
            loop {
                if pressed.elapsed() > Duration::from_secs(2) {
                    eprintln!("a press never showed up");
                    break;
                }
                if frames.seq() > seq {
                    seq = frames.seq();
                    if let Some(f) = frames.take() {
                        let now = square_is_white(&f);
                        frames.recycle(f);
                        if white.is_some_and(|w| w != now) {
                            presses.push(pressed.elapsed().as_secs_f64() * 1000.0);
                            white = Some(now);
                            break;
                        }
                        white = Some(now);
                    }
                }
                std::thread::sleep(Duration::from_micros(500));
            }
            input.key(0x41, false, 0);
        }
    }

    let mut lines = Vec::new();
    for second in 1..=seconds {
        std::thread::sleep(Duration::from_secs(1));
        while let Ok(e) = rx.try_recv() {
            eprintln!("{e:?}");
        }
        let s = session.stats();
        eprintln!(
            "{second:>3}s {:5.1} fps {:6.2} Mbps rtt {:3} ms decode {:5.2} ms host {:5.2} ms assembly {:5.2} ms queue {:5.2} ms loss {:4.1}% fec {:3} {}x{} {} | {}",
            s.fps, s.mbps, s.rtt_ms, s.decode_ms, s.host_ms, s.assembly_ms, s.queue_ms, s.loss_pct,
            s.fec_recovered, s.width, s.height, s.decoder, s.audio
        );
        lines.push(serde_json::json!({
            "fps": s.fps, "mbps": s.mbps, "rtt_ms": s.rtt_ms, "decode_ms": s.decode_ms,
            "host_ms": s.host_ms, "assembly_ms": s.assembly_ms, "queue_ms": s.queue_ms,
            "loss_pct": s.loss_pct, "fec_recovered": s.fec_recovered,
        }));
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let cpu = (cpu_seconds() - cpu0) / elapsed * 100.0;
    let stats = session.stats();
    presses.sort_by(f64::total_cmp);
    let pct = |p: f64| {
        presses
            .get(((presses.len() as f64 - 1.0) * p).round() as usize)
            .copied()
    };
    let summary = serde_json::json!({
        "host": host.to_string(), "asked": [width, height, fps, kbps],
        "decoder": stats.decoder, "encoder": stats.encoder, "codec": stats.codec,
        "viewer_cpu_pct_one_core": cpu, "viewer_rss_mb": rss_mb(),
        "key_to_picture_ms": { "n": presses.len(), "min": presses.first(), "p50": pct(0.5), "p95": pct(0.95), "max": presses.last() },
        "seconds": lines,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    session.stop();
    let start = Instant::now();
    while !session.finished() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
