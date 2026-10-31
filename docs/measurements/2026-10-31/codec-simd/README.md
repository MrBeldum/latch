# Codec SIMD measurements — 2026-10-31

Baseline: `5afca2d`. Implementation: `7436ec8` (the later documentation commit contains this report). Apple M3 Pro, macOS aarch64; stuzha, Oracle A1 / Neoverse N1, Linux aarch64, four online CPUs. No native dependencies, assembly, engine changes or new public encoder API.

The encoder emits **byte-identical H.264** before/after for every measured configuration. Paired runs assert both the SHA-256 of the complete Annex B access units and every quality/rate-control field. All 168 hashes also match across the two architectures. PSNR and achieved bitrate are exactly unchanged, rather than just within a tolerance. PSNR here is the benchmark's combined visible Y/U/V metric. The complete per-kind table, including both 2 and 4 threads, 720p/1080p, FPS, PSNR and actual kbps, is in [results.md](results.md).

The 2× full-motion target was **not achieved**. On the Mac, four-thread 1080p encoding improves 1.22–1.44× across these kinds. Stuzha desktop improves 1.30–1.39× in the main paired matrix; synthetic full motion is 34.87 → 28.03 ms (28.7 → 35.7 fps). Stuzha is shared and busy: load average was 4.83 / 4.35 / 2.92 during the final latency check. Main-matrix regressions and rechecks are retained below, not silently discarded. This is not an end-to-end streaming or capture measurement.

**Method.** Target 60 fps / 8,000 kbps, 30-frame sequences including the first IDR, three paired runs per configuration with alternating before/after order. Reported ms/frame is the median of each build's three per-run averages; FPS is its reciprocal, not a measured delivery rate. Metrics and input generation happen outside the encode timer. Each remote encoder process is a short burst; final remote scripts run at nice 10. The service was never stopped, inspected or reconfigured. The synthetic `motion` sequence cannot reach the target rate over this half-second window (10.64 Mbps at 1080p/four threads in both builds); retain that actual bitrate when comparing its 17.779 dB PSNR. `video` is quarter-screen motion, `motion` is full-screen synthetic motion, `noise` is a stress case, and FFmpeg supplies `testsrc2` and `mandelbrot` through `--input`.

**Colour conversion**, single thread, same fixed-point BT.709 and rounded RGB 2×2 chroma averaging, exact scalar equivalence:

| Machine | Size | Initial before → after ms | Paired median before → after ms |
|---|---|---:|---:|
| mac | 1280×720 | 0.821 → 0.643 | 0.914 → 0.526 |
| mac | 1920×1080 | 1.348 → 0.878 | 2.292 → 0.912 |
| stuzha | 1280×720 | 1.987 → 1.326 | 1.943 → 1.320 |
| stuzha | 1920×1080 | 4.391 → 3.071 | 4.388 → 3.060 |

Mac timings also vary with other local work, particularly the paired colour baseline; the initial and repeated measurements are both reported.

**Sustained desktop latency**, 1080p/four threads, 120 frames at the same target, encoding only. Percentiles use the 119 frames after the initial IDR (nearest rank P95). These are short throughput runs, not paced real-world sessions; colour, capture and network time are additional.

| Machine | Kind | Mean ms | P50 ms | P95 ms | Max ms | Initial IDR ms |
|---|---|---:|---:|---:|---:|---:|
| mac | typing | 1.415 | 1.290 | 2.511 | 4.412 | 55.717 |
| mac | scroll | 2.415 | 2.147 | 4.284 | 9.713 | 56.428 |
| stuzha | typing | 2.771 | 2.408 | 5.088 | 6.058 | 133.655 |
| stuzha | scroll | 4.345 | 4.061 | 7.727 | 13.999 | 106.494 |

Stuzha's P95 encode plus average full-frame conversion is approximately 8.15 ms typing / 10.79 ms scrolling, below 16.67 ms with headroom for capture. The maximum scrolling encode plus average conversion can exceed that budget; neither the initial IDR nor all live-service frames are proven to meet 60 fps.

**Profile and stage timings.** `sample` at 1 ms over three seconds of 1080p synthetic motion identified worker chroma transform/packing, motion search, serial deblock and main-thread scroll detection. [profile-before.txt](profile-before.txt) retains collapsed sample counts (waiting samples are not CPU time). Separate temporary `Instant` instrumentation in `Encoder::encode` produced the following after-change means over 30 frames, including IDR. The instrumentation was built separately and removed from the source; it is not in the measured final benchmark binaries. Boundaries: validated input → before scroll dispatch; scroll dispatch → after half-pel preparation; worker dispatch → all retries/merges complete; deblock call. Allocation, cloning and rate-control setup are in setup; remaining post-deblock bookkeeping is outside this breakdown.

