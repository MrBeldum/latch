# FEC-first transport, 2026-11-04

Three revisions, each cell **main / 42adc9e / this branch**: main
`3d5d69e`, the PR as first submitted (`42adc9e`, independently reviewed in
`review-58`), and this branch after the review fixes. All budgets are
**wire** budgets (parity and headers included). No installed host or port
47850 was used. No protocol number, UDP layout or capability changed; new
JSON fields default on read.

## What the review found, and what changed

| Review case | main | 42adc9e | this branch |
| --- | --- | --- | --- |
| S1: encoder offers 10 Mbps under a 100 Mbps budget, queue-free 5 Mbps policer, 50% loss, 30 s | backs off, ~4.85 Mbps | stays at 100 Mbps, 50% loss forever | backs off, 4.85 Mbps (unit test bound 4.5–5.25) |
| S2: harness FEC policy | production | **uncapped** in the harness (up to 100%) | one shared `FecPolicy` (server and both harnesses) |
| S2: `slow250` queue p95 under the production cap (bound 16 ms) | 11.4 ms | 21.2 ms | 12.8 ms |
| S3: idle 5 s, capacity 100 → 5 Mbps at 2.5 s, motion at 5 s, 250 ms RTT: queue peak 5.0–5.5 s / lost frames | 40 ms / 95 | 40 ms / 37 | **4.1 ms / 1** |
| S3 with a 4× keyframe on resume | 40 ms / 96 | 40 ms / 37 | 10.5 ms / 1 |
| S4: protected frame (30%, ≥4 parity) sized by the reservation, wire-equivalent at 1 Mbps/240, 5/120, 20/120, 100/120 | no protected floor | 2.40 / 7.37 / 20.89 / 113.05 Mbps | 1.05 / 4.80 / 18.43 / 95.85 Mbps (1.05 = the irreducible five-datagram layout, the permitted burst) |
| S11: 90 Mbps policer + 10% random loss, residual damage every window, 60 s | collapses to the 1 Mbps floor | restores 100 Mbps (re-tests the overshoot forever) | 87.3 Mbps; re-tested once (5 s near 100 Mbps) when the 10 s policer memory lapses |

S5–S10 are the `fifo2x*`, `step_loss` and `loss3mixed*` rows below; S1, S4
and S11 are unit tests in `congestion.rs`, S3 is `idle_drop*` in
`steady_transport.rs`.

Design of the fixes (details in [ENGINE.md](../../../ENGINE.md)):

* **Underfilled is not unconstrained.** >10% loss with under 90% of the sent
  rate arriving, sustained for 500 ms or a feedback horizon, is pressure even
  for an encoder below its budget; the queue-free rate test lowers the rate
  actually sent. A confirmed test records the random-loss floor below the
  policer, which lapses with the policer memory.
* **One policy.** `congestion::FecPolicy` (loss decay, RTT floor, 30% cap,
  burst floor) and `latch_proto::video::fec_parity_shards` (the parity layout,
  including the protected floor) are shared by the server, the reservation and
  the simulations.
* **Idle keeps the target, not its proof.** After a 1 s grace the rate trusted
  for resumed frames halves every 500 ms (to 1/32 of the target, twice the idle
  peak, ≥1 Mbps); the encoder and pacer use it. Motion restarts there, held for
  one feedback horizon, then doubles the carried rate per validated horizon.
  This costs idle→motion time when nothing changed: **678 ms** at 10 ms RTT
  instead of 13 ms (main: 3179 ms).
* **Reservation includes the protected floor** while a keyframe or recovery
  frame is owed.

## Method

Codec-free deterministic simulations (`steady_transport.rs`, 35 s, and
`transport_scenarios.rs`, 12 s): 120 fps, seed 17, the production pacer and
parity policy, 40 ms FIFO unless noted, link loss before the FIFO. Means, SD
and queue p95 cover seconds 5–35; lost/skipped frames cover the whole run.
Older revisions run **this branch's** harness through
[compare.py](compare.py), which exposes each revision's own controller,
reservation and production FEC policy (30% cap; 42adc9e's burst floor; main
has none) behind the harness API and turns assertions into printed SOFTFAIL
lines. Raw output: [sim-main.txt](sim-main.txt),
[sim-42adc9e.txt](sim-42adc9e.txt), [sim-this.txt](sim-this.txt).

