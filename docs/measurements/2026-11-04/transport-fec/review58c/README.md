# Idle train confidence: C1/C2 follow-up, 2026-11-04

Compared exact main **5f8f45f**, PR head **54dcbc5**, and the implementation
**abbe664** (the next commit only records these measurements). These are codec-free model
measurements unless explicitly labelled live. The review's `jitter.rs`,
`order.rs`, `meter.rs`, `safety.rs` and shared scenario harness were reused;
portable copies and raw results are beside this report. No deployed host
or port 47850 was used. Own live servers used **47968**; Windows was offline.

## Change

The viewer tracks sequence positions and deduplicates indexes, including
late duplicates after completion and delayed tails from earlier trains.
Only an ordered contiguous prefix with at least four unique packets can
measure capacity; a lost tail is allowed. Interior loss or reordering
invalidates the sample. Count/length changes cannot inflate its byte count.

Capacity uses the median of overlapping eight-position packet pairs
(shortened to leave at least two pairs on short trains), rather than total
train span. Minimum pair dispersion must exceed twice the observed timing
variation. Dominant receiver stalls are rejected. Equal arrival timestamps
allow up to 1 ms of batching noise across the wider pairs; wholly batched
trains give only a high timing bound. Userspace receive timestamps and
batching still limit accuracy; this does not certify every short train.

A downward encoder decision requires two recent slow samples **and**
independent delay or lossy-delivery evidence. Each host train is followed
by an ordinary Ping; its echo times the FIFO behind the burst, separately
from the ordinary lowest-window RTT. Its excess over baseline must exceed
max(1 ms, 3× learned ping jitter). This supplies evidence before motion on
a newly slow FIFO without interpreting distributed arrival jitter as capacity.
No new wire fields are needed, and the train plus trailing ping is sent only
to viewers opting into `probe_trains`.

Without that corroboration, the encoder target stays unchanged. Resume
pacing uses 105% of the recent measured rate, or 1/20 of the retained budget
when unknown (at least the floor). Delivery filling that rate without
loss/delay validates 25% pace increases per feedback horizon; a fresh fast
train removes the limit. Pacer-created local queue is not capacity evidence.

## Main vs previous head vs fix

| Case | main 5f8f45f | 54dcbc5 | fix |
|---|---:|---:|---:|
| C1: sparse 0.5 Mbps, loss-free 100 Mbps path/ceiling, 0–4 ms jitter, 35 s: mean encoder budget | 100 Mbps | 16.933 Mbps | **100 Mbps** |
| C1: independent 100 Mbps / 16-packet meter: false rates below 75 Mbps, 10,000 trains | no trains | 9,705 | **0 (all unknown)** |
| C2: alternating interior loss, 100 Mbps train | no trains | 53.333 Mbps | **unknown** |
| C2: indexes 5–8 lost | no trains | 73.333 Mbps | **unknown** |
| C2: duplicate indexes | no trains | 200 Mbps | **100 Mbps** |
| C2: reordered tail | no trains | 100 Mbps | **unknown** |
| Two synthetic 53.333 Mbps reports, no independent congestion: target at 1.5–5.5 s | n/a | 51.733 Mbps | **100 Mbps** |
| N1: clean resume RTT 10/50/150/250 ms | 8/28/78/128 ms | 8/28/78/128 ms | **8/28/78/128 ms** |
| N1: sparse 120 KB IDR, RTT20 | 21 ms | 21 ms | **21 ms** |
| N2: idle 100→20 / 40 Mbps, RTT250, 200 ms FIFO: queue p95 | 13.1/7.7 ms | 7.7/7.7 ms | **7.7/7.7 ms** |
| N2: same 20 / 40 Mbps runs, lost/skipped | 16/65; 6/69 | 0/0; 0/0 | **0/0; 0/0** |

C1 has 0 lost/skipped video frames and queue p95 0.2 ms on all three
revisions; lack of video damage does not excuse the old target reduction.
Mean budget is the harness measurement interval (seconds 5–35), not a
throughput/capacity measurement. The final target alone hides the old
regression: its last compressed train restores 100 Mbps after the prolonged cut.

## S1–S11 and N1–N4

All were rerun. `this-scenarios.txt`, `54-scenarios.txt` and
`main-scenarios.txt` retain the shared cases and original suite results.
`main-idle-drop.txt` additionally runs the aligned 35-second 100→5 Mbps
shared-harness case (main loses/skips 72/860). Different earlier 15-second
runs and the checked-in steady harness have different startup/frame rules;
do not combine their counts.

