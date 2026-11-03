# macOS host follow-up, 2026-11-03

M3 Pro, synthetic NV12 IOSurfaces, hardware HEVC Main and H.264 High.
This repeats the independent review's harness and source patterns, including
its two-second samples, startup IDR and forced IDR once a second. Production
has no periodic IDRs. This measures the encoder, not ScreenCaptureKit or the
network. No server or test port is used.

## Builds and method

- `main`: 9fe0ae9, original synchronous `encode` / `CompleteFrames`, including
  its low-latency rate-control selection.
- `ac7`: ac7b2f1, the reviewed PR head, asynchronous submission with two in flight,
  default rate control and the speed-over-quality hint.
- `after`: 5165841, default RC, two in flight, quality-over-speed for 8-bit
  hardware HEVC through 248,832,000 pixels/s (1080p120), speed-over-quality
  above that budget. H.264, software and Main10 keep the speed hint.

Three runs of every build/case, rotating build order each repetition. The
Mac was shared with other agents' builds; these are medians, not clean-machine
throughput guarantees. Each column is the independent median of that metric.

The source is the reviewer's exact 12-surface pool: a shifted noisy gradient
(`sweep`), independent full luma noise on each surface (`noise`), or the same
unchanging textured picture (`still`). Chroma stays at 128. There is no warmup.
`Pacer` uses its existing fixed grid, 1% faster than the requested fps.
Successful outputs carry their source frame indices; reference NV12 contains
only those indices, in output order. FFmpeg decodes Annex B independently and
computes all-plane PSNR against those exact sources, with normalized timebases
and timestamps. FFmpeg and file writes run after the encoder timer stops.

Encode latency is submission-to-callback for the pipelined builds and the
synchronous encode call's elapsed time for main. Achieved fps and Mbps divide
output frames/bytes by actual encoding wall time, including the final drain;
`mbps` in the CSV is the separate nominal-fps equivalent. Startup and IDR
costs are included. kB is decimal 1000 bytes. IDR max/mean includes all IDRs;
frame max includes every picture and Annex B parameter sets. The frame-count
column matters: the faster grid can produce a displayed fps near 60 despite
a dropped picture.

## Reproduce

From the repository root, with Cargo and FFmpeg on PATH:

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
python3 docs/measurements/2026-11-03/mac-host/compare.py --work-dir /tmp/latch-mac-compare
python3 docs/measurements/2026-11-03/mac-host/summarize.py /tmp/latch-mac-compare/runs.jsonl
```

`compare.py` builds isolated git archives and copies each test executable
before running it; it cannot accidentally measure a reused target binary as
a different revision. `--case hevc:1920:1080:60:20000:sweep` limits the matrix;
`--repeats 3` is the default. The Python scripts work with macOS's Python 3.9.
The ignored Rust `quality_benchmark` also runs directly with
`LATCH_VT_QUALITY=hevc:1920:1080:60:20000:sweep LATCH_VT_OUTPUT=/tmp/latch-quality`
and `cargo test --release -p latch-engine --lib quality_benchmark -- --ignored --nocapture`.
Artifacts are overwritten on each case and deleted after PSNR to avoid
retaining hundreds of GB of raw video. [runs.jsonl](runs.jsonl) retains all
270 observations; [medians.csv](medians.csv) and [matrix.md](matrix.md) contain
the complete three-build matrix, including frame counts and max frame size.

## Configuration search

The chosen quality path changes only `PrioritizeEncodingSpeedOverQuality`.
It retains RealTime=true, no reordering, MaxFrameDelayCount=0, the requested
ExpectedFrameRate and MaximumRealTimeFrameRate, AverageBitRate, the existing
1.5× bitrate/one-second DataRateLimits, and the endless GOP. Apple documents
the hint as an encoding-decision speed/quality tradeoff, not an alternative
rate-control mode ([property documentation](https://developer.apple.com/documentation/videotoolbox/kvtcompressionpropertykey_prioritizeencodingspeedoverquality)).

Tuning trials reused the same harness and are in [tuning.jsonl](tuning.jsonl):

- Low-latency RC, including disabled DataRateLimits, a 3× cap, and RealTime=false,
  retained the same compressed outputs/PSNR for a given case. It did not solve
  the throughput/rate deficit. At 1080p120/50 sweep its nominal equivalent was
  32.56 Mbps; changing timing/load changed wall bitrate, not coding decisions.
- Default RC with Quality=0.8 or MaxFrameDelayCount=1 did not change sweep PSNR.
- Spatial adaptive QP disabled made small, mixed changes. At 1080p60/20,
  fast HEVC sweep fell from 34.72 to 34.36 dB. Disabling it on the quality path
  did not recover meaningful encode headroom; keep the driver's default.
- MaxAllowedFrameQP=28 is unsuitable for bandwidth control: 1080p60/20 noise
  became 435 Mbps HEVC / 463 Mbps H.264 in those runs.
- Quality-over-speed at 1080p240/80 sweep improved PSNR to 47.31 dB, but only
  delivered 190 fps versus 238 on the fast path. At 3024×1964@120/100 it gave
  43.61 dB at 69 fps, versus 36.18 dB at 119 fps fast. Keep speed there.
- Existing investigations of ConstantBitRate/VBV, MinAllowedFrameQP and
  per-frame BaseFrameQP are described in [MACOS.md](../../../MACOS.md#measured-limits-m3-pro).
  No measured setting bounds just IDRs while preserving the following GOP's
  quality. The selected mode accepts a larger IDR to preserve PSNR.

This crossover is calibrated to M3 Pro, not a capacity probe for every Mac.
No tested configuration met every requested goal for every pattern/rate.
In particular, incompressible noise has low PSNR and can overshoot the rate;
the quality hint is not a universal improvement for noise. H.264 is already
hardware-limited at the largest/fastest cases. The full tables retain these
failures and tradeoffs.

## Validation and limits

`cargo fmt --all`, `cargo clippy --locked --workspace --all-targets -- -D warnings`,
`cargo test --locked --workspace --all-targets`, and `bash scripts/audit-native.sh`
passed locally, including the audio test that failed in the independent review.
Regression tests cover two queued P-frames before a recovery request, a request
retried while the IDR is pending, a dropped forced IDR, and a bitrate change
with queued work. An idle bitrate change preserves references without adding
an IDR; an unchanged bitrate does too.

Live ScreenCaptureKit capture, display selection/restart and capture-to-send
latency remain unverified because the shell lacks Screen Recording permission.
The tests exercise the encoder state used by `Video::next`, with real
VideoToolbox callbacks and synthetic surfaces. Main10 round trips still pass,
but HDR capture and older macOS versions are not measured. No installed host,
Windows display, protocol, viewer or network policy was changed.