| Machine | Kind | Setup ms | Scroll + half-pel ms | Workers + merge + retries ms | Deblock ms |
|---|---|---:|---:|---:|---:|
| mac | typing | 0.161 | 0.018 | 2.346 | 0.367 |
| mac | motion | 0.165 | 0.262 | 5.321 | 2.005 |
| mac | mandelbrot | 0.134 | 0.976 | 2.459 | 0.897 |
| stuzha | typing | 0.560 | 0.100 | 4.568 | 0.897 |
| stuzha | motion | 0.568 | 1.061 | 16.449 | 6.921 |
| stuzha | mandelbrot | 0.565 | 4.355 | 8.089 | 2.959 |

**Changes kept.** Fused two-row NEON BGRA conversion; packed NEON residual loading/reconstruction; pairwise NEON SAD accumulation; exact lower-bound early rejection of losing motion candidates; DC-only chroma transform/quantization shortcuts; row fills for simple intra predictors; NEON weak luma deblock with scalar strong/chroma filtering; whole-edge bypass for provably zero boundary strengths; parallel scroll detection on the existing persistent slice workers. Every new SIMD operation has a scalar fallback and randomized scalar-equivalence coverage, including strides, tails, clipping, all deblock QPs/strengths and colour missing-row behaviour. Band boundaries, mode decisions, quantizers, bit writing and output bits are preserved.

**What did not solve the bottleneck.** Packed pixels alone were only a modest improvement and did not address serial scroll preparation or deblock. Initial individual runs, including outliers affected by concurrent local validation, are retained as `*-packed.csv`, `mac-weak.csv`, `mac-parallel-dc.csv` and `stuzha-parallel.csv`; they are exploratory, not the final speedup evidence. A general CAVLC/bit-writer rewrite was not justified by the sampled share. More slices/work queues were not implemented: new slice boundaries alter prediction/rate allocation, whereas the changes here preserve exact quality per bit. Serial deblock still costs about 6.9 ms for synthetic motion on stuzha and remains a worthwhile next target.

The main stuzha matrix showed 720p/four-thread motion and 1080p video/mandelbrot regressions amid large within-group swings. Five-run, same-input rechecks were performed independently; original and recheck numbers are both retained. They establish that some apparent regressions were scheduling noise, but do not justify a precise speedup claim on this live host.

| Size | Kind | Main before → after ms | Five-run recheck before → after ms |
|---|---|---:|---:|
| 1280 | video | 13.234 → 11.318 | 12.941 → 11.889 |
| 1280 | motion | 14.943 → 18.232 | 16.774 → 13.584 |
| 1920 | video | 17.554 → 23.634 | 17.248 → 14.373 |
| 1920 | motion | 34.868 → 28.030 | 37.615 → 36.569 |
| 1920 | mandelbrot | 19.762 → 21.008 | 20.028 → 15.338 |

**Engine recommendations.** Keep the existing four-worker cap on the four-core N1; two workers leave significant throughput unused in most cases. Use the existing `bgra_to_yuv420_rows` API with reliable dirty rows when capture provides them, and reuse the destination frame. Preserve timing separation between capture, conversion and encoding. Initial IDRs still take roughly 100–134 ms on this host; these encoder optimisations do not remove that stall. Measure live capture and delivery before claiming steady 1080p60; the short desktop tests show P95 headroom, not a long-duration guarantee. No new encoder API is required.

**Validation.** Release codec suite, explicit full-resolution FFmpeg and VideoToolbox reconstruction oracles, Clippy all targets with warnings denied, workspace fmt check, native audit, x86_64 Linux zigbuild, and all 27 x86_64 macOS library tests under Rosetta pass. AArch64 has 28 library tests (the NEON deblock test is architecture-specific). See [validation.txt](validation.txt). External ITU decoder fixtures were unavailable and the optional decoder cross-check environment was unset; encoder oracles ran, rather than being skipped.

**Reproduce.** Build both revisions with the repository release profile. The old scalar colour kernel was measured with the updated portable `color_bench` harness as well as the original harness; the new harness removes a Mac-specific wall-time assertion, adds 720p and black-boxes inputs/outputs. It is single-threaded; encoder thread count does not apply to it.

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
cargo build --release -p latch-codec --example encode_bench --example color_bench
cargo zigbuild --release -p latch-codec --example encode_bench --example color_bench --target aarch64-unknown-linux-gnu.2.39
# For each w/h in 1280/720 and 1920/1080, generate 30 frames:
ffmpeg -f lavfi -i "testsrc2=size=1920x1080:rate=60" -frames:v 30 -pix_fmt yuv420p -f rawvideo testsrc2-1920.yuv
ffmpeg -f lavfi -i "mandelbrot=s=1920x1080:r=60" -frames:v 30 -pix_fmt yuv420p -f rawvideo mandelbrot-1920.yuv
python3 paired.py /path/to/encode-before /path/to/encode-after /path/to/inputs > machine-paired.csv
python3 summarize.py > results.md
# Sustained latency CSV is on stderr; aggregate metrics remain on stdout:
encode_bench --width 1920 --height 1080 --threads 4 --fps 60 --frames 120 --kind scroll --metrics no --timings yes
```

For stuzha, copy the two binaries, inputs and `paired.py` to `/tmp/latch-bench`, run the script through `ssh stuzha` at nice 10, then remove that temporary directory. This run cleaned up that directory after collecting results. No PR was opened and no service was touched.
