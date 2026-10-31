# Consumer/performance integration

Base: `e49efe8` (#32); merged performance parent: `ed8226b` (#38).
Runtime under test includes the completed-frame dispatch fix (`0997e8b`)
and negotiation/safety follow-up (`bd1f2a7`).

## Method

1080p60 sweep, 20,000 kbps requested, 40 key presses, three interleaved runs
per pair and path. Eight steady sampling seconds follow the key presses,
so each host run lasts about 14–15 seconds. Noise uses 30,000 kbps and no
keys. Loopback: M3 Pro MacBook Pro. Real path: stuzha A1/N1 → Mac over
Tailscale. Only the temporary `/tmp/latch-bench/serve-*` processes are managed;
no services or installed host binaries are changed by these runs.

The supplied plain #32 viewer did not record frame intervals. The baseline
viewer is #32 built with `pr32-timing-only.patch`: an independent polling
observer and JSON statistics in the measurement example, with **no changes
to engine, decoder, receiver, feedback, or pacing**. Both viewers use the
same 250 µs observer. Per-run p99 and SD span decoded arrivals during keys
and steady sampling; startup before connection is excluded. Key times mean
press → decoded picture, not physical display photons.

`runs.csv` contains every run; `summary.json` contains grouped medians.
Tables use the median of each run's steady per-second median (first sample
excluded), then the median across runs. Loss is the maximum reported
unrecovered loss in the steady samples. Mbps is compressed video payload;
wire throughput including FEC/audio is separately preserved in raw JSON.
`raw/` contains the original viewer output. Noise keys are absent.
An initial loopback noise pair overlapped compilation and was repeated
after builds finished; that first pair is retained in `exploratory/`,
not pooled with the quiet comparison. Binary SHA-256s are in `binaries.json`.

Run `python3 summarize.py` to regenerate the table. `loopback.sh` and
`remote.sh` invoke the provided harness. Build the merged ARM host in a
separate Cargo target directory from the baseline, copy it to
`stuzha:/tmp/latch-bench/serve-new`, and baseline to `serve-pr32`, before
running the remote matrix. Separate target directories avoid reusing local
path-dependency artifacts from the other build.

## Decisions

- **Cadence:** retain #38's `video::Pacer` in software, Windows MF and macOS;
  remove #32's duplicate `cadence`. `cadence.py` compiles both original
  schedulers against identical synthetic CPU work and production media
  priority. At 20 ms work, #32's skip-to-future-slot rule falls to 38.1 fps,
  SD 10.05 ms/p99 44.78 ms; Pacer delivers 50.0 fps, SD <0.01 ms/p99 20.00 ms.
  With 3 ms work, both stay near 60 fps (59.99 vs 60.47), SD 3.25 vs 3.06 ms.
  Full-pipeline results below also include the receiver and SIMD improvements;
  they cannot attribute every latency gain to cadence alone.
- **Congestion:** retain #32's controller and its wire budget, overhead/audio
  reservation, bounded sender, feedback, status counters and bottleneck tests.
  Add actual-throughput backoff, lowest-window host RTT (10 Hz pings), and
  base-RTT parity floors. Delete the redundant `latch_proto::rate` controller.
  Deterministic tests show an underfilled 50 Mbps target with 8 Mbps actual
  traffic cuts to 6.4 Mbps, ignores a transient viewer RTT spike when the host
  window is clean, enables pacing under pressure and releases it after two
  clean seconds. Saturated 10 Mbps and live queue-delay tests keep their
  assertions; the relay now releases packets on time even at low rates.
- **Send/receive latency:** clean paths bypass the token bucket (including a
  10,000-datagram no-wait test). Loss ≥1.5% or controller queue pressure enables
  #32's existing bounded 2.5×-budget/16-datagram bucket. The other major cause
  was the receiver waiting on readiness before handing completed pictures
  to decode. Dispatch now happens before the wait. Earlier same-host A/B
  records (`fix-*` vs `nofix-*` in the parent measurement session) show SD
  10.2→2.0 ms, p99 34.6→21 ms and key p50 42→30 ms from this fix alone.