## Random loss and parity cost

1/3/5% independent loss plus Gilbert–Elliott bursts (enter 0.002, exit 0.3),
1% duplication/reorder (4 ms), 0–4 ms jitter, 10 ms RTT, cap = ceiling.
All three use the production 30% cap here, so 42adc9e's numbers are its
production behaviour, not the uncapped figures it first published
(3%/20 Mbps: 15.4 lost/min and 47% parity there).

| Loss / cap | Mean budget Mbps | Lost frames/min | Skipped | Parity % of wire | Wire Mbps |
|---|---|---|---|---|---|
| loss1cap20 | 19.6 / 20.0 / 20.0 | 42.9 / 30.9 / 29.1 | 88 / 62 / 61 | 24.8 / 32.9 / 32.2 | 19.7 / 18.4 / 18.4 |
| loss3cap20 | 16.6 / 20.0 / 20.0 | 58.3 / 30.9 / 36.0 | 120 / 64 / 74 | 25.3 / 33.9 / 34.4 | 17.6 / 18.4 / 18.4 |
| loss5cap20 | 19.6 / 20.0 / 20.0 | 72.0 / 42.9 / 32.6 | 156 / 87 / 69 | 24.1 / 33.9 / 34.8 | 20.2 / 18.4 / 18.4 |
| loss1cap50 | 50.0 / 50.0 / 50.0 | 22.3 / 22.3 / 17.1 | 52 / 52 / 40 | 24.4 / 24.2 / 25.0 | 50.4 / 47.9 / 47.9 |
| loss3cap50 | 50.0 / 50.0 / 50.0 | 25.7 / 30.9 / 27.4 | 60 / 72 / 64 | 24.4 / 24.4 / 24.4 | 50.4 / 47.9 / 47.9 |
| loss5cap50 | 50.0 / 50.0 / 50.0 | 22.3 / 37.7 / 29.1 | 52 / 88 / 68 | 24.4 / 23.9 / 24.2 | 50.4 / 47.9 / 47.9 |
| loss1cap100 | 100.0 / 100.0 / 100.0 | 1.7 / 5.1 / 3.4 | 4 / 12 / 8 | 23.7 / 23.1 / 23.1 | 98.3 / 95.8 / 95.8 |
| loss3cap100 | 100.0 / 100.0 / 100.0 | 5.1 / 10.3 / 10.3 | 12 / 20 / 24 | 23.7 / 23.1 / 23.1 | 98.3 / 95.8 / 95.8 |
| loss5cap100 | 100.0 / 100.0 / 100.0 | 5.1 / 8.6 / 8.6 | 7 / 15 / 15 | 23.7 / 23.1 / 23.1 | 98.3 / 95.8 / 95.8 |

Every 20 Mbps case keeps its full budget on both PR revisions (main drops to
7.4–16 Mbps). Parity takes 32–35% of the wire at 20 Mbps (main 24–25%); the
extra is reserved inside the budget, so it costs payload, not overload. This
branch's frame losses are close to 42adc9e's under the same cap: better in
five cases, worse in two (3%/20: 36.0 vs 30.9 lost/min), equal in two.

## Idle and restart

`idle_motion`: 50 Mbps path, 10 ms RTT, the encoder sends 1% of its allowance
for 5 s (with a 25 ms local sender stall in seconds 1–5), then full motion.
`idle_drop*`: review S3 (250 ms RTT, 100 → 5 Mbps at 2.5 s while idle).

| Scenario | Idle→full-rate frame ms | Queue peak 5.0–5.5 s ms | Lost | Mean budget Mbps |
|---|---|---|---|---|
| idle_motion | 3179 / 13 / 678 | 0.6 / 8.0 / 0.8 | 0 / 0 / 0 | 43.5 / 50.0 / 49.1 |
| idle_drop | – / – / – | 40.0 / 40.0 / 4.1 | 95 / 37 / 1 | 5.0 / 5.5 / 5.9 |
| idle_drop_idr | – / – / – | 40.0 / 40.0 / 10.5 | 96 / 37 / 1 | 5.0 / 5.5 / 5.6 |