- **S1:** underfilled 10 Mbps offer / 5 Mbps policer / 100 Mbps ceiling:
  settles near 4.85 Mbps with loss-free tail; bounded re-test remains.
- **S2:** production `FecPolicy` stays shared. `slow250` queue p95 12.845 ms;
  3% / 20 Mbps parity 6.326 Mbps, wire 18.411 Mbps, 36 lost frames/min.
- **S3:** checked-in `steady_transport` `idle_drop` and `idle_drop_idr`:
  **0/0 lost/skipped** on both previous head and fix. Final fix first-500-ms
  queue peaks **18.586 / 24.576 ms**, versus 18.432 / 24.576 ms on 54dcbc5;
  permanent assertions require ≤1 lost, no skips, and peak <30 ms.
- **S4 / N4:** protected targets remain bounded to half ordinary:
  5 Mbps/120 fps 2251→1125 kbps; 2 Mbps/60 1125→562; 1 Mbps/240 274→137.
  Packetized protected equivalent rates remain 6.139/3.067/1.718 Mbps:
  the explicit protected-frame burst allowance remains, not a strict
  per-frame wire guarantee.
- **N1:** sparse target and clean resume permanent regression passes;
  repeating desktop and sparse IDR cases preserve their clean target.
- **N2:** 8/20/40 Mbps idle drops at RTT100/250, FIFO40/200 lose/skip 0/0.
  No old restart ladder returns. **N3:** no persistent encoder restart goal.
- **S11:** 90 Mbps policer + 10% random loss ends 89.010 Mbps after one
  bounded re-test, without persistent 100 Mbps overshoot.

S5–S10 are unchanged from 54dcbc5 (same production FEC, seed 17):

| Case | Mean budget Mbps | Queue p95 ms | Lost/skipped |
|---|---:|---:|---:|
| S5 fifo2x | 19.782 | 7.680 | 16/2 |
| S6 fifo2x_short | 9.423 | 3.072 | 200/0 |
| S7 step_loss | 20.948 | 8.384 | 7/7 |
| S8 loss3mixed | 21.199 | 8.192 | 43/82 |
| S9 loss3mixed150 | 20.291 | 7.680 | 67/533 |
| S10 loss3mixed250 | 20.462 | 7.680 | 83/681 |

The extra review case 100→30 Mbps at RTT250/FIFO40 loses/skips **0/0**.
The documented 100→70 Mbps deadband remains: **18/43** lost/skipped and
40 ms queue peak. Small FIFOs still lose frames; rejected trains cannot
prove a safe low path rate. Unknown pacing can delay large resumed frames
and meet the 100 ms assembly timeout; clean 8–128 ms resume is not a promise
for unusable probes. Neither train dispersion nor its trailing echo proves
sustained service through a large token bucket or competing-flow fairness.

## Probe accounting

Probe datagrams bypass the video pacer, `session.bytes`, and ordinary viewer
throughput counters: **outside the wire budget**. Each has 1,200 UDP bytes
plus 80 modeled wire bytes, 6–16 per train: **7,680–20,480 B bursts**,
up to 327.68 kbps in the first idle second (two trains), then ≤163.84 kbps.
Each trailing Ping adds 96 modeled wire bytes. These allowances are now
explicit in ENGINE.md and PROTOCOL.md.

Trains are skipped below **8 Mbps retained budget**, replacing the 4 Mbps
gate: even the smallest 6-packet burst costs 7.68 ms at 8 Mbps; a full
16-packet burst would occupy 32.768 ms of a 40 ms FIFO at 5 Mbps. Retained
budget can be stale, so this gate cannot eliminate burst loss after a drop.
The review's 4 ms FIFO safety test remains in `safety.txt`: at 5 Mbps only
one datagram fits and the meter correctly reports unknown.

## Reproduce

Use PATH=$HOME/.cargo/bin:$PATH and TMPDIR=/tmp. In this directory:
`rustc jitter.rs -o /tmp/probe-jitter && /tmp/probe-jitter`, likewise
`meter.rs`, `order.rs`, and `rustc -A dead_code safety.rs`.
Run `python3 run.py` for the preserved review scenarios (including C1/C2,
S1/S11 and protected overhead); it temporarily creates Cargo test targets
and removes them even on failure. `R58B=<case>` filters N1–N4. The permanent
`probe_transport` and `steady_transport` cover C1 and S3.
The wider-pair batching regression and sequence/jitter cases are proto
unit tests. Historic main/head shims use the original body without echoes;
this branch's body adds only the trailing Ping/return-half-RTT model.

