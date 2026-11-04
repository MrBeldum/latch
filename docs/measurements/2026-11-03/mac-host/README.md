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
  above that budget. H.264, software and Main10 keep the speed hint. The
  final head (8d7950f, which only changes bitrate updates, never used here)
  was rebuilt with `compare.py`'s `build()` and reproduced these outputs
  exactly: HEVC 1080p60/20 sweep 42.41 dB, IDR 1061/1329 kB; 1080p120/50
  still 46.00 dB; 1080p240/80 sweep 39.36 dB; H.264 1080p60/20 still
  39.33 dB.

Three runs of every build/case, rotating build order each repetition. The
Mac was shared with other agents' builds (load average 35–66 during the
runs); these are medians, not clean-machine throughput guarantees. Each
column is the independent median of that metric. main's synchronous,
low-latency-RC encoder suffered most from the load: its H.264 1080p60
runs managed 5–43 fps, so its PSNR there describes very few frames.

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

The committed runs came from an earlier working copy of the same build,
measure and decode steps run case by case; `compare.py` is those steps as
one script and its build of the final head reproduced them (above).
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

## Results

Medians; fps achieved, encode p50/p95, Mbps achieved over wall time, PSNR,
IDR max/mean. [matrix.md](matrix.md) adds frame counts and the largest frame.

