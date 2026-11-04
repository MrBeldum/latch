# FEC-first transport, 2026-11-04

Before: main `3d5d69e6aec2c139b6be4fff67aa10d798686940`. After: this branch.
All budgets below are **wire** budgets, including parity and headers, not video
payload. No installed host or port 47850 was used. No protocol number or UDP
layout changed; new JSON fields default on read.

## Random loss and parity cost

35 simulated seconds per case, 120 fps, 50 ms feedback, 10 ms RTT, 40 ms FIFO.
Seed 17; 1/3/5% independent packet loss plus Gilbert–Elliott bursts
(enter 0.002, exit 0.3), 1% duplication/reorder (4 ms), 0–4 ms jitter.
Budget means/SD and queue p95 use seconds 5–35; minimum budget, damage,
parity and IDRs count the whole run. IDR counts include startup and retries;
these codec-free IDRs have the same payload size as other frames.

| Loss / cap Mbps | Minimum budget Mbps, before → after | Lost frames/min | IDRs/min | Parity Mbps | Parity share of wire |
| --- | --- | --- | --- | --- | --- |
| 1% / 20 | 15.95 → 20.00 | 42.9 → 12.0 | 77.1 → 25.7 | 4.89 → 8.16 | 24.8% → 46.0% |
| 3% / 20 | 7.36 → 20.00 | 58.3 → 15.4 | 111.4 → 32.6 | 4.45 → 8.33 | 25.3% → 47.0% |
| 5% / 20 | 15.41 → 20.00 | 72.0 → 15.4 | 137.1 → 32.6 | 4.88 → 8.57 | 24.1% → 48.9% |
| 1% / 50 | 50.00 → 50.00 | 22.3 → 3.4 | 46.3 → 8.6 | 12.27 → 19.09 | 24.4% → 40.3% |
| 3% / 50 | 50.00 → 50.00 | 25.7 → 1.7 | 53.1 → 5.1 | 12.27 → 20.04 | 24.4% → 42.3% |
| 5% / 50 | 50.00 → 50.00 | 22.3 → 0.0 | 46.3 → 1.7 | 12.27 → 21.04 | 24.4% → 44.4% |
| 1% / 100 | 100.00 → 100.00 | 1.7 → 1.7 | 5.1 → 5.1 | 23.32 → 35.61 | 23.7% → 37.3% |
| 3% / 100 | 100.00 → 100.00 | 5.1 → 3.4 | 12.0 → 5.1 | 23.32 → 37.93 | 23.7% → 39.8% |
| 5% / 100 | 100.00 → 100.00 | 5.1 → 3.4 | 8.6 → 5.1 | 23.32 → 40.00 | 23.7% → 42.0% |

