# Real-time codec effort — 2026-10-31

Baseline: `e1b42db` (the current HEAD supplied for this work). Encoder implementation: `56a46e9`, plus the media-priority change `b866c2e`. Apple M3 Pro / macOS aarch64 and stuzha / Oracle A1 / four Neoverse N1 CPUs. Changes are confined to `crates/codec` and this directory; no PR.

The encoder now adapts compression effort without dropping frames. It substantially improves synthetic full-motion throughput on stuzha, but **does not guarantee the requested 10.83 ms p95 encode budget**. Minimum effort, IDR/startup work and OS scheduling still impose a floor. A CPU can be too slow or too contended to encode a chosen resolution at a chosen rate, even at minimum effort. The short paced runs demonstrate throughput, not an end-to-end capture/network/viewer or long-duration latency guarantee.

See [results.md](results.md) for all 720p/1080p × typing/scroll/motion × before/off/on results: mean, p95, max, initial IDR, paced FPS, PSNR and actual bitrate. Both runs and every frame's timings are retained in `mac/` and `stuzha/`. Older development matrices are retained under `exploratory/`; their minimum effort still performed one refinement iteration and they are not the final implementation.

## Governor and API

`Encoder::new(config)` defaults to an encode-time budget of **65% of 1/fps**, or **10.833 ms at 60 fps**. `EncoderConfig::with_frame_budget(percent)` accepts 0–100; 0 disables all timing-dependent decisions. It returns `EncoderOptions`, accepted by `Encoder::new`. This builder keeps existing `EncoderConfig` struct literals source-compatible, including the engine and stream callers outside this task's allowed edit scope. There is no added required config field. Invalid percentages are rejected. `fps` remains the media rate used by rate control and SPS timing.

```rust
let encoder = Encoder::new(config.with_frame_budget(65))?; // live
let oracle = Encoder::new(config.with_frame_budget(0))?;   // deterministic
```

Every successful encode, including settled all-skip frames and initial/requested IDRs, measures wall time. The rolling nearest-rank p95 covers up to 32 frames. Two consecutive frames over 1.5× budget reduce effort by two levels immediately. Otherwise, after at least eight samples at the current level, p95 above budget with at least two over-budget samples reduces effort by one level. A single IDR or scheduling outlier cannot cause a demotion by itself. Recovery requires 64 consecutive observations whose rolling p95 is below 60% of budget, then raises effort one level. History resets on a level change, avoiding decisions based on old-level costs. There are no timer assertions in the governor's deterministic tests.

| Level | Compression effort |
|---|---|
| 0 | Existing full motion/intra search and adaptive subpel |
| 1 | Disable subpel, including whole-frame half-pel preparation |
| 2 | One diamond iteration at 2- and 1-pixel steps, higher early exit threshold; only try P-frame intra DC comparison when inter cost is poor; avoid soft bitrate retries on P frames |
| 3 | Additionally choose intra16 instead of intra4, including IDRs |
| 4 | Use the best motion predictor candidate directly, without refinement; DC-only intra16 mode search |

Exact reuse/skip checks, quantization, chroma reconstruction, refresh and the existing eight-interval hard access-unit limit remain. The governor never substitutes an old complete frame or drops an input frame. At levels 2–4, soft bitrate overshoots are corrected through the existing rolling rate-control feedback on following frames rather than repeatedly encoding the same P frame. Hard-bound retries and IDR retries still run. This intentionally permits larger instantaneous bitrate bursts; an impossible hard-bound access unit still returns the existing error.

`Encoder::timing() -> &EncodeTiming` reports `elapsed_ms`, `preparation_ms`, `band_ms` (per worker, summed across retries), `deblock_ms` (sum across workers/retries, **not** serial frame latency), `passes`, `effort` actually used, and `p95_ms`. Zero budget leaves p95 zero and effort zero; timing instrumentation stays enabled. Measurement is wall time, including preemption, rather than CPU cycles. The feedback affects the next frame, so it cannot prevent a sudden scheduling stall in the current one.

## Profile and independent speedups

`perf record` was attempted on stuzha and refused by `perf_event_paranoid=4`; no kernel settings, services or privileges were changed. A separately instrumented baseline records stage wall times in [profile-before.timings](profile-before.timings). The 18 frames after the IDR and first P frame average **1.482 ms preparation, 9.463 ms workers/merge, 6.341 ms serial deblock**, with one coding pass. The first P frame took ten passes. Reproduce that instrumentation with `profile-baseline.py` after `build-baseline.py`. These profile results are separate from the uninstrumented baseline matrix.

