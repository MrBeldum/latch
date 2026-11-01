//! A headless Latch viewer for measurements: connects, decodes, prints a
//! line a second and a JSON summary at the end. Against a host streaming
//! the test pattern, `--keys N` times N key presses to the decoded picture
//! (the pattern's top-left square flips on each press) on this machine's
//! own clock.
//!
//!     view HOST [--port 47860] [--width 1280] [--height 720] [--fps 60]
//!          [--kbps 10000] [--seconds 30] [--sample-ms 1000] [--keys 0] [--hevc] [--audio]
//!          [--point X,Y]   (one absolute pointer position, in picture pixels)
//!          [--moves N]     (N relative moves of +10,+5, one every 16 ms)
//!          [--dump PREFIX] (PREFIX-before.pgm and PREFIX-after.pgm: the
//!                           picture's luma either side of the moves)

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
    f.with_planes(|y, y_stride, _, _| {
        let mut sum = 0u64;
        for y_pos in 16..48 {
            for x_pos in 16..48 {
                let idx = y_pos * y_stride + x_pos;
                if idx < y.len() {
                    sum += y[idx] as u64;
                }
            }
        }
        sum / (32 * 32) > 128
    })
}

/// The next decoded picture's luma as a binary PGM.
fn dump(frames: &FrameSlot, path: &str) -> anyhow::Result<()> {
    let start = Instant::now();
    let frame = loop {
        if let Some(f) = frames.take() {
            break f;
        }
        anyhow::ensure!(
            start.elapsed() < Duration::from_secs(5),
            "no picture to dump"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let (w, h) = (frame.width as usize, frame.height as usize);
    let mut out = format!("P5\n{w} {h}\n255\n").into_bytes();
    frame.with_planes(|y, stride, _, _| {
        for row in 0..h {
            out.extend_from_slice(&y[row * stride..row * stride + w]);
        }
    });
    std::fs::write(path, out)?;
    eprintln!("wrote {path}");
    Ok(())
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
    let sample_ms: u64 = arg("--sample-ms", "1000").parse()?;
    anyhow::ensure!(
        sample_ms > 0 && sample_ms <= 1000,
        "sample-ms must be 1..=1000"
    );
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
            cursor: false,
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

    // Smoothness: when each decoded frame appears, polled every 250 µs.
    let timing_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let arrivals = {
        let done = timing_done.clone();
        let frames = frames.clone();
        std::thread::spawn(move || {
            let mut times = Vec::new();
            let mut seq = frames.seq();
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let now = frames.seq();
                if now != seq {
                    seq = now;
                    times.push(Instant::now());
                }
                std::thread::sleep(Duration::from_micros(250));
            }
            times
        })
    };

    if let Some((x, y)) = arg("--point", "")
        .split_once(',')
        .and_then(|(x, y)| Some((x.parse::<i16>().ok()?, y.parse::<i16>().ok()?)))
    {
        std::thread::sleep(Duration::from_millis(500));
        session
            .input()
            .mouse_position(x, y, width as i16, height as i16);
        eprintln!("pointer sent to {x},{y} of {width}x{height}");
    }

    let moves: u32 = arg("--moves", "0").parse()?;
    let dump_to = arg("--dump", "");
    if moves > 0 {
        std::thread::sleep(Duration::from_millis(500));
        if !dump_to.is_empty() {
            dump(&frames, &format!("{dump_to}-before.pgm"))?;
        }
        let input = session.input();
        for _ in 0..moves {
            input.mouse_move(10, 5);
            std::thread::sleep(Duration::from_millis(16));
        }
        eprintln!("{moves} relative moves sent");
        if !dump_to.is_empty() {
            std::thread::sleep(Duration::from_millis(500));
            dump(&frames, &format!("{dump_to}-after.pgm"))?;
        }
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
    let sample_start = Instant::now();
    while sample_start.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_millis(sample_ms));
        let second = t0.elapsed().as_secs_f64();
        while let Ok(e) = rx.try_recv() {
            eprintln!("elapsed_ms={} {e:?}", t0.elapsed().as_millis());
        }
        let s = session.stats();
        eprintln!(
            "{second:>6.2}s {:5.1} fps {:6.2} Mbps rtt {:3} ms decode {:5.2} ms host {:5.2} ms assembly {:5.2} ms queue {:5.2} ms loss {:4.1}% fec {:3} {}x{} {} | {}",
            s.fps, s.mbps, s.rtt_ms, s.decode_ms, s.host_ms, s.assembly_ms, s.queue_ms, s.loss_pct,
            s.fec_recovered, s.width, s.height, s.decoder, s.audio
        );
        lines.push(serde_json::json!({
            "fps": s.fps, "mbps": s.mbps, "rtt_ms": s.rtt_ms, "decode_ms": s.decode_ms,
            "host_ms": s.host_ms, "assembly_ms": s.assembly_ms, "queue_ms": s.queue_ms,
            "loss_pct": s.loss_pct, "fec_recovered": s.fec_recovered,
            "elapsed_ms": t0.elapsed().as_millis(), "window_ms": s.window_ms,
            "width": s.width, "height": s.height, "decoded_seq": frames.seq(),
            "received_mbps": s.received_mbps, "frames_lost": s.frames_lost,
            "video_problem": s.video_problem, "rtt_var_ms": s.rtt_var_ms,
            "requested_bitrate_kbps": s.requested_bitrate_kbps,
            "applied_bitrate_kbps": s.applied_bitrate_kbps, "pacing_queue_ms": s.pacing_queue_ms,
            "send_errors": s.send_errors, "send_retries": s.send_retries,
            "socket_send_bytes": s.socket_send_bytes, "socket_receive_bytes": s.socket_receive_bytes,
            "host_socket_send_bytes": s.host_socket_send_bytes,
            "host_socket_receive_bytes": s.host_socket_receive_bytes,
            // Sparse luma samples confirm changing decoded content without saving pictures.
            "luma_sample": frames.take().map(|f| {
                let sample: Vec<u8> = f.with_planes(|y, stride, _, _| (0..16).map(|i| {
                    y[(f.height as usize * (2 * i + 1) / 32) * stride + f.width as usize / 2]
                }).collect());
                frames.recycle(f);
                sample
            }),
        }));
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let cpu = (cpu_seconds() - cpu0) / elapsed * 100.0;
    let stats = session.stats();
    presses.sort_by(f64::total_cmp);
    timing_done.store(true, std::sync::atomic::Ordering::Relaxed);
    let times = arrivals.join().unwrap_or_default();
    let mut gaps: Vec<f64> = times
        .windows(2)
        .map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0)
        .collect();
    let mean_gap = gaps.iter().sum::<f64>() / gaps.len().max(1) as f64;
    let stdev_gap = (gaps.iter().map(|g| (g - mean_gap).powi(2)).sum::<f64>()
        / gaps.len().max(1) as f64)
        .sqrt();
    gaps.sort_by(f64::total_cmp);
    let gap = |p: f64| {
        gaps.get(((gaps.len() as f64 - 1.0) * p).round() as usize)
            .copied()
    };
    let span = times
        .last()
        .zip(times.first())
        .map_or(0.0, |(l, f)| (*l - *f).as_secs_f64());
    let pct = |p: f64| {
        presses
            .get(((presses.len() as f64 - 1.0) * p).round() as usize)
            .copied()
    };
    let summary = serde_json::json!({
        "host": host.to_string(), "asked": [width, height, fps, kbps],
        "sample_ms": sample_ms,
        "decoder": stats.decoder, "encoder": stats.encoder, "codec": stats.codec,
        "viewer_cpu_pct_one_core": cpu, "viewer_rss_mb": rss_mb(),
        "key_to_picture_ms": { "n": presses.len(), "min": presses.first(), "p50": pct(0.5), "p95": pct(0.95), "max": presses.last() },
        "frame_interval_ms": { "n": gaps.len(), "fps": if span > 0.0 { gaps.len() as f64 / span } else { 0.0 }, "mean": mean_gap, "stdev": stdev_gap, "p50": gap(0.5), "p95": gap(0.95), "p99": gap(0.99), "max": gaps.last() },
        "seconds": lines,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    session.stop();
    let start = Instant::now();
    while !session.finished() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::ensure!(
        session.finished(),
        "stream did not stop within five seconds"
    );
    eprintln!("stream stopped in {:?}", start.elapsed());
    Ok(())
}
