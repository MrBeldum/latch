# FEC-first transport, 2026-11-04

Three revisions, each cell **main / e2fad22 / this branch**: main (engine
of `ee3176a`, unchanged since `3d5d69e`), `e2fad22` (the first round of
review fixes, re-reviewed in `review-58b`), and this branch after the second
round. All budgets are **wire** budgets (parity and headers included). No
installed host or port 47850 was used. Protocol 1 and every existing UDP
layout are unchanged; the one new datagram kind (`Probe`, 16) goes only to a
viewer that asks for it, and new JSON fields default on read.

## Second review round (review-58b on e2fad22)

| Finding / case | main | e2fad22 | this branch |
| --- | --- | --- | --- |
| N1: clean idle 5 s → motion, first full-rate frame delivered (20 Mbps, 60 fps; RTT 10 / 50 / 150 / 250 ms) | 8 / 28 / 78 / 128 ms | 678 / 1315 / 2614 / 3614 ms | **8 / 28 / 78 / 128 ms** |
| N1: desktop pattern (2 s idle, 1 s motion), mean encoder target during motion, RTT 10 / 50 ms | 20.0 / 20.0 Mbps | 17.0 / 14.4 Mbps | **20.0 / 20.0 Mbps** |
| N1: sparse 0.5 Mbps stream, a forced 120 KB IDR delivered in | 21 ms | 98 ms | **21 ms** |
| N1: real Linux → Mac sweep stream (≈0.5 Mbps), host's applied encoder target at 20 / 50 Mbps | 15 / 39 Mbps median; floor dips in 4 of 8 clean runs | **≈2 / ≈2 Mbps** in 8 of 8 | **15 / 39 Mbps** in 8 of 8, no dips |
| N1: `idle_motion` (50 Mbps, 10 ms RTT) idle→full-rate frame, no fake idle queue | **13 ms** (published as 3179 ms) | 678 ms | 13 ms |
| N2: capacity 100 → 20 Mbps during idle, 250 ms RTT, 200 ms FIFO: queue p95 / lost frames | 13.1 ms / 16 | 119.0 ms / 0 | **7.7 ms / 0** |
| N2: same, → 40 Mbps | 7.7 ms / 6 | 91.1 ms / 3 | **7.7 ms / 0** |
| N3: restart state on a sparse stream | none | never ends | none (removed); probe trains stop when idle ends |
| N4: encoder target while a protected frame is owed, 1 Mbps / 240 fps (ordinary 274 kbps) | 274 kbps (no floor) | 3 kbps | **137 kbps** (bounded to half) |
| N4: same, 2 Mbps / 60 fps (ordinary 1125) and 5 Mbps / 120 fps (ordinary 2251) | no floor | 332 / 857 kbps | 562 / 1125 kbps |
| CI: `bottleneck`, a 12 fps stream after a standing 40 ms queue clears | recovers | stuck at the 1 Mbps floor (macOS runner) | back to what it delivered, 1 s after the queue clears |
| S3 (`idle_drop`: 100 → 5 Mbps during 5 s idle, 250 ms RTT): queue peak 5.0–5.5 s / lost frames | 40.0 ms / 95 | 4.1 ms / 1 | **18.4 ms / 0** |
| S3 with a 4× keyframe on resume | 40.0 ms / 96 | 10.5 ms / 1 | 24.6 ms / 0 |

The first review's fixes (S1, S2, S4, S11, `slow250`, one shared FEC policy)
still hold: the S1 and S11 unit tests pass their bounds (4.5–5.25 Mbps under
the 5 Mbps policer; 85–90 Mbps under the 90 Mbps policer with 10% random
loss), `slow250` queue p95 is 12.8 ms (bound 16), and every non-idle
simulation row below is identical to e2fad22. The loss rate test now also
needs 4 Mbps of traffic and compares means (see the real-path section).

### What changed

* **Idle keeps the encoder's target.** e2fad22 aged the encoder target after
  1 s of idle (to 1/32) and restarted by doubling per feedback horizon. That
  cost a clean path 0.7–3.6 s of shrunken frames after every pause and held
  sparse real streams near 2 Mbps. The restart state (goal, trust, doubling,
  restart exemptions) is gone: motion resumes exactly as on main.