| Case | fps main / ac7 / after | Encode p50/p95 ms main → ac7 → after | Mbps main / ac7 / after | PSNR dB main / ac7 / after | IDR max/mean kB main → ac7 → after |
| --- | --- | --- | --- | --- | --- |
| HEVC 1920×1080@60/20 sweep | 60 / 60 / 60 | 9.1/11.1 → 4.7/5.1 → 7.1/8.0 | 18.6 / 20.5 / 21.0 | 40.61 / 34.72 / 42.41 | 925/925 → 830/688 → 1329/1061 |
| HEVC 1920×1080@60/20 noise | 38 / 60 / 60 | 9.2/11.2 → 4.6/5.5 → 6.8/8.9 | 42.9 / 25.4 / 33.0 | 14.57 / 14.19 / 14.22 | 1350/1349 → 915/460 → 1169/591 |
| HEVC 1920×1080@60/20 still | 60 / 61 / 61 | 9.0/11.5 → 4.6/4.9 → 7.0/7.3 | 10.3 / 14.1 / 11.6 | 42.32 / 38.69 / 43.32 | 925/925 → 1018/782 → 1279/1036 |
| HEVC 1920×1080@60/50 sweep | 60 / 61 / 61 | 8.7/11.2 → 4.5/5.0 → 6.9/7.4 | 21.2 / 47.2 / 32.2 | 45.60 / 46.62 / 49.38 | 1266/1266 → 1675/1301 → 1970/1578 |
| HEVC 1920×1080@60/50 noise | 60 / 61 / 61 | 9.7/12.7 → 5.1/7.1 → 7.8/8.8 | 66.3 / 54.1 / 59.4 | 14.43 / 14.64 / 14.52 | 1728/1728 → 1302/654 → 1576/790 |
| HEVC 1920×1080@60/50 still | 60 / 61 / 61 | 9.5/12.8 → 4.5/6.3 → 7.1/8.7 | 10.2 / 18.7 / 14.0 | 46.00 / 46.37 / 46.58 | 1266/1266 → 1018/972 → 1279/1232 |
| HEVC 1920×1080@120/50 sweep | 106 / 120 / 119 | 8.9/12.8 → 5.5/6.4 → 7.5/8.6 | 28.8 / 52.8 / 47.3 | 44.27 / 40.72 / 47.70 | 1021/1021 → 1215/929 → 1970/1433 |
| HEVC 1920×1080@120/50 noise | 66 / 120 / 119 | 10.2/12.9 → 5.4/6.8 → 7.8/8.7 | 53.8 / 50.3 / 51.9 | 14.43 / 14.30 / 14.23 | 1456/1456 → 1009/507 → 1265/635 |
| HEVC 1920×1080@120/50 still | 49 / 119 / 118 | 9.9/80.5 → 4.9/7.8 → 7.1/7.5 | 4.0 / 21.5 / 14.1 | 44.10 / 43.59 / 46.00 | 1021/1021 → 1018/831 → 1279/1087 |
| HEVC 1920×1080@240/80 sweep | 45 / 233 / 228 | 9.9/82.2 → 4.7/5.9 → 4.7/7.3 | 10.5 / 81.2 / 79.4 | 44.17 / 39.36 / 39.36 | 925/925 → 1066/782 → 1066/782 |
| HEVC 1920×1080@240/80 noise | 30 / 230 / 235 | 10.1/86.7 → 4.6/5.1 → 4.7/5.9 | 27.9 / 77.1 / 78.9 | 14.82 / 14.28 / 14.28 | 1350/1350 → 867/507 → 867/507 |
| HEVC 1920×1080@240/80 still | 48 / 227 / 227 | 9.9/77.2 → 4.7/7.5 → 4.7/7.2 | 2.1 / 21.2 / 21.3 | 44.14 / 43.61 / 43.61 | 925/925 → 1018/758 → 1018/758 |
| HEVC 3024×1964@120/100 sweep | 26 / 118 / 119 | 24.4/105.4 → 10.5/12.9 → 10.7/12.3 | 13.4 / 102.4 / 103.1 | 43.70 / 36.18 / 36.18 | 2524/2524 → 2794/2044 → 2794/2044 |
| HEVC 3024×1964@120/100 noise | 21 / 119 / 117 | 25.5/112.5 → 10.1/14.7 → 10.6/15.0 | 37.1 / 100.1 / 98.5 | 14.26 / 14.17 / 14.17 | 3723/3723 → 2360/1187 → 2360/1187 |
| HEVC 3024×1964@120/100 still | 40 / 118 / 118 | 22.6/25.5 → 10.5/11.8 → 10.5/12.0 | 10.1 / 60.7 / 60.7 | 43.05 / 39.88 / 39.88 | 2524/2524 → 2919/2106 → 2919/2106 |
| H.264 High 1920×1080@60/20 sweep | 43 / 60 / 59 | 10.7/63.1 → 6.8/10.3 → 6.9/12.0 | 14.6 / 19.8 / 19.7 | 40.24 / 39.25 / 39.25 | 925/925 → 1087/817 → 1087/817 |
| H.264 High 1920×1080@60/20 noise | 7 / 59 / 60 | 87.3/335.6 → 6.6/11.6 → 6.7/12.3 | 9.8 / 28.9 / 29.0 | 14.70 / 14.23 / 14.23 | 1542/1541 → 970/507 → 970/507 |
| H.264 High 1920×1080@60/20 still | 5 / 60 / 60 | 147.3/693.7 → 6.6/7.8 → 6.6/8.2 | 1.0 / 9.3 / 9.2 | 43.18 / 39.33 / 39.33 | 925/925 → 844/695 → 844/695 |
| H.264 High 1920×1080@60/50 sweep | 7 / 60 / 60 | 88.1/491.6 → 6.8/9.4 → 6.9/10.7 | 2.6 / 34.0 / 34.2 | 46.73 / 47.30 / 47.30 | 1370/1370 → 2060/1451 → 2060/1451 |
| H.264 High 1920×1080@60/50 noise | 55 / 59 / 60 | 9.0/30.1 → 6.6/29.6 → 6.6/29.7 | 83.0 / 51.8 / 52.2 | 14.57 / 14.64 / 14.64 | 2684/2684 → 1341/693 → 1341/693 |
| H.264 High 1920×1080@60/50 still | 59 / 60 / 60 | 8.9/11.7 → 6.7/7.8 → 6.9/8.9 | 11.1 / 12.1 / 12.1 | 46.05 / 45.18 / 45.18 | 1370/1370 → 1254/1048 → 1254/1048 |
| H.264 High 1920×1080@120/50 sweep | 111 / 118 / 118 | 8.4/10.2 → 6.5/7.5 → 7.0/9.1 | 34.2 / 46.8 / 46.8 | 45.44 / 44.18 / 44.18 | 1031/1031 → 2060/1304 → 2060/1304 |
| H.264 High 1920×1080@120/50 noise | 63 / 117 / 117 | 9.5/20.0 → 7.0/8.8 → 6.8/8.2 | 57.9 / 49.4 / 49.4 | 14.39 / 14.29 / 14.29 | 1835/1835 → 970/507 → 970/507 |
| H.264 High 1920×1080@120/50 still | 110 / 118 / 118 | 8.5/10.2 → 6.5/7.5 → 6.6/8.8 | 10.8 / 10.6 / 10.6 | 45.18 / 42.92 / 42.92 | 1031/1031 → 1254/900 → 1254/900 |
| H.264 High 1920×1080@240/80 sweep | 105 / 190 / 191 | 9.2/11.5 → 10.1/10.5 → 10.1/10.5 | 27.1 / 64.9 / 65.0 | 45.18 / 45.24 / 45.24 | 925/925 → 2585/1566 → 2585/1566 |
| H.264 High 1920×1080@240/80 noise | 18 / 195 / 193 | 10.7/226.3 → 9.7/10.1 → 9.7/10.4 | 15.2 / 65.0 / 64.3 | 14.61 / 14.22 / 14.22 | 1541/1541 → 970/559 → 970/559 |
| H.264 High 1920×1080@240/80 still | 108 / 194 / 196 | 9.0/10.8 → 9.7/10.1 → 9.7/9.8 | 5.6 / 9.3 / 9.4 | 45.08 / 44.27 / 44.27 | 925/925 → 1254/900 → 1254/900 |
| H.264 High 3024×1964@120/100 sweep | 24 / 71 / 71 | 39.6/64.0 → 27.0/27.4 → 26.8/27.4 | 21.5 / 45.7 / 46.0 | 27.54 / 42.50 / 42.50 | 2635/1881 → 3638/2601 → 3638/2601 |
| H.264 High 3024×1964@120/100 noise | 20 / 71 / 71 | 47.1/51.9 → 26.6/27.1 → 26.5/27.2 | 28.9 / 63.9 / 63.9 | 13.62 / 14.10 / 14.10 | 3853/1986 → 2775/1451 → 2775/1451 |
| H.264 High 3024×1964@120/100 still | 36 / 73 / 72 | 26.5/32.5 → 26.4/27.1 → 26.4/27.1 | 12.4 / 17.8 / 17.8 | 46.89 / 42.34 / 42.34 | 4217/3426 → 3365/2465 → 3365/2465 |