Every new case keeps at least 80% of capacity throughout; measured here all
nine keep 100%. Parity is deliberately expensive: 37–49% of transmitted wire
bytes versus 24–25% before (including that packet's headers in either case).
The extra parity costs 3.27–16.68 Mbps, or about 14–25 percentage points of
wire share. Total offered traffic stays within capacity in these cases:
17.5–17.7 / 47.4 / 95.3–95.4 Mbps at the 20/50/100 Mbps caps. Extra parity
reduces payload share; it is not free bitrate. Bursts can still exceed the
bounded protection: the 20 Mbps cases retain 12–15 lost frames/min.

Parity rules: path percentage 10–30%, raw loss fast attack / two-second decay,
base-RTT floors 15% at 40 ms and 20% at 100 ms; keyframes and negotiated
reference-recovery frames get at least 30% and four parity shards per block.
Data-erasure runs raise a normal block's floor to burst + 2, bounded by its
data count and the 255-shard FEC limit. Erasure runs are bounded to 32, decay
to zero, and can overestimate radio loss under reorder. Severe overflow and
congestion pacing do not teach burst length. MTU remains 1200 bytes.

## Idle to motion and actual sessions

The codec-free idle test sends 1% of the budget for five seconds. In seconds
1–5 a 25 ms local sender stall models CPU scheduling rather than a network
queue; ping RTT stays clean. This specifically reproduces misclassifying an
underfilled encoder as a slow network. Motion starts at five seconds; the
reported time is to the first delivered frame above 30 kB, about 80% of the
full 50 Mbps allowance. A completely clean idle run was already fast on main.

| Case | Before | After |
| --- | --- | --- |
| Idle minimum wire budget | 1 Mbps | 50 Mbps |
| Idle → full-rate frame | 3179 ms | 13 ms |
| Noise 640×360@12, 50 Mbps request, baseline applied target | 39.86 Mbps | 39.83 Mbps |
| Impaired 640×360@60, 6 Mbps request, sampled budget | not exposed | 6 Mbps in all eight samples |
| Impaired average decoded fps | 57.5 | 56.1 |
| Impaired FEC rebuilt shards (eight samples) | 69 | 91 |

The wall-clock session runs are single samples on a busy Mac, not a codec
performance claim. The old 797–1859 kbps idle baseline was not reproduced in
this run without the explicit sender stall. `impaired.rs` now asserts the
last three wire-budget samples stay at least 4.8 Mbps and still verifies exact
input delivery. Its relay now shares the bottleneck harness's timed delivery:
waiting for another receive or a coarse socket timeout must not add a fake
queue to a random-loss test. This timer change also limits direct before/after
wall-clock comparisons.

## Real path: Linux ARM64 host → Mac VideoToolbox viewer

Own `serve` on Linux port 47901, `LATCH_TEST_PATTERN=sweep`, 640×360@60,
20 Mbps requested, 25 seconds per build. Viewer-side
`LATCH_TEST_VIDEO_LOSS=3:4` drops 3% of video datagrams in four-packet bursts.
The Linux host and Mac viewer binaries are test copies. Test processes were
stopped afterward. No system-wide loss rule or display setting was changed.

| Metric | Before | After |
| --- | --- | --- |
| Minimum wire budget | 1 Mbps (host decision log) | 20 Mbps (all 25 viewer samples) |
| Decoded fps | 56.1 | 55.8 |
| Frame interval p95 / p99 ms | 18.8 / 69.6 | 19.8 / 84.9 |
| IDRs / recovery frames per minute | 2.4 / 74.2 | 4.8 / 76.5 |
| Recovery freeze p95 ms | 102.2 | 133.7 |

The budget collapse is fixed, but this small, mostly static pattern underfills
either budget and does not show a frame-rate or recovery-tail improvement.
The after run's freeze p95 is worse; it was taken during builds on shared
machines and needs repeated isolated timing before a latency claim. Bursts
that erase an entire tiny frame still need reference recovery. Recovery frames
already had the new four-parity minimum in this run.

## Existing regression scenarios

All existing test bounds are retained. A policer must still exercise damage,
but no longer must lose more than three windows (better FEC can prevent that).

| Scenario | Mean budget Mbps | Budget SD Mbps | Queue p95 ms | Lost / skipped frames (35 s) |
| --- | --- | --- | --- | --- |
| steady20 | 19.29 → 21.17 | 0.73 → 0.24 | 8.99 → 8.76 | 19/1 → 19/1 |
| steady50 | 49.21 → 52.24 | 1.21 → 0.68 | 7.99 → 8.79 | 16/2 → 17/0 |
| steady100 | 101.64 → 103.58 | 2.50 → 1.65 | 8.80 → 8.58 | 16/2 → 17/0 |
| cap20rtt150 | 19.53 → 20.52 | 0.58 → 0.09 | 14.66 → 7.68 | 48/13 → 48/13 |
| cap20rtt250 | 19.49 → 21.30 | 0.23 → 0.09 | 10.52 → 8.38 | 66/25 → 66/25 |
| cap100rtt150 | 102.17 → 104.16 | 1.13 → 0.39 | 9.00 → 8.59 | 33/10 → 34/9 |
| cap100rtt250 | 98.68 → 98.70 | 1.95 → 1.95 | 7.88 → 7.68 | 76/15 → 63/28 |
| rtt150 | 50.49 → 51.12 | 0.54 → 0.23 | 8.58 → 7.99 | 33/10 → 34/9 |
| rtt250 | 49.23 → 51.17 | 0.97 → 0.26 | 7.99 → 7.99 | 69/22 → 67/24 |
| slow250 | 50.71 → 52.12 | 0.66 → 1.48 | 11.41 → 14.36 | 42/0 → 43/0 |
| legacy1000 | 50.23 → 45.26 | 0.77 → 7.12 | 22.69 → 17.52 | 238/0 → 223/0 |
| legacy_radio | 100.00 → 100.00 | 0.00 → 0.00 | 7.78 → 7.68 | 10/31 → 7/23 |
| cap2 | 1.08 → 2.01 | 0.11 → 0.05 | 14.61 → 7.94 | 20/1 → 20/1 |
| cap5 | 3.06 → 4.77 | 0.83 → 0.00 | 28.26 → 6.14 | 20/1 → 20/1 |
| route | 86.30 → 86.26 | 32.46 → 32.54 | 8.38 → 7.88 | 0/0 → 0/0 |
| lan1g | 1000.00 → 1000.00 | 0.00 → 0.00 | 8.00 → 8.00 | 0/0 → 0/0 |

Budget can exceed a link's nominal capacity slightly because rounded encoder
reservation leaves headroom. Actual offered wire throughput is in the raw
logs. Low-cap queues improve substantially. A few startup loss counts move by
one or two frames; this is not a claim of bit-identical loss outcomes.

| Short scenario (12 s) | Before lost/skipped | After lost/skipped | Other |
| --- | --- | --- | --- |
| step | 7/7 | 7/7 | cut 60 → 60 ms; ceiling recovery 1110 → 710 ms |
| 240fps | 0/0 | 0/0 |  |
| random2 | 0/0 | 0/0 |  |
| wifi | 11/28 | 6/18 |  |
| rtt100 | 0/0 | 0/0 |  |
| jitter | 0/0 | 0/0 |  |
| wifi_jitter | 0/0 | 0/0 | both retain 100 Mbps |
| policer5 | 1/3 | 1/3 |  |
| policer10 | 21/11 | 9/5 |  |
| policer20 | 25/12 | 9/5 |  |

Moderate loss without queueing first gets 500 ms of FEC adaptation, followed
by a reversible 20% rate test. Only actual sending falling at least 10% and
mean raw loss falling by half over a horizon plus one second confirm a policer.
Inconclusive tests restore the budget. Confirmed policers hold their estimate
for ten seconds; severe/legacy fallback retains three seconds. Delay-gradient,
standing-queue and loaded-sender collapse paths remain in force. Sparse
windows, even when a few erased packets look like severe percentage loss,
cannot establish a network capacity. Four low-send reports mark an encoder
application-limited; its clean idle rate neither lowers capacity nor validates
a higher probe. This is a heuristic, not a full BBR/GCC classifier.

## Reproduce

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
cargo test --locked --release -p latch-engine --test steady_transport --test transport_scenarios --test impaired --test bottleneck -- --nocapture
cargo test --locked -p latch-proto
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
bash scripts/audit-native.sh
```

For before, extract commit `3d5d69e` into a throwaway directory and apply
[baseline-harness.patch](baseline-harness.patch). It only adds the same
measurement cases/counters and sender-stall stimulus to main's harness; it
uses main's controller, packetizer, 30% path FEC cap and video reservation.
The new >=80%/idle assertions are omitted to allow measurements of failures.
Use a separate `CARGO_TARGET_DIR` and run `steady_transport -- --nocapture`.
Original existing-scenario results were also captured before edits.

Cross-build the Linux measurement server on the Mac:

```sh
PKG_CONFIG_ALLOW_CROSS=1 \
PKG_CONFIG_SYSROOT_DIR=$HOME/.cache/latch-sysroot/aarch64 \
PKG_CONFIG_LIBDIR=$HOME/.cache/latch-sysroot/aarch64/usr/lib/aarch64-linux-gnu/pkgconfig \
cargo zigbuild --locked --target aarch64-unknown-linux-gnu.2.39 --release -p latch-engine --example serve
# Copy only to ~/gfn-transport-fec; run there, remember/stop that exact PID:
# LATCH_TEST_PATTERN=sweep RUST_LOG=latch_engine=debug ./serve --bind 100.70.121.94 --port 47901 --telemetry
LATCH_TEST_VIDEO_LOSS=3:4 target/release/examples/view 100.70.121.94 \
  --port 47901 --width 640 --height 360 --fps 60 --kbps 20000 --seconds 25
```

## Validation and limits

Validation results are recorded in the PR. No new dependency or native build
step was added. The Mac lacks shell screen-recording permission; capture was
synthetic. The real path was not a saturated capacity test. No on-device
Windows desktop session, 4K motion/IDR-spike test, real Wi-Fi AP test or long
multi-seed reliability run was performed. Long-RTT/caps/policer coverage above
is deterministic simulation. Wire compatibility uses serde defaults and the
existing variable parity field; no negotiated capability changed.