* **Probe trains measure the path while idle.** After 500 ms of idle the
  host sends two short trains of padding back to back, 200 ms apart, then one
  a second: 6–16 full datagrams, about 3 ms of the budget. A viewer that asked
  for them (`StreamRequest.probe_trains`) times their spread, which is the
  bottleneck's rate, and reports the fastest in `Report.probe_kbps`. Two
  trains within 2.5 s both under 3/4 of the budget lower it to 90% of the
  faster one (pacing on) **before** motion resumes into a path that got
  slower; later faster trains restore the earlier budget. This protects S3,
  the N2 deep-buffer cases and the desktop capacity cases without delaying
  anything on a clean path. Older viewers get no trains, and idle is main's.
* **An underfilled stream regains what it delivered** once a standing delay
  that still delivered everything (someone else's queue) has been gone for a
  second, and waiting in the pacer is not sender pressure while pacing. This
  was the `bottleneck` CI failure: a 12 fps stream at the floor whose 81 ms
  pacer queue counted as pressure forever, and whose probes an underfilled
  encoder can never prove.
* **The protected-frame reservation is bounded** to half the ordinary
  encoder target; at very low rates the excess is one frame's burst.

## Method

Codec-free deterministic simulations (`steady_transport.rs`, 35 s, and
`transport_scenarios.rs`, 12 s): 120 fps, seed 17, the production pacer and
parity policy (on this branch also the probe trains and the viewer's train
meter), 40 ms FIFO unless noted, link loss before the FIFO. Means, SD and
queue p95 cover seconds 5–35; lost/skipped frames cover the whole run. The
idle rows no longer inject a 25 ms local queue while idle: an idle unpaced
server's local queue is about zero, and on main that fake queue alone made
idle→motion look like 3.2 s. Older revisions run **this branch's** harness
through [compare.py](compare.py), which exposes each revision's own
controller, encoder share, reservation and production FEC policy behind the
harness API (without probe trains) and turns assertions into printed
SOFTFAIL lines. Raw output: [sim-main.txt](sim-main.txt),
[sim-e2fad22.txt](sim-e2fad22.txt), [sim-this.txt](sim-this.txt).

The review-58b harness (60 fps, 50 ms reports, a simulated FIFO, its idle,
desktop, sparse, spike, overshoot and legacy scenarios) is kept in
[review58b/](review58b/) with one shim per revision; the three-way table is
[review58b.txt](review58b.txt).

## Random loss and parity cost

1/3/5% independent loss plus Gilbert–Elliott bursts (enter 0.002, exit 0.3),
1% duplication/reorder (4 ms), 0–4 ms jitter, 10 ms RTT, cap = ceiling.

| Loss / cap | Mean budget Mbps | Lost frames/min | Skipped | Parity % of wire | Wire Mbps |
|---|---|---|---|---|---|
| loss1cap20 | 19.6 / 20.0 / 20.0 | 42.9 / 29.1 / 29.1 | 88 / 61 / 61 | 24.8 / 32.2 / 32.2 | 19.7 / 18.4 / 18.4 |
| loss3cap20 | 16.6 / 20.0 / 20.0 | 58.3 / 36.0 / 36.0 | 120 / 74 / 74 | 25.3 / 34.4 / 34.4 | 17.6 / 18.4 / 18.4 |
| loss5cap20 | 19.6 / 20.0 / 20.0 | 72.0 / 32.6 / 32.6 | 156 / 69 / 69 | 24.1 / 34.8 / 34.8 | 20.2 / 18.4 / 18.4 |
| loss1cap50 | 50.0 / 50.0 / 50.0 | 22.3 / 17.1 / 17.1 | 52 / 40 / 40 | 24.4 / 25.0 / 25.0 | 50.4 / 47.9 / 47.9 |
| loss3cap50 | 50.0 / 50.0 / 50.0 | 25.7 / 27.4 / 27.4 | 60 / 64 / 64 | 24.4 / 24.4 / 24.4 | 50.4 / 47.9 / 47.9 |
| loss5cap50 | 50.0 / 50.0 / 50.0 | 22.3 / 29.1 / 29.1 | 52 / 68 / 68 | 24.4 / 24.2 / 24.2 | 50.4 / 47.9 / 47.9 |
| loss1cap100 | 100.0 / 100.0 / 100.0 | 1.7 / 3.4 / 3.4 | 4 / 8 / 8 | 23.7 / 23.1 / 23.1 | 98.3 / 95.8 / 95.8 |
| loss3cap100 | 100.0 / 100.0 / 100.0 | 5.1 / 10.3 / 10.3 | 12 / 24 / 24 | 23.7 / 23.1 / 23.1 | 98.3 / 95.8 / 95.8 |
| loss5cap100 | 100.0 / 100.0 / 100.0 | 5.1 / 8.6 / 8.6 | 7 / 15 / 15 | 23.7 / 23.1 / 23.1 | 98.3 / 95.8 / 95.8 |

Every 20 Mbps case keeps its full budget on both PR revisions (main drops to
7.4–16 Mbps). Parity takes 32–35% of the wire at 20 Mbps (main 24–25%); the
extra is reserved inside the budget, so it costs payload, not overload.

## Idle and resume

`idle_motion`: 50 Mbps path, 10 ms RTT, the encoder sends 1% of its allowance
for 5 s, then full motion. `idle_drop*`: review S3 (250 ms RTT, 100 → 5 Mbps
at 2.5 s while idle; `_idr` resumes with a 4× keyframe).

| Scenario | Idle→full-rate frame ms | Queue peak 5.0–5.5 s ms | Lost | Skipped | Mean budget Mbps |
|---|---|---|---|---|---|
| idle_motion | 13 / 678 / 13 | 8.6 / 0.8 / 8.0 | 0 / 0 / 0 | 0 / 0 / 0 | 50.0 / 49.1 / 50.0 |
| idle_drop | – / – / – | 40.0 / 4.1 / 18.4 | 95 / 1 / 0 | 728 / 51 / 0 | 5.0 / 5.9 / 5.0 |
| idle_drop_idr | – / – / – | 40.0 / 10.5 / 24.6 | 96 / 1 / 0 | 776 / 51 / 0 | 5.0 / 5.6 / 5.0 |

Main resumes `idle_drop` at 100 Mbps into a 5 Mbps link; e2fad22 restarts at
1/32 of its target and doubles; on this branch two trains measured about
5 Mbps after the drop, so the budget was 4.6 Mbps before motion started. The
18 ms peak is a train and the first frames sharing the 40 ms FIFO, without
loss.

Review-58b overshoot cases (capacity 100 → X Mbps at 2.5 s during a 5 s
idle, 120 fps), lost/skipped frames, main · e2fad22 · this branch:

| RTT / FIFO | → 8 Mbps | → 20 Mbps | → 40 Mbps | → 70 Mbps |
| --- | --- | --- | --- | --- |
| 100 ms / 40 ms | 13/18 · 3/22 · **0/0** | 12/22 · 0/0 · **0/0** | 11/22 · 13/22 · **0/0** | 9/23 · 0/0 · 8/23 |
| 250 ms / 40 ms | 22/36 · 10/42 · **0/0** | 21/40 · 17/45 · **0/0** | 20/40 · 25/37 · **0/0** | 18/43 · 10/51 · 18/43 |
| 250 ms / 200 ms | 20/60 · 0/0 · **0/0** | 16/65 · 0/0 · **0/0** | 6/69 · 3/70 · **0/0** | 0/0 · 0/0 · 0/0 |

Queue p95 in the 250 ms / 200 ms FIFO rows: 120.9 · 25.3 · 5.1, 13.1 · 119.0
· 7.7, 7.7 · 91.1 · 7.7, 7.8 · 44.4 · 7.9 ms. A drop to 70% of the budget is
above the 3/4 threshold in this 1 ms-resolution simulation (its trains read
76.8 Mbps), so it resumes as on main and the ordinary controller cuts it;
the threshold leaves room for timing noise. Desktop pattern (2 s idle, 1 s
motion) under a 20 Mbps path with a 100 Mbps ceiling, lost/skipped: 250 ms
RTT / 200 ms FIFO 16/65 · 0/0 · **0/0**; 100 ms / 40 ms 62/137 · 6/20 ·
**0/0**, mean motion target 11.6 · 13.9 · 20.4 Mbps.

## Review FIFO and mixed-loss scenarios (S5–S10)

`fifo2x`: 40 Mbps ceiling over a 20 Mbps FIFO (`_short`: 4 ms FIFO).
`step_loss`: 100 → 20 Mbps at 5 s. `loss3mixed*`: the random-loss mix above in
front of a 20 Mbps FIFO, 100 Mbps ceiling, 10/150/250 ms RTT.

| Scenario | Mean budget Mbps | Queue p95 ms | Lost | Skipped | Parity % |
|---|---|---|---|---|---|
| fifo2x | 19.3 / 19.8 / 19.8 | 9.0 / 7.7 / 7.7 | 16 / 16 / 16 | 2 / 2 / 2 | 14.8 / 15.4 / 15.4 |
| fifo2x_short | 9.0 / 9.4 / 9.4 | 3.6 / 3.1 / 3.1 | 254 / 200 / 200 | 0 / 0 / 0 | 25.7 / 30.7 / 30.7 |
| step_loss | 19.3 / 20.9 / 20.9 | 9.4 / 8.4 / 8.4 | 7 / 7 / 7 | 7 / 7 / 7 | 13.1 / 13.1 / 13.1 |
| loss3mixed | 15.4 / 21.2 / 21.2 | 7.2 / 8.2 / 8.2 | 53 / 43 / 43 | 120 / 82 / 82 | 25.5 / 30.1 / 30.1 |
| loss3mixed150 | 15.4 / 20.3 / 20.3 | 7.7 / 7.7 / 7.7 | 77 / 67 / 67 | 636 / 533 / 533 | 25.4 / 30.6 / 30.6 |
| loss3mixed250 | 7.7 / 20.5 / 20.5 | 4.6 / 7.7 / 7.7 | 122 / 83 / 83 | 1051 / 681 / 681 | 24.9 / 31.3 / 31.3 |

The 4 ms FIFO remains poor on every revision: unpaced whole frames overflow a
queue that short even below the average rate.

## Existing regression scenarios

| Scenario | Mean budget Mbps | Budget SD Mbps | Queue p95 ms | Lost | Skipped |
|---|---|---|---|---|---|
| steady20 | 19.3 / 21.1 / 21.1 | 0.7 / 0.3 / 0.3 | 9.0 / 8.4 / 8.4 | 19 / 19 / 19 | 1 / 1 / 1 |
| steady50 | 49.2 / 49.4 / 49.4 | 1.2 / 1.9 / 1.9 | 8.0 / 7.6 / 7.6 | 16 / 16 / 16 | 2 / 2 / 2 |
| steady100 | 101.6 / 103.6 / 103.6 | 2.5 / 1.6 / 1.6 | 8.8 / 8.6 / 8.6 | 16 / 16 / 16 | 2 / 2 / 2 |
| cap20rtt150 | 19.5 / 20.5 / 20.5 | 0.6 / 0.1 / 0.1 | 14.7 / 7.7 / 7.7 | 48 / 48 / 48 | 13 / 13 / 13 |
| cap20rtt250 | 19.5 / 20.5 / 20.5 | 0.2 / 0.1 / 0.1 | 10.5 / 7.7 / 7.7 | 66 / 66 / 66 | 25 / 25 / 25 |
| cap100rtt150 | 102.2 / 103.3 / 103.3 | 1.1 / 0.5 / 0.5 | 9.0 / 8.4 / 8.4 | 33 / 33 / 33 | 10 / 10 / 10 |
| cap100rtt250 | 98.7 / 102.3 / 102.3 | 1.9 / 0.3 / 0.3 | 7.9 / 8.2 / 8.2 | 70 / 69 / 69 | 20 / 22 / 22 |
| rtt150 | 50.5 / 52.1 / 52.1 | 0.5 / 0.2 / 0.2 | 8.6 / 8.6 / 8.6 | 33 / 33 / 33 | 10 / 10 / 10 |
| rtt250 | 49.2 / 52.0 / 52.0 | 1.0 / 0.2 / 0.2 | 8.0 / 8.4 / 8.4 | 69 / 68 / 68 | 22 / 23 / 23 |
| slow250 | 50.7 / 52.0 / 52.0 | 0.7 / 2.2 / 2.2 | 11.4 / 12.8 / 12.8 | 42 / 42 / 42 | 0 / 0 / 0 |
| legacy1000 | 50.2 / 45.6 / 45.6 | 0.8 / 6.9 / 6.9 | 22.7 / 17.5 / 17.5 | 238 / 156 / 156 | 0 / 30 / 30 |
| legacy_radio | 100.0 / 99.2 / 99.2 | 0.0 / 4.2 / 4.2 | 7.8 / 7.7 / 7.7 | 10 / 5 / 5 | 31 / 16 / 16 |
| cap2 | 1.1 / 2.0 / 2.0 | 0.1 / 0.1 / 0.1 | 14.6 / 7.9 / 7.9 | 20 / 20 / 20 | 1 / 1 / 1 |
| cap5 | 3.1 / 4.8 / 4.8 | 0.8 / 0.0 / 0.0 | 28.3 / 6.1 / 6.1 | 20 / 20 / 20 | 1 / 1 / 1 |
| route | 86.3 / 86.3 / 86.3 | 32.5 / 32.5 / 32.5 | 8.4 / 7.9 / 7.9 | 0 / 0 / 0 | 0 / 0 / 0 |
| lan1g | 1000.0 / 1000.0 / 1000.0 | 0.0 / 0.0 / 0.0 | 8.0 / 8.0 / 8.0 | 0 / 0 / 0 | 0 / 0 / 0 |

`legacy1000` is a synthetic shape (one-second windows with frame loss but no
`unrecovered_loss_pct`) that no shipped viewer sends; review-58b's
5.0-shaped reports (`v50_*` in [review58b.txt](review58b.txt)) are equal or
better on both PR revisions than on main.

| Scenario (12 s) | Cut ms | Back to ceiling ms | Queue p95 ms | Lost | Skipped | Min Mbps |
|---|---|---|---|---|---|---|
| step | 60 / 60 / 60 | 1110 / 710 / 710 | 9.4 / 8.0 / 8.0 | 7 / 7 / 7 | 7 / 7 / 7 | 10.9 / 9.6 / 9.6 |
| 240fps | – / – / – | 0 / 0 / 0 | 4.2 / 4.0 / 4.0 | 0 / 0 / 0 | 0 / 0 / 0 | 200.0 / 200.0 / 200.0 |
| random2 | – / – / – | 0 / 0 / 0 | 8.0 / 7.8 / 7.8 | 0 / 0 / 0 | 0 / 0 / 0 | 100.0 / 100.0 / 100.0 |
| wifi | – / – / – | 1460 / 0 / 0 | 7.6 / 7.9 / 7.9 | 11 / 6 / 6 | 28 / 18 / 18 | 68.9 / 100.0 / 100.0 |
| rtt100 | – / – / – | 0 / 0 / 0 | 8.4 / 7.9 / 7.9 | 0 / 0 / 0 | 0 / 0 / 0 | 100.0 / 100.0 / 100.0 |
| jitter | – / – / – | 0 / 0 / 0 | 8.4 / 8.0 / 8.0 | 0 / 0 / 0 | 0 / 0 / 0 | 100.0 / 100.0 / 100.0 |
| wifi_jitter | – / – / – | 0 / 0 / 0 | 0.0 / 0.0 / 0.0 | 0 / 0 / 0 | 0 / 0 / 0 | 100.0 / 100.0 / 100.0 |
| policer5 | – / – / – | 0 / 0 / 0 | 0.0 / 0.0 / 0.0 | 1 / 1 / 1 | 3 / 3 / 3 | 100.0 / 100.0 / 100.0 |
| policer10 | – / – / – | 1110 / never / never | 0.0 / 0.0 / 0.0 | 21 / 8 / 8 | 11 / 2 / 2 | 75.4 / 74.4 / 74.4 |
| policer20 | – / – / – | never / never / never | 0.0 / 0.0 / 0.0 | 25 / 8 / 8 | 12 / 2 / 2 | 69.2 / 74.2 / 74.2 |

`policer10/20` never return to the ceiling within 12 s on either PR revision:
the confirmed policer memory (10 s) holds the climb under the policer.

## Real path: Linux ARM64 host → Mac viewer

Own `serve` on `linux`, port 47966 (`LATCH_TEST_PATTERN=sweep`, a sparse
desktop-like stream of about 0.5–2 Mbps), Mac `view` at 640×360@60 for 15 s
per run: both this branch's viewer and main's (old viewers send no
`probe_trains`, so they also test compatibility), 20 and 50 Mbps requested,
clean and with viewer-side `LATCH_TEST_VIDEO_LOSS=3:4` (3% of video
datagrams, in four-packet bursts). Two rounds of eight runs per host; the
main and e2fad22 rounds ran about an hour before this branch's final rounds
on the same tailnet. "Applied" is the host's encoder target as each run's
viewer reported it each second. Raw per-run lines: [real-path.txt](real-path.txt).

| Host | Condition | Requested | Runs | Applied target, median of per-run medians (Mbps) | Runs that fell below 1 Mbps / ended there | Frame interval p95 (median of runs) ms | Worst gap ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| main | clean | 20 Mbps | 4 | 15 | 2 / 0 | 18.4 | 97 |
| main | clean | 50 Mbps | 4 | 39 | 2 / 0 | 18.2 | 157 |
| main | loss3x4 | 20 Mbps | 4 | 7 | 3 / 0 | 19.2 | 107 |
| main | loss3x4 | 50 Mbps | 4 | 16.5 | 2 / 1 | 18.8 | 135 |
| e2fad22 | clean | 20 Mbps | 4 | 2 | 1 / 1 | 19.5 | 451 |
| e2fad22 | clean | 50 Mbps | 4 | 2 | 1 / 0 | 18.8 | 431 |
| e2fad22 | loss3x4 | 20 Mbps | 4 | 0.5 | 4 / 4 | 19.5 | 479 |
| e2fad22 | loss3x4 | 50 Mbps | 4 | 0 | 3 / 3 | 18.9 | 250 |
| this | clean | 20 Mbps | 4 | 15 | 0 / 0 | 18.1 | 101 |
| this | clean | 50 Mbps | 4 | 39 | 0 / 0 | 18.4 | 95 |
| this | loss3x4 | 20 Mbps | 4 | 12 | 0 / 0 | 19.1 | 104 |
| this | loss3x4 | 50 Mbps | 4 | 33 | 0 / 0 | 18.6 | 102 |

All 48 runs exited cleanly with 739–902 frames. On a clean path this branch
holds the encoder target in every run, as main mostly does (main's
occasional dips to the floor are tailnet delay spikes) and e2fad22 never
did. Under bursty loss this branch kept its target in all eight runs. Its
first final round did not: a sparse stream's queue-free loss rate test
"confirmed" phantom 2 Mbps policers. One noisy window started the test, its
mean afterwards looked like an improvement, and the stream's content moved
its send rate. The test now needs 4 Mbps of traffic, compares means and
checks that the reduced budget (not the content) lowered the load. A stream
cut to the floor while idle or underfilled regrows by time, capped by the
last probe trains.

Mac loopback through the review relay with a 5 Mbps / 40 ms FIFO cap
([loopback.txt](loopback.txt)), 50 Mbps requested: this host with this
viewer reports a 4 Mbps budget from the first second of a still screen (the
probe trains measured the cap), and holds 50 Mbps without the cap. With
main's viewer (no trains) it holds 50 Mbps either way, as main does; e2fad22
falls to 2–6 Mbps in all four cases.

## Reproduce

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
cargo test --locked -p latch-engine --test steady_transport --test transport_scenarios --test impaired --test bottleneck -- --nocapture
cargo test --locked -p latch-engine --lib congestion -- --nocapture
cargo test --locked -p latch-proto --test keyframe_recovery -- --nocapture
# An older revision through this branch's harness:
mkdir /tmp/old && git archive e2fad22 | tar -x -C /tmp/old   # or origin/main with "main"
python3 docs/measurements/2026-11-04/transport-fec/compare.py e2fad22 /tmp/old .
(cd /tmp/old && CARGO_TARGET_DIR=/tmp/old-target cargo test --release -p latch-engine \
  --test steady_transport --test transport_scenarios -- --nocapture)
# The review-58b harness, in any of the three checkouts:
cp docs/measurements/2026-11-04/transport-fec/review58b/{body.rs,scenarios.rs,this.rs} crates/engine/tests/
cargo test --release -p latch-engine --test this -- --nocapture   # main.rs / e2fad22.rs in those trees
```

Real path: cross-build `serve` with `cargo zigbuild --locked --target
aarch64-unknown-linux-gnu.2.39 --release -p latch-engine --example serve`
(`PKG_CONFIG_ALLOW_CROSS=1`, `PKG_CONFIG_SYSROOT_DIR` and `PKG_CONFIG_LIBDIR`
pointing at the ALSA sysroot), copy it to `~/gfn-transport-fec` on the ARM
host, run `LATCH_TEST_PATTERN=sweep ./serve --bind <ip> --port 47966`, then on
the Mac `[LATCH_TEST_VIDEO_LOSS=3:4] target/release/examples/view <ip> --port
47966 --width 640 --height 360 --fps 60 --kbps 20000 --seconds 15`.

## Validation and limits

See [validation.txt](validation.txt). Not done: Windows on-device runs (the
PC was offline; Windows was checked with clippy and a release `cargo check`
for `x86_64-pc-windows-msvc` on the Mac), a real Wi-Fi AP, a real competing
TCP/QUIC flow, 4K motion with encoder-sized IDRs, multi-seed simulations,
and a real capacity drop during idle on a real path (the loopback relay
measures a fixed cap; the drop itself is simulated). Probe trains on real
Wi-Fi can read slow or fast; only two trains both under 3/4 of the budget
act, a single fresh pair at that, and later faster trains undo it. A drop
to 70% of the budget is not acted on in the simulation (above the 3/4
threshold at 1 ms resolution). Lossy real-path runs are 16 per host on one
shared tailnet, not a statistical result.