The first resumed train is now constrained; the later restart step that
crosses 5 Mbps still overshoots by up to 2× the rate that just held and fills
the 40 ms FIFO once at 250 ms RTT (whole-run peak 40 ms on all three), which
costs one lost frame and its recovery round trip (51 skipped frames). That is
the price of doubling per round trip; smaller steps would make long-RTT
idle→motion many seconds slower.

## Review FIFO and mixed-loss scenarios (S5–S10)

`fifo2x`: 40 Mbps ceiling over a 20 Mbps FIFO (`_short`: 4 ms FIFO).
`step_loss`: 100 → 20 Mbps at 5 s. `loss3mixed*`: the random-loss mix above in
front of a 20 Mbps FIFO, 100 Mbps ceiling, 10/150/250 ms RTT.

| Scenario | Mean budget Mbps | Queue p95 ms | Lost | Skipped | Parity % |
|---|---|---|---|---|---|
| fifo2x | 19.3 / 20.7 / 19.8 | 9.0 / 8.6 / 7.7 | 16 / 17 / 16 | 2 / 0 / 2 | 14.8 / 14.8 / 15.4 |
| fifo2x_short | 9.0 / 9.4 / 9.4 | 3.6 / 3.1 / 3.1 | 254 / 200 / 200 | 0 / 0 / 0 | 25.7 / 30.7 / 30.7 |
| step_loss | 19.3 / 20.6 / 20.9 | 9.4 / 8.4 / 8.4 | 7 / 7 / 7 | 7 / 7 / 7 | 13.1 / 13.1 / 13.1 |
| loss3mixed | 15.4 / 20.7 / 21.2 | 7.2 / 8.2 / 8.2 | 53 / 36 / 43 | 120 / 61 / 82 | 25.5 / 31.5 / 30.1 |
| loss3mixed150 | 15.4 / 20.2 / 20.3 | 7.7 / 7.7 / 7.7 | 77 / 67 / 67 | 636 / 403 / 533 | 25.4 / 29.9 / 30.6 |
| loss3mixed250 | 7.7 / 20.4 / 20.5 | 4.6 / 7.9 / 7.7 | 122 / 81 / 83 | 1051 / 689 / 681 | 24.9 / 30.2 / 31.3 |

The 4 ms FIFO remains poor on every revision: unpaced whole frames overflow a
queue that short even below the average rate.

## Existing regression scenarios

| Scenario | Mean budget Mbps | Budget SD Mbps | Queue p95 ms | Lost | Skipped |
|---|---|---|---|---|---|
| steady20 | 19.3 / 21.1 / 21.1 | 0.7 / 0.3 / 0.3 | 9.0 / 8.8 / 8.4 | 19 / 19 / 19 | 1 / 1 / 1 |
| steady50 | 49.2 / 49.4 / 49.4 | 1.2 / 1.9 / 1.9 | 8.0 / 7.6 / 7.6 | 16 / 17 / 16 | 2 / 0 / 2 |
| steady100 | 101.6 / 103.6 / 103.6 | 2.5 / 1.6 / 1.6 | 8.8 / 8.6 / 8.6 | 16 / 17 / 16 | 2 / 0 / 2 |
| cap20rtt150 | 19.5 / 20.5 / 20.5 | 0.6 / 0.1 / 0.1 | 14.7 / 7.7 / 7.7 | 48 / 48 / 48 | 13 / 13 / 13 |
| cap20rtt250 | 19.5 / 20.5 / 20.5 | 0.2 / 0.1 / 0.1 | 10.5 / 7.7 / 7.7 | 66 / 66 / 66 | 25 / 25 / 25 |
| cap100rtt150 | 102.2 / 103.3 / 103.3 | 1.1 / 0.5 / 0.5 | 9.0 / 8.4 / 8.4 | 33 / 34 / 33 | 10 / 9 / 10 |
| cap100rtt250 | 98.7 / 102.3 / 102.3 | 1.9 / 0.3 / 0.3 | 7.9 / 8.2 / 8.2 | 70 / 71 / 69 | 20 / 19 / 22 |
| rtt150 | 50.5 / 52.1 / 52.1 | 0.5 / 0.2 / 0.2 | 8.6 / 8.6 / 8.6 | 33 / 34 / 33 | 10 / 9 / 10 |
| rtt250 | 49.2 / 52.0 / 52.0 | 1.0 / 0.2 / 0.2 | 8.0 / 8.4 / 8.4 | 69 / 71 / 68 | 22 / 20 / 23 |
| slow250 | 50.7 / 51.7 / 52.0 | 0.7 / 3.2 / 2.2 | 11.4 / 21.2 / 12.8 | 42 / 43 / 42 | 0 / 0 / 0 |
| legacy1000 | 50.2 / 45.0 / 45.6 | 0.8 / 7.2 / 6.9 | 22.7 / 17.5 / 17.5 | 238 / 188 / 156 | 0 / 17 / 30 |
| legacy_radio | 100.0 / 100.0 / 99.2 | 0.0 / 0.0 / 4.2 | 7.8 / 7.7 / 7.7 | 10 / 7 / 5 | 31 / 23 / 16 |
| cap2 | 1.1 / 2.0 / 2.0 | 0.1 / 0.1 / 0.1 | 14.6 / 7.9 / 7.9 | 20 / 20 / 20 | 1 / 1 / 1 |
| cap5 | 3.1 / 4.8 / 4.8 | 0.8 / 0.0 / 0.0 | 28.3 / 6.1 / 6.1 | 20 / 20 / 20 | 1 / 1 / 1 |
| route | 86.3 / 86.3 / 86.3 | 32.5 / 32.5 / 32.5 | 8.4 / 7.9 / 7.9 | 0 / 0 / 0 | 0 / 0 / 0 |
| lan1g | 1000.0 / 1000.0 / 1000.0 | 0.0 / 0.0 / 0.0 | 8.0 / 8.0 / 8.0 | 0 / 0 / 0 | 0 / 0 / 0 |

