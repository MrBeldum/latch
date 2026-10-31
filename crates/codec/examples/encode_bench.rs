//! Generates the same padded desktop sequences used by the decoder oracles.
#[path = "../tests/encode_content.rs"]
mod content;
use latch_codec::{
    h264::{Encoder, EncoderConfig},
    Yuv420,
};
use std::{
    env,
    fs::File,
    io::{BufReader, Read, Write},
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let get = |key: &str, default: &str| {
        args.windows(2)
            .find(|a| a[0] == key)
            .map_or(default.to_owned(), |a| a[1].clone())
    };
    let width: u32 = get("--width", "1920").parse()?;
    let height: u32 = get("--height", "1080").parse()?;
    let fps: u32 = get("--fps", "30").parse()?;
    let frames: usize = get("--frames", "30").parse()?;
    let threads: usize = get("--threads", "2").parse()?;
    let kbps: u32 = get("--kbps", "8000").parse()?;
    let kind = get("--kind", "typing");
    let output = get("--output", "");
    let source = get("--source", "");
    let reconstruction = get("--reconstruction", "");
    let input = get("--input", "");
    let mut out = if output.is_empty() {
        None
    } else {
        Some(File::create(output)?)
    };
    let mut raw = if source.is_empty() {
        None
    } else {
        Some(File::create(source)?)
    };
    let mut rec = if reconstruction.is_empty() {
        None
    } else {
        Some(File::create(reconstruction)?)
    };
    let mut input = if input.is_empty() {
        None
    } else {
        Some(BufReader::new(File::open(input)?))
    };
    if frames < 2 {
        return Err("need at least two frames".into());
    }
    let mut e = Encoder::new(
        EncoderConfig {
            width,
            height,
            fps,
            bitrate_kbps: kbps,
            threads,
        }
        .with_frame_budget(get("--budget", "65").parse()?),
    )?;
    e.set_subpixel_refinement(get("--subpel", "yes") != "no");
    let metrics = get("--metrics", "yes") != "no";
    let timings = get("--timings", "no") == "yes";
    if timings {
        eprintln!("frame,ms,bytes,qp,skipped_mbs,effort,preparation_ms,deblock_ms,passes,bands_ms");
    }
    let paced = get("--paced", "no") == "yes";
    let pictures: Vec<_> = (0..frames)
        .map(|i| {
            if let Some(input) = &mut input {
                read_frame(input, width, height).expect("input frame")
            } else {
                content::frame(width, height, i, &kind)
            }
        })
        .collect();
    let mut latencies = Vec::new();
    let mut reconstructions = Vec::new();
    let clock = Instant::now();
    let mut millis = 0.;
    let mut bytes = 0usize;
    let mut max_frame = 0usize;
    let mut skips = 0u64;
    let mut qps = 0u64;
    let mut mse = 0u64;
    let mut samples = 0u64;
    for (i, p) in pictures.iter().enumerate() {
        if paced {
            let due = clock + Duration::from_secs_f64(i as f64 / fps as f64);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        if let Some(raw) = &mut raw {
            raw.write_all(&content::raw(p))?;
        }
        let mut data = Vec::new();
        let start = Instant::now();
        let info = e.encode(p, false, &mut data)?;
        let elapsed = start.elapsed().as_secs_f64() * 1000.;
        millis += elapsed;
        latencies.push(elapsed);
        if timings {
            eprintln!(
                "{i},{elapsed:.3},{},{},{},{},{:.3},{:.3},{},{}",
                info.bytes,
                info.qp,
                info.skipped_mbs,
                e.timing().effort,
                e.timing().preparation_ms,
                e.timing().deblock_ms,
                e.timing().passes,
                e.timing()
                    .band_ms
                    .iter()
                    .map(|v| format!("{v:.3}"))
                    .collect::<Vec<_>>()
                    .join(";")
            );
        }
        bytes += data.len();
        max_frame = max_frame.max(data.len());
        skips += info.skipped_mbs as u64;
        qps += info.qp as u64;
        if let Some(out) = &mut out {
            out.write_all(&data)?;
        }
        if metrics || rec.is_some() {
            reconstructions.push(e.reconstruction().clone());
        }
    }
    // Include the last display interval, so an on-time run reports 60, not 60.7 fps.
    if paced {
        let end = clock + Duration::from_secs_f64(frames as f64 / fps as f64);
        std::thread::sleep(end.saturating_duration_since(Instant::now()));
    }
    let paced_fps = frames as f64 / clock.elapsed().as_secs_f64();
    // Quality scoring is outside the paced loop as well as the encode timer.
    for (p, decoded) in pictures.iter().zip(&reconstructions) {
        let decoded = content::raw(decoded);
        if let Some(rec) = &mut rec {
            rec.write_all(&decoded)?;
        }
        for (a, b) in content::raw(p).into_iter().zip(decoded) {
            let d = a as i32 - b as i32;
            mse += (d * d) as u64;
            samples += 1;
        }
    }
    let mut steady = latencies[1..].to_vec();
    steady.sort_by(f64::total_cmp);
    eprintln!(
        "summary: steady_mean={:.3} p95={:.3} max={:.3} idr={:.3} paced_fps={:.3}",
        steady.iter().sum::<f64>() / steady.len() as f64,
        steady[(steady.len() * 95).div_ceil(100) - 1],
        steady.last().unwrap(),
        latencies[0],
        paced_fps
    );
    let psnr = 10. * (255. * 255. * samples as f64 / mse.max(1) as f64).log10();
    println!("kind,width,height,frames,threads,target_kbps,ms_frame,achieved_kbps,psnr,max_frame_budget,skip_percent,average_qp,last_window_kbps,p95_ms,max_ms,steady_mean_ms,idr_ms,paced_fps");
    println!("{kind},{width},{height},{frames},{threads},{kbps},{:.3},{:.3},{:.3},{:.3},{:.2},{:.2},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",millis/frames as f64,bytes as f64*8.*fps as f64/frames as f64/1000.,psnr,max_frame as f64*8.*fps as f64/(kbps as f64*1000.),skips as f64*100./(frames as f64*width.div_ceil(16) as f64*height.div_ceil(16) as f64),qps as f64/frames as f64,e.achieved_bitrate_kbps(),steady[(steady.len()*95).div_ceil(100)-1],steady.last().unwrap(),steady.iter().sum::<f64>()/steady.len() as f64,latencies[0],paced_fps);
    Ok(())
}
fn read_frame(input: &mut impl Read, width: u32, height: u32) -> std::io::Result<Yuv420> {
    let mut p = Yuv420::new(width, height);
    for (plane, stride, n) in [
        (&mut p.y, p.y_stride, 1),
        (&mut p.u, p.c_stride, 2),
        (&mut p.v, p.c_stride, 2),
    ] {
        for y in 0..height as usize / n {
            input.read_exact(&mut plane[y * stride..y * stride + width as usize / n])?;
        }
    }
    p.pad_edges();
    Ok(p)
}
