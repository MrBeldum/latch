//! Decodes an Annex B file and reports single-threaded wall-clock latency.
use latch_codec::h264::{nal::access_units, Decoder};
use std::{
    env, fs,
    io::{self, Write},
    time::Instant,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let input = args
        .next()
        .ok_or("usage: decode_bench input.h264 [output.yuv]")?;
    let data = fs::read(input)?;
    let mut output = args
        .next()
        .map(fs::File::create)
        .transpose()?
        .map(io::BufWriter::new);
    let mut decoder = Decoder::new();
    let mut times = Vec::new();
    for (i, unit) in access_units(&data).into_iter().enumerate() {
        let now = Instant::now();
        let frame = decoder
            .decode(unit)
            .map_err(|e| format!("access unit {i}: {e}"))?;
        let elapsed = now.elapsed().as_secs_f64() * 1000.;
        if let Some(p) = frame {
            times.push(elapsed);
            if let Some(out) = output.as_mut() {
                for (plane, stride, w, h) in [
                    (&p.y, p.y_stride, p.width as usize, p.height as usize),
                    (
                        &p.u,
                        p.c_stride,
                        p.width as usize / 2,
                        p.height as usize / 2,
                    ),
                    (
                        &p.v,
                        p.c_stride,
                        p.width as usize / 2,
                        p.height as usize / 2,
                    ),
                ] {
                    for row in plane.chunks(stride).take(h) {
                        out.write_all(&row[..w])?;
                    }
                }
            }
        }
    }
    if let Some(out) = output.as_mut() {
        out.flush()?;
    }
    if times.is_empty() {
        return Err("no pictures decoded".into());
    }
    times.sort_by(f64::total_cmp);
    println!(
        "frames={} average_ms={:.3} p50_ms={:.3} p95_ms={:.3}",
        times.len(),
        times.iter().sum::<f64>() / times.len() as f64,
        times[times.len() / 2],
        times[(times.len() * 95 / 100).min(times.len() - 1)]
    );
    Ok(())
}
