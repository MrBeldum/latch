//! Synthetic H.264 video decoder microbenchmark baseline.
//!
//! Note: Repeatedly decoding the same IDR fixture is a synthetic microbenchmark
//! of decoder throughput and frame allocation overhead; it is NOT representative
//! of end-to-end streaming, network transmission, or video encoding performance.

use anyhow::{bail, Context, Result};
use latch_core::update::sha256_hex;
use latch_stream::video::{new_decoder, Decoder, Frame};
use latch_stream::Codec;
use serde_json::json;
use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

const BUILTIN_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/gray-bars.h264");
const MAX_FIXTURE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    fixture: Option<String>,
    warmup: usize,
    iterations: usize,
    require_hardware: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fixture: None,
            warmup: 30,
            iterations: 300,
            require_hardware: false,
        }
    }
}

fn parse_args<I, S>(args: I) -> Result<Option<Config>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut config = Config::default();
    let mut iter = args.into_iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_ref() {
            "--help" => return Ok(None),
            "--fixture" => {
                let p = iter.next().context("missing --fixture")?;
                config.fixture = Some(p.as_ref().into());
            }
            "--warmup" => {
                let s = iter.next().context("missing --warmup")?;
                config.warmup = s.as_ref().parse()?;
                if config.warmup > 10_000 {
                    bail!("warmup exceeds max 10000");
                }
            }
            "--iterations" => {
                let s = iter.next().context("missing --iterations")?;
                config.iterations = s.as_ref().parse()?;
                if !(1..=100_000).contains(&config.iterations) {
                    bail!("iterations out of range 1..=100000");
                }
            }
            "--require-hardware" => config.require_hardware = true,
            unknown => bail!("unrecognized argument: '{unknown}'"),
        }
    }
    Ok(Some(config))
}

fn load_fixture(path: Option<&str>) -> Result<(Vec<u8>, bool)> {
    let Some(p) = path else {
        return Ok((BUILTIN_FIXTURE.to_vec(), true));
    };
    let file = File::open(p).with_context(|| format!("open {p}"))?;
    let len = file.metadata()?.len();
    if len == 0 || len > MAX_FIXTURE_BYTES {
        bail!("invalid fixture size: {len} bytes (limit 32MiB)");
    }
    let mut data = Vec::with_capacity(len as usize);
    file.take(MAX_FIXTURE_BYTES + 1).read_to_end(&mut data)?;
    if data.is_empty() || data.len() as u64 > MAX_FIXTURE_BYTES {
        bail!("invalid fixture file length");
    }
    Ok((data, false))
}

// This small harness deliberately accepts one self-contained, single-slice
// H.264 IDR. It is not an access-unit splitter or a packet-capture reader.
fn validate_fixture(data: &[u8]) -> Result<()> {
    let mut counts = [0usize; 32];
    if !(data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])) {
        bail!("fixture must be Annex B H.264, not a container or packet capture");
    }
    for (i, bytes) in data.windows(3).enumerate() {
        if bytes == [0, 0, 1] {
            let header = *data.get(i + 3).context("empty NAL unit")?;
            if header & 0x80 != 0 {
                bail!("invalid H.264 NAL header");
            }
            counts[usize::from(header & 0x1f)] += 1;
        }
    }
    if counts[5] != 1
        || counts[1..5].iter().any(|&n| n != 0)
        || counts[7] == 0
        || counts[8] == 0
        || counts[9] > 1
    {
        bail!("fixture requires SPS, PPS and exactly one IDR slice; multi-frame and multi-slice fixtures are unsupported");
    }
    Ok(())
}

fn validate_frame_planes(f: &Frame) -> Result<()> {
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2) * 2, h.div_ceil(2));
    if w == 0 || h == 0 {
        bail!("zero frame dimensions");
    }
    if f.y_stride < w || f.y_stride.checked_mul(h).is_none_or(|n| f.y.len() < n) {
        bail!("invalid Y plane buffer or stride");
    }
    if f.uv_stride < cw || f.uv_stride.checked_mul(ch).is_none_or(|n| f.uv.len() < n) {
        bail!("invalid UV plane buffer or stride");
    }
    Ok(())
}