HEVC's quality path restores and passes 5.1's PSNR at 1080p up to 120 fps
(60/20 sweep 40.61 → 34.72 → 42.41 dB; 120/50 sweep 44.27 → 40.72 →
47.70 dB) and keeps pace (≥118 of 120 fps, p95 ≤ 9 ms). The price is
larger IDRs: up to 1970 kB at 1080p120/50 against 1021 (5.1) and 1215 (ac7).
Above 1080p120 it keeps the fast path, so 1080p240 and 3024×1964@120 are
unchanged from ac7 (39.36 and 36.18 dB, at 228–235 and 117–119 fps).

H.264 is unchanged from ac7. `PrioritizeEncodingSpeedOverQuality=false`
produced bit-identical H.264 output (same PSNR and IDR sizes, three runs
each of 1080p60/20 and 1080p120/50, sweep and still; in
[tuning.jsonl](tuning.jsonl) as `h264-quality`), so the HEVC fix does not
carry over. H.264 trails 5.1 by 1.0 dB (60/20 sweep), 3.9 dB (60/20
still), 2.3 dB (120/50 still) and gains 0.6 dB at 60/50 sweep. Only the
low-latency rate control recovered it (40.24 dB at 60/20 sweep) and that
doubles the encode time, the problem this PR removed. The still cases
measure mostly the IDR forced every second; production sends none, so
how a still screen refines over a long GOP is not measured here.

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
Regression tests, with real VideoToolbox callbacks, HEVC and H.264 High:

- `recovery_skips_queued_pictures_and_coalesces_retries`: two P-frames are
  already out when a keyframe is requested; nothing leaves until the next
  submitted picture, which is the IDR; a retry while it is pending adds no
  second IDR; the request clears once it is out and the next frame is a P.
- `a_dropped_recovery_idr_is_retried_before_any_p_frame_leaves`.
- `a_bitrate_change_keeps_the_reference_chain_and_applies_to_the_next_picture`:
  20 → 2 Mbps with two pictures in the encoder. The change completes them
  first, so every later picture is encoded at the new rate; all pictures
  leave in order and only the first is an IDR (the earlier fix discarded
  the completed pictures and forced an IDR at nearly every rate change in
  a busy stream); frames 40–59 average under a quarter of frames 10–29.
  An unchanged rate does nothing.

Live ScreenCaptureKit capture, display selection/restart and capture-to-send
latency remain unverified because the shell lacks Screen Recording permission.
The tests exercise the encoder state used by `Video::next`, with real
VideoToolbox callbacks and synthetic surfaces. Main10 round trips still pass,
but HDR capture and older macOS versions are not measured. No installed host,
Windows display, protocol, viewer or network policy was changed.