Deblocking now runs on the existing persistent slice workers before merging their pictures. Every slice signals `disable_deblocking_filter_idc=2`: H.264 filtering remains enabled within slices, while the three horizontal boundaries between four slices are not filtered. This removes the serial filter stage at every effort, including governor-off encoding, and changes the bitstream/reconstruction slightly. This is a quality trade-off, not a bit-identical SIMD optimization. Decoder oracles check the signaled behavior exactly, including cropped dimensions, arbitrary slice counts, and all effort transitions.

An exact lower-bound early exit also stops the P-frame intra DC SAD comparison once it cannot beat inter prediction. Media worker startup calls the engine's `media_thread()` helper included by path, matching `crates/stream`. macOS requests interactive QoS; Linux attempts nice -10 and continues if the unprivileged OS rejects it. No service or process priority is changed externally. Unit-test workers omit that request; benchmark and integration-test workers execute the production path.

The implementation keeps one band per thread. At 1080p, each band has 17 macroblock rows; the slowest worker varies (32 / 47 / 37 / 62 of 178 P frames) and the median longest/shortest wall-time ratio is 1.17. At 720p, the final band is consistently slowest (123 of 178 frames on stuzha, 170 on the Mac). It has 12 rows versus 11 in the others, and the synthetic pattern is spatially uneven; stuzha's median ratio is 1.39. A work queue could help this 720p imbalance, but would add slice boundaries and prediction/rate overhead. It was considered and deferred: 720p already reaches paced 60 FPS, while the failing 1080p p95 is dominated by the shared CPU, startup retries and the minimum effort floor, not a repeatably slow spatial band. The existing slice count and deterministic allocation remain. Raw counts are in [band-summary.txt](band-summary.txt).

## Results and quality cost

Stuzha full-motion steady encode mean improves **26.229 → 16.899 ms with governor off → 12.226 ms on** at 1080p. P95 is **56.722 → 24.897 → 14.866 ms**, and measured paced throughput is **38.91 → 53.69 → 60.00 FPS**. At 720p the corresponding means are **10.452 → 8.743 → 8.000 ms** and p95s **15.769 → 14.836 → 12.318 ms**; all three keep up in these short paced runs. The initial baseline here was sometimes faster than the prior report's 28 ms, and sometimes slower: individual 1080p baseline run averages including IDR were 19.445 and 36.613 ms. These pooled improvements include scheduling noise and are not an isolated 2.15× algorithmic speedup claim.

The **65% p95 target is missed for both stuzha motion sizes**. At 1080p effort 4 alone has p95 13.268 ms. The mean paced throughput recovers because encoding can catch up after stalls. Worst P frames remain **204.535 ms** on stuzha and **43.409 ms** on the Mac at 1080p with the governor on, while maximum initial IDRs are 257.746 and 42.674 ms. A synthetic throughput success does not eliminate those visible latency spikes.

At matched requested settings (60 fps, 8,000 kbps, four workers, identical 90-frame inputs):

| Machine / full motion | Before PSNR / actual Mbps | Off PSNR / actual Mbps | On PSNR / actual Mbps |
|---|---:|---:|---:|
| Mac 720p | 22.157 / 8.019 | 22.148 / 8.008 | 22.148 / 8.008 |
| Mac 1080p | 17.011 / 9.629 | 16.989 / 9.593 | 16.912 / 8.279 |
| Stuzha 720p | 22.157 / 8.019 | 22.148 / 8.008 | 22.014 / 8.040 |
| Stuzha 1080p | 17.011 / 9.629 | 16.989 / 9.593 | 18.414 / 8.115 |

The independent slice-filter change costs 0.009 dB at 720p and 0.022 dB at 1080p in this motion sequence, with small actual-rate changes; desktop deltas range from -0.005 to +0.013 dB. The 720p adaptive cost on stuzha is 0.143 dB versus baseline at nearly equal achieved bitrate. At 1080p, the requested bitrate is matched but the achieved bitrate is **not**: the old encoder overshoots and governor decisions alter the rate-control trajectory. The synthetic stuzha PSNR improvement is therefore not a matched-actual-rate or perceptual video-quality claim. Mac 1080p loses 0.099 dB versus baseline while using roughly 14% fewer bits. Search/mode cuts can lose detail and compression efficiency on other content. Natural video/game capture quality at matched actual rates remains to be evaluated before drawing general quality conclusions.

Mac motion means are 4.704 → 4.359 → 4.606 ms (720p) and 6.353 → 5.352 → 5.220 ms (1080p), with all configurations delivering about 60 paced FPS. The governor stays at level 0 for all measured 720p P frames. At 1080p two expensive startup frames push it to level 2, where its conservative recovery margin keeps it during this short window. Longer low-load runs can recover once p95 stays under 60% of budget for 64 frames. Desktop means are already low on both machines; some small timing regressions remain in the table and are not hidden.