fn validate_pixel_oracle(f: &Frame) -> Result<()> {
    if f.width != 64 || f.height != 64 || f.is_black() {
        bail!("pixel oracle failed: unexpected dimensions or black frame");
    }
    let (top, bot) = (16 * f.y_stride + 32, 48 * f.y_stride + 32);
    if top >= f.y.len() || bot >= f.y.len() {
        bail!("pixel oracle coordinates exceed Y buffer bounds");
    }
    let (white, black) = (f.y[top], f.y[bot]);
    if white < 230 || black > 20 {
        bail!("pixel oracle brightness failed: white={white} (>=230), black={black} (<=20)");
    }
    Ok(())
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    assert!(
        !sorted.is_empty(),
        "cannot compute percentile of empty slice"
    );
    let rank = ((p.clamp(0.0, 100.0) / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn run_bench(
    decoder: &mut dyn Decoder,
    fixture: &[u8],
    is_builtin: bool,
    cfg: &Config,
) -> Result<serde_json::Value> {
    // Priming decode: initializes session before querying actual decoder hardware state
    let prime = decoder
        .decode(fixture, true, None)?
        .context("no priming frame")?;
    validate_frame_planes(&prime)?;
    let mut spare = Some(prime);

    let (dec_name, is_hw) = (decoder.name(), decoder.name().contains("hardware"));
    if cfg.require_hardware && !is_hw {
        bail!("hardware acceleration required (--require-hardware), but active is '{dec_name}'");
    }
    let check_backend = |decoder: &dyn Decoder| -> Result<()> {
        if decoder.name() != dec_name {
            bail!("decoder backend changed from '{dec_name}' to '{}'; rerun for a consistent baseline", decoder.name());
        }
        Ok(())
    };

    // Warmup iterations (discarded from measured timings)
    for _ in 0..cfg.warmup {
        let mut f = decoder
            .decode(fixture, true, spare.take())?
            .context("warmup decode failed")?;
        f.ensure_cpu_planes();
        validate_frame_planes(&f)?;
        check_backend(decoder)?;
        spare = Some(f);
    }

    // Measured iterations
    let mut samples = Vec::with_capacity(cfg.iterations);
    let wall_start = Instant::now();
    for _ in 0..cfg.iterations {
        let t0 = Instant::now();
        let mut f = decoder
            .decode(fixture, true, spare.take())?
            .context("decode failed")?;
        f.ensure_cpu_planes();
        samples.push(t0.elapsed());
        validate_frame_planes(&f)?;
        check_backend(decoder)?;
        if is_builtin {
            validate_pixel_oracle(&f)?;
        }
        spare = Some(f);
    }
    let elapsed_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    let raw_decode_ns: Vec<_> = samples.iter().map(Duration::as_nanos).collect();
    samples.sort();

    let sum_ns: u128 = samples.iter().map(|d| d.as_nanos()).sum();
    let mean_wall_ms = (sum_ns as f64 / samples.len() as f64) / 1_000_000.0;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let last = spare.as_ref().unwrap();

    Ok(json!({
        "version": 1,
        "build": { "version": env!("CARGO_PKG_VERSION"), "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "debug_assertions": cfg!(debug_assertions) },
        "benchmark": "synthetic_decoder_microbenchmark",
        "note": "Repeated single IDR. Decode wall time includes CPU output copy into host memory; excludes capture, encoding, network, audio, and presentation. Loop elapsed time also includes frame validation. CPU output bytes are not GPU transfer measurements.",
        "decoder": dec_name,
        "hardware": is_hw,
        "source": { "hash": sha256_hex(fixture), "bytes": fixture.len(), "builtin": is_builtin },
        "config": { "warmup": cfg.warmup, "iterations": cfg.iterations, "require_hardware": cfg.require_hardware },
        "counts": { "priming": 1, "warmup": cfg.warmup, "measured": cfg.iterations, "samples": samples.len() },
        "actual_resolution": { "width": last.width, "height": last.height },
        "cpu_output_bytes": last.y.len() + last.uv.len(),
        "pixel_oracle": if is_builtin { serde_json::Value::Bool(true) } else { serde_json::Value::Null },
        "decode_wall_ns_in_order": raw_decode_ns,
        "timings": {
            "elapsed_ms": elapsed_ms,
            "mean_wall_ms": mean_wall_ms,
            "p50_wall_ms": ms(percentile(&samples, 50.0)),
            "p95_wall_ms": ms(percentile(&samples, 95.0)),
            "p99_wall_ms": ms(percentile(&samples, 99.0)),
            "max_wall_ms": ms(*samples.last().unwrap()),
        }
    }))
}

fn main() -> Result<()> {
    let config = match parse_args(std::env::args())? {
        Some(cfg) => cfg,
        None => {
            println!("Usage: decode_bench [--fixture <PATH>] [--warmup <N>] [--iterations <N>] [--require-hardware] [--help]\n\nFixture: one Annex B H.264 IDR slice with SPS/PPS (default: built-in 64x64 bars).\nWarmup: 0..10000 (default 30). Iterations: 1..100000 (default 300).\nSynthetic decode plus CPU output copy; excludes capture, encode, network, audio and presentation.");
            return Ok(());
        }
    };

    let (fixture_bytes, is_builtin) = load_fixture(config.fixture.as_deref())?;
    validate_fixture(&fixture_bytes)?;
    let mut decoder =
        new_decoder(Codec::H264High).context("failed to instantiate H.264 decoder")?;

    let output = run_bench(decoder.as_mut(), &fixture_bytes, is_builtin, &config)?;
    serde_json::to_writer_pretty(std::io::stdout(), &output)?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_valid_and_defaults() {
        assert_eq!(parse_args(["bench"]).unwrap().unwrap(), Config::default());

        let custom = [
            "b",
            "--fixture",
            "f",
            "--warmup",
            "0",
            "--iterations",
            "5",
            "--require-hardware",
        ];
        let cfg = parse_args(custom).unwrap().unwrap();
        assert_eq!(
            (cfg.warmup, cfg.iterations, cfg.require_hardware),
            (0, 5, true)
        );
        assert_eq!(cfg.fixture.as_deref(), Some("f"));

        assert!(parse_args(["bench", "--help"]).unwrap().is_none());
    }

    #[test]
    fn test_parser_invalid_inputs() {
        let bad: &[&[&str]] = &[
            &["b", "--unknown"],
            &["b", "-w", "1"],
            &["b", "--warmup=1"],
            &["b", "--fixture"],
            &["b", "--warmup"],
            &["b", "--warmup", "10001"],
            &["b", "--warmup", "abc"],
            &["b", "--iterations", "0"],
            &["b", "--iterations", "100001"],
        ];
        for a in bad {
            assert!(parse_args(*a).is_err());
        }
    }

    #[test]
    fn test_percentile_tails_and_ranks() {
        let single = [Duration::from_millis(42)];
        assert_eq!(percentile(&single, 0.0), Duration::from_millis(42));
        assert_eq!(percentile(&single, 50.0), Duration::from_millis(42));
        assert_eq!(percentile(&single, 100.0), Duration::from_millis(42));

        let samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&samples, 0.0), Duration::from_millis(1));
        assert_eq!(percentile(&samples, 50.0), Duration::from_millis(50));
        assert_eq!(percentile(&samples, 95.0), Duration::from_millis(95));
        assert_eq!(percentile(&samples, 99.0), Duration::from_millis(99));
        assert_eq!(percentile(&samples, 100.0), Duration::from_millis(100));
    }

    #[test]
    // `native` exists only on macOS.
    #[allow(clippy::needless_update)]
    fn test_pixel_oracle_validation() {
        let mut f = Frame {
            width: 64,
            height: 64,
            y_stride: 64,
            uv_stride: 64,
            y: vec![16; 4096],
            uv: vec![128; 2048],
            full_range: false,
            ..Default::default()
        };
        f.y[16 * 64 + 32] = 235;
        f.y[48 * 64 + 32] = 16;
        assert!(validate_pixel_oracle(&f).is_ok());

        let mut dim = f;
        dim.y[16 * 64 + 32] = 200;
        assert!(validate_pixel_oracle(&dim).is_err());

        let mut black = dim;
        black.y[16 * 64 + 32] = 16;
        assert!(validate_pixel_oracle(&black).is_err());

        let wrong = Frame {
            width: 32,
            height: 32,
            ..Default::default()
        };
        assert!(validate_pixel_oracle(&wrong).is_err());
    }

    #[test]
    fn fixtures_must_contain_one_self_contained_idr_slice() {
        validate_fixture(BUILTIN_FIXTURE).unwrap();
        for invalid in [b"not a bitstream".as_slice(), &[0, 0, 1, 0x65, 0x80], &[]] {
            assert!(validate_fixture(invalid).is_err());
        }
        for suffix in [BUILTIN_FIXTURE, &[0, 0, 1, 0x41, 0x80], &[0, 0, 1]] {
            let mut stream = BUILTIN_FIXTURE.to_vec();
            stream.extend_from_slice(suffix);
            assert!(validate_fixture(&stream).is_err());
        }
    }

    struct FakeDecoder {
        decoded: usize,
        is_hw: bool,
        fallback_at: usize,
    }

    impl Decoder for FakeDecoder {
        fn decode(&mut self, _: &[u8], _: bool, spare: Option<Frame>) -> Result<Option<Frame>> {
            self.decoded += 1;
            let mut f = spare.unwrap_or_default();
            f.fill((64, 64), (&[16; 4096], 64), (&[128; 2048], 64), false);
            f.y[16 * 64 + 32] = 235;
            f.y[48 * 64 + 32] = 16;
            Ok(Some(f))
        }

        fn name(&self) -> &'static str {
            match (self.decoded, self.is_hw && self.decoded < self.fallback_at) {
                (0, _) => "H.264 · software (uninitialized)",
                (_, true) => "H.264 · hardware",
                (_, false) => "H.264 · software",
            }
        }
    }

    #[test]
    fn test_hardware_detection_after_priming() {
        let cfg = Config {
            warmup: 0,
            iterations: 2,
            require_hardware: true,
            ..Default::default()
        };
        let mut hw = FakeDecoder {
            decoded: 0,
            is_hw: true,
            fallback_at: usize::MAX,
        };
        assert_eq!(hw.name(), "H.264 · software (uninitialized)");
        let report = run_bench(&mut hw, b"idr", true, &cfg).unwrap();
        assert_eq!(report["hardware"], true);
        assert_eq!(
            report["decode_wall_ns_in_order"].as_array().unwrap().len(),
            2
        );
        assert_eq!(hw.decoded, 3); // 1 priming + 0 warmup + 2 measured

        let mut sw = FakeDecoder {
            decoded: 0,
            is_hw: false,
            fallback_at: usize::MAX,
        };
        assert!(run_bench(&mut sw, b"idr", true, &cfg).is_err());

        let cfg = Config { warmup: 1, ..cfg };
        // A fallback in warmup or measurement must not inherit the prime's label.
        for fallback_at in [2, 3] {
            let mut changed = FakeDecoder {
                decoded: 0,
                is_hw: true,
                fallback_at,
            };
            assert!(run_bench(&mut changed, b"idr", true, &cfg).is_err());
        }
    }
}