`legacy1000` (one-second legacy reports) improved further because one coarse
window that is damaged with >10% loss or a queue now cuts at once instead of
waiting a second window of 2× startup overload.

| Scenario (12 s) | Cut ms | Back to ceiling ms | Queue p95 ms | Lost | Skipped | Min Mbps |
|---|---|---|---|---|---|---|
| step | 60 / 60 / 60 | 1110 / 610 / 710 | 9.4 / 8.0 / 8.0 | 7 / 7 / 7 | 7 / 7 / 7 | 10.9 / 10.1 / 9.6 |
| 240fps | – / – / – | 0 / 0 / 0 | 4.2 / 4.0 / 4.0 | 0 / 0 / 0 | 0 / 0 / 0 | 200.0 / 200.0 / 200.0 |
| random2 | – / – / – | 0 / 0 / 0 | 8.0 / 7.8 / 7.8 | 0 / 0 / 0 | 0 / 0 / 0 | 100.0 / 100.0 / 100.0 |
| wifi | – / – / – | 1460 / 0 / 0 | 7.6 / 8.0 / 7.9 | 11 / 5 / 6 | 28 / 15 / 18 | 68.9 / 100.0 / 100.0 |
| rtt100 | – / – / – | 0 / 0 / 0 | 8.4 / 7.9 / 7.9 | 0 / 0 / 0 | 0 / 0 / 0 | 100.0 / 100.0 / 100.0 |
| jitter | – / – / – | 0 / 0 / 0 | 8.4 / 8.0 / 8.0 | 0 / 0 / 0 | 0 / 0 / 0 | 100.0 / 100.0 / 100.0 |
| wifi_jitter | – / – / – | 0 / 0 / 0 | 0.0 / 0.0 / 0.0 | 0 / 0 / 0 | 0 / 0 / 0 | 100.0 / 100.0 / 100.0 |
| policer5 | – / – / – | 0 / 0 / 0 | 0.0 / 0.0 / 0.0 | 1 / 1 / 1 | 3 / 3 / 3 | 100.0 / 100.0 / 100.0 |
| policer10 | – / – / – | 1110 / never / never | 0.0 / 0.0 / 0.0 | 21 / 9 / 8 | 11 / 5 / 2 | 75.4 / 60.9 / 74.4 |
| policer20 | – / – / – | never / never / never | 0.0 / 0.0 / 0.0 | 25 / 9 / 8 | 12 / 5 / 2 | 69.2 / 60.9 / 74.2 |

`policer10/20` never return to the ceiling within 12 s on either PR revision:
the confirmed policer memory (10 s) holds the climb under the policer.