## Method and limits

Two 90-frame runs per configuration, at 60 fps / 8,000 kbps / four threads. The second repeat reverses before/off/on order. Inputs are identical deterministic synthetic sequences from `encode_content.rs`; `motion` changes the whole screen and is not a claim about natural video quality. Pictures are pre-generated; reconstruction copies are made during pacing and quality scoring happens after it. Encode timing excludes those copies, quality scoring, sleep, input generation and output logging. Paced FPS includes encoding, reconstruction copying, logging, scheduling, startup and the final display interval; it is measured, not reciprocal encode time. Deadlines are absolute and overdue frames catch up; **no frames are dropped**, so 60 average FPS can coexist with late individual frames and a startup backlog.

Tables pool the 178 P frames after the initial IDR in each pair for mean/p95/max. Initial IDRs are reported separately and are included in paced FPS, PSNR and bitrate. PSNR combines visible Y/U/V squared error; bitrate is encoded bits per requested media second, not wire throughput divided by a potentially slower wall clock. “Off” is the new implementation with budget zero; it includes parallel slice filtering and media priority. Off is deterministic; on can produce different output when wall-clock load changes. Both runs are retained, including regressions and outliers. No precise cross-machine speedup or sustained realtime guarantee is inferred from these short live-host bursts.

Stuzha was reached only through `ssh stuzha`; no services were inspected, stopped or configured. Initial load was 2.93 / 2.75 / 2.36; final load was 2.71 / 1.99 / 2.06. Each process encodes only 90 frames, then exits. Temporary remote binaries, profile data and results were removed from `/tmp/latch-bench-codec` after collection. The earlier probe's separately copied timing file is retained as exploratory evidence.

## Engine recommendations

No engine change is required to enable the 65% default. To expose a policy setting, pass the chosen percentage through `Encoder::new(config.with_frame_budget(percent))`; use zero for offline fixtures/oracles. Existing `EncoderConfig` literals continue to compile. Read `encoder.timing()` after each successful encode to distinguish preparation, retries and per-worker delays, and correlate with capture/conversion/send queue timing. No late-frame hint API was added: observed encode wall time already responds to competing load, while a queued-frame hint needs separate end-to-end policy.

Do not interpret achieved 60 paced FPS as meeting the viewer's latency target. At effort 4 with persistent budget misses, this encoder has exhausted its search reductions. Measure live capture/conversion and negotiate a lower resolution or rate if needed; frame dropping is not performed by the governor. Initial/requested IDRs and first-P rate-control retries remain a source of long stalls. A future improvement should target initial rate-control/suppression prediction without relaxing the hard frame-size guard, and use real game/video captures to tune the mode decisions. Keep deblocking within slices enabled.

## Validation and reproduction

All required gates passed: `cargo test --release -p latch-codec` (30 library tests plus integration tests), explicit full-resolution FFmpeg and VideoToolbox matrices including adaptive effort, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `bash scripts/audit-native.sh`, and `cargo zigbuild -p latch-codec --release --target x86_64-unknown-linux-gnu`. Raw logs are `tests.txt`, `oracles.txt`, `clippy.txt`, `fmt.txt`, `audit.txt`, `x86-build.txt`. Deterministic oracle cases explicitly use budget zero; adaptive cases additionally validate low-budget output. Optional external ITU fixtures and the opt-in decoder cross-check retain their existing availability behavior. The Clippy command succeeds with an informational future-incompatibility notice for the existing `block` dependency.

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR=$HOME/.cache/latch-sysroot/aarch64
export PKG_CONFIG_LIBDIR=$HOME/.cache/latch-sysroot/aarch64/usr/lib/aarch64-linux-gnu/pkgconfig
cargo build --release -p latch-codec --example encode_bench
cargo zigbuild --release -p latch-codec --example encode_bench --target aarch64-unknown-linux-gnu.2.39
python3 docs/measurements/2026-10-31/codec-realtime/build-baseline.py /tmp/latch-codec-before
cargo build --release --manifest-path /tmp/latch-codec-before/Cargo.toml --example encode_bench
cargo zigbuild --release --manifest-path /tmp/latch-codec-before/Cargo.toml --example encode_bench --target aarch64-unknown-linux-gnu.2.39
# Each measurement process is a short burst; do not run benchmarks concurrently on one machine.
python3 docs/measurements/2026-10-31/codec-realtime/measure.py mac /path/to/before /path/to/after /path/to/mac-results
# Copy binaries and measure.py to stuzha:/tmp/latch-bench-codec/, run the same
# script there with machine=stuzha, collect results, then remove that directory.
python3 docs/measurements/2026-10-31/codec-realtime/summarize.py
```