- **Audio:** keep #32 unchanged. Its smoothed ±0.2% steering already holds
  25–45 ms mean queued audio in a 30-minute ±300 ppm drift test, with no gaps
  or cuts; 120 ms hard cap/50 ms trim and 40 ms preroll bound stall latency.
  Adding the second adaptive ±2% controller would duplicate this mechanism.
- **Sockets:** retain `proto::transport::configure_socket`, request 8 MiB both
  ways and preserve actual kernel-size readback/counters. Remove the second
  `sockets.rs` implementation.
- **Codec/FEC:** retain governor, persistent worker/search improvements,
  SIMD prediction/reconstruction, and FEC SSSE3/AVX2. #32 already has fused
  NEON colour conversion and shared encoder/decoder SIMD loop filtering;
  retain those implementations and their oracle/tail tests. Keep #32's
  equal-length assertion before unsafe FEC kernels. Existing component
  evidence is in `../codec-simd/` and `../codec-realtime/`.
- **Platforms:** retain media priority, data before parity, Windows NV12 ring,
  cached views, growable MF output, callback-driven events, 1.5-frame VBV and
  opt-in HEVC. Preserve elevated input, HDR/display recovery and lifecycle
  fixes. Retain Mac IOSurface→Metal zero-copy and VT settings. GPU Windows
  and live Mac desktop capture are not exercised by the synthetic harness.
- **Protocol/cursor:** additive protocol 1, serde defaults and unknown-control
  handling preserve #32/5.0.1 compatibility. Send shapes and accept drawing
  controls only after cursor negotiation; retain Linux/test-pattern local
  cursor. Keep immediate viewer cancellation (no synchronous stop write).

## Results

| Path / pattern | Pair | fps | Interval SD / p99 ms | Key p50 / p95 ms | Host ms | Mbps | Loss % |
|---|---|---:|---:|---:|---:|---:|---:|
| loop / sweep | pr32 | 59.9 | 7.78 / 35.87 | 32.5 / 39.1 | 3.33 | 0.48 | 0.0 |
| loop / sweep | merged | 59.9 | 1.58 / 22.95 | 13.7 / 17.5 | 2.78 | 0.53 | 0.0 |
| loop / noise | pr32 | 59.7 | 3.56 / 23.18 | — | 4.91 | 25.19 | 0.0 |
| loop / noise | merged | 59.9 | 1.67 / 21.70 | — | 4.80 | 24.70 | 0.0 |
| path / sweep | pr32 | 59.2 | 10.16 / 34.08 | 44.6 / 56.6 | 8.14 | 0.45 | 0.0 |
| path / sweep | merged | 59.9 | 2.11 / 21.56 | 31.7 / 40.4 | 6.74 | 0.54 | 0.0 |
| path / noise | pr32 | 52.0 | 10.89 / 45.50 | — | 14.76 | 17.02 | 0.0 |
| path / noise | merged | 59.7 | 6.49 / 25.54 | — | 10.69 | 24.71 | 4.4 |


## Verification

`cargo fmt --all -- --check`, native and Windows-target workspace clippy
with `-D warnings`, release workspace/all-target tests, and native audit
pass. The bottleneck assertions are unchanged. CI exercises macOS, Windows,
Linux x64/arm64, conformance, audits and deployment self-tests. Windows GPU
hardware and controlled real-path congestion remain unmeasured; use the
retained deterministic and relay tests for the latter.

Compatibility smoke runs also streamed a merged host to the existing 5.0.1
viewer and a #32 host to the merged viewer (640×360 sweep, five keys each).
Both delivered all keys and about 60 fps. No installed services were used.

The initial remote merged noise run had one late loss burst: the final
250 ms statistics window reported 4.36% unrecovered loss, two lost frames
and 27.9 fps, while its steady median was 59.7 fps. This is retained in the
main table, not excluded. A subsequent 12-second run (`rechecks/`) reported
zero loss, median 59.65 fps, interval SD 3.06 ms and p99 22.65 ms; the paired
#32 repeat had zero loss, median 55.67 fps, SD 9.05 ms and p99 45.74 ms.
The burst ended too near disconnection to measure controller recovery;
controlled loss/queue recovery is verified by the retained bottleneck tests.