## Real path: Linux ARM64 host → Mac VideoToolbox viewer

Own `serve` on `linux` port 47901, `LATCH_TEST_PATTERN=sweep`, 640×360@60,
20 Mbps requested, 25 s per run, viewer-side `LATCH_TEST_VIDEO_LOSS=3:4`
(3% of video datagrams in four-packet bursts). main and b809318 (= 42adc9e's
code) are the PR's earlier single runs; this branch was run three times.
Raw: [real-before.json](real-before.json) (main),
[real-b809318.json](real-b809318.json), `real-this-{1,2,3}.json`.

| Run | Lowest reported target kbps | fps | Frame interval p95 / p99 ms | IDRs / recovery frames per min | Recovery freeze p95 ms | Losses |
| --- | --- | --- | --- | --- | --- | --- |
| main | 1000 (host log; not reported then) | 56.1 | 18.8 / 69.6 | 2.4 / 74.2 | 102.2 | 31 |
| b809318 | 20000 | 55.8 | 19.8 / 84.9 | 4.8 / 76.5 | 133.7 | 32 |
| this, run 1 | 1191 | 56.0 | 27.0 / 75.6 | 2.4 / 71.7 | 100.0 | 30 |
| this, run 2 | 20000 | 55.9 | 24.9 / 77.4 | 2.4 / 76.5 | 85.1 | 32 |
| this, run 3 | 1000 | 56.5 | 19.0 / 68.6 | 2.4 / 69.4 | 85.6 | 29 |

The pattern sends about 0.5 Mbps, so this path is idle by the new definition
and restarts often. These runs found three restart bugs that the simulations
did not (a keyframe window looked like a loaded sender during a restart;
waiting in the restart pacer read as sender pressure; restarting streams lost
the idle jitter allowance), all fixed before the runs above. What remains: an
8 ms ping spike over the tailnet during a restart is a genuine delay signal,
ends the restart with a real cut, and the target then follows the ordinary
controller (as on main) until the next idle; 42adc9e never constrained
resumed traffic, so it kept 20 Mbps. Frame rate and recovery freeze are no
worse than either earlier revision; single runs on a shared Mac and tailnet,
not a latency claim.

## Reproduce

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
cargo test --locked -p latch-engine --test steady_transport --test transport_scenarios --test impaired --test bottleneck -- --nocapture
cargo test --locked -p latch-engine --lib congestion -- --nocapture
cargo test --locked -p latch-proto --test keyframe_recovery -- --nocapture
# An older revision through this branch's harness:
mkdir /tmp/old && git archive 42adc9e | tar -x -C /tmp/old
python3 docs/measurements/2026-11-04/transport-fec/compare.py head /tmp/old .   # "main" for 3d5d69e
(cd /tmp/old && CARGO_TARGET_DIR=/tmp/old-target cargo test -p latch-engine \
  --test steady_transport --test transport_scenarios -- --nocapture)
```

Real path: cross-build `serve` with `cargo zigbuild --locked --target
aarch64-unknown-linux-gnu.2.39 --release -p latch-engine --example serve` (the
ALSA sysroot `PKG_CONFIG_*` settings CI uses), copy it to `~/gfn-transport-fec`
on the ARM host, run it with `LATCH_TEST_PATTERN=sweep ./serve --bind <ip>
--port 47901 --telemetry`, then on the Mac
`LATCH_TEST_VIDEO_LOSS=3:4 target/release/examples/view <ip> --port 47901
--width 640 --height 360 --fps 60 --kbps 20000 --seconds 25`.

## Validation and limits

See [validation.txt](validation.txt). Not done: Windows on-device runs (the PC
was offline; the Windows build was checked with `cargo check --release` and
clippy for `x86_64-pc-windows-msvc` on the Mac), a real Wi-Fi AP, a real
competing TCP/QUIC flow, 4K motion with encoder-sized IDRs, multi-seed
simulations, and a mixed-version (old viewer ↔ new host) session; wire
compatibility rests on serde defaults and the existing variable parity field.
The wall-clock `impaired` test can see a one-sample budget dip when the Mac is
heavily loaded (CPU starvation inflates the host's pings into a delay cut);
under 11 busy-loop processes it passed 10/10 after the fixes, with one
recovered early dip.