## Live compatibility and Linux→Mac

Sweep pattern, 640×360 at 60 fps, 20 Mbps request, 12 seconds per session,
through the review's counting TCP/UDP relay. All **13 sessions exited 0**.
Main↔fix worked on both Mac and Linux hosts; every session with an old main
viewer saw **zero Probe datagrams**. Same-version baselines for main and
54dcbc5 were rebuilt on the Mac from the exact extracted sources; their
previous exact-revision ARM64 builds were reused for Linux. The fix was
rebuilt for both platforms after the median-pair adjustment.

| Host/viewer (this = fix) | Frame intervals | Mean fps | Interval p95 ms | Probes |
|---|---:|---:|---:|---:|
| mac-main-main | 722 | 60.01 | 18.56 | 0 |
| mac-54-54 | 723 | 60.01 | 18.49 | 78 |
| mac-this-main | 721 | 60.01 | 18.37 | 0 |
| mac-main-this | 720 | 59.85 | 18.38 | 0 |
| mac-this-this | 691 | 57.21 | 28.27 | 78 |
| linux-main-main | 720 | 60.04 | 17.88 | 0 |
| linux-54-54 | 721 | 60.02 | 18.11 | 72 |
| linux-this-main | 719 | 60.01 | 18.36 | 0 |
| linux-main-this | 721 | 60.10 | 38.18 | 0 |
| linux-this-this | 689 | 57.45 | 29.78 | 66 |
| mac-this-this-isolated | 714 | 59.21 | 24.93 | 78 |
| linux-main-this-isolated | 721 | 60.02 | 18.15 | 0 |
| linux-this-this-isolated | 634 | 52.82 | 29.26 | 72 |

These short live sessions are functionality checks on a shared tailnet,
not controlled throughput benchmarks. The first Mac matrix overlapped
platform tests; isolated repeats followed their completion. The final
Linux same-version repeat ran at 52.82 fps with two recovery acknowledgments,
despite keeping the ordinary encoder target near 15.75 Mbps (15.19 Mbps
for three samples). That run does not establish new throughput headroom.

A path disturbance produced ordinary standing-delay cuts on **both main
and fix**: Linux main→fix sampled 0.797–15.943 Mbps target; Linux fix→fix
briefly fell to 0.428 Mbps, then restored 15.752 Mbps by the fourth sample.
The latter host log shows 41–53 ms ordinary host RTT during the initial
cuts, versus ~12 ms after the disturbance; these were not idle-dispersion
capacity cuts. The main→fix isolated repeat held 15.943 Mbps throughout.
Raw viewer JSON and host decisions for the disturbed runs are retained.
No claim is made that these real paths were lossless or had bounded jitter.
All own servers were terminated; Linux scratch binaries and directory were
removed, and no listener remained on 47968 on either host.

## Final validation

- `cargo fmt --all -- --check`: pass.
- CI clippy `cargo clippy --locked --workspace --all-targets -- -D warnings`:
  pass on Mac, x86_64-pc-windows-msvc, and aarch64-unknown-linux-gnu
  (via `source /tmp/latch-bench/armenv.sh`).
- `cargo check --locked --release --workspace --target x86_64-pc-windows-msvc`: pass.
- `cargo test --locked --workspace --all-targets`: **778 passing executions,
  0 failures, 37 ignored** (sum of Cargo result groups, including helpers);
  workspace doctests separately pass. The all-targets run includes all
  ordinary workspace tests plus examples.
- Release four transport suites, `probe_transport`, shared C1/C2 + N1–N4
  scenarios, direct S1/S11 and protected overhead: pass. The same original
  four transport suites and proto keyframe recovery were rerun on exact
  main and 54dcbc5 and pass.
- Release proto unit tests and `keyframe_recovery` (3/3): pass.
- Native audit: pass; Mac release serve/view and ARM64 zigbuild serve: pass.

An initial release `impaired` run during concurrent compilation failed its
random-loss budget assertion (1.882–2.990 Mbps tail). It then passed in the
full workspace run and in subsequent full release-suite runs; the final
workspace rerun also passes. This is recorded rather than hidden; no test
assertion was weakened. Slow Mac VideoToolbox tests completed normally.

Not verified: Windows execution/release linking or receive timing (PC
offline), a physical Wi-Fi AP with imposed capacity changes, a real
token-bucket policer, competing TCP/QUIC fairness, or real encoder-sized
IDR quality/reconfiguration costs. These are functional and simulated
transport results, not a hardware/game-streaming quality certification.
