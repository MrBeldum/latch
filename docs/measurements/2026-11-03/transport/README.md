# Transport / rate control — 2026-11-03

Baseline: `9fe0ae9`. Workstream B, branch `gfn/transport`. Codec-free tests use
production Packetizer, ReedSolomon, wire parser and Reassembler; they exclude
capture, encoders, decoders, rendering and audio. FEC is on (10% initially).
No installed host was replaced or stopped. All manual servers used UDP 47881.

## Review fixes (#43 independent review)

Commits `533e289` (finding 1) and `041711a` (findings 2–6), measured against
the reviewed head **16ac72a**. Raw logs are in [review/](review/).

### Steady caps, long RTT, slow feedback, low caps, route change

[`steady_transport`](../../../../crates/engine/tests/steady_transport.rs) is the
review's paced harness ported into the engine tests: 35 simulated seconds,
production Packetizer/FEC/Reassembler and Pacer, 120 fps, 40 ms FIFO, 80 bytes
per packet overhead, seed 17; mean, population SD and queue p95 cover the last
30 s, and frame counts cover all 35 s. At 16ac72a it reproduces the review's
table digit for digit. "main" is 9fe0ae9's controller in the same harness
(`steady-main-controller-9fe0ae9.txt`). Budget values are Mbps of wire budget,
which counts headers and parity, so they can sit slightly above the link cap
while encoder output stays below it.

| Scenario | Cap / ceiling Mbps | RTT ms | Report / feedback delay ms | Mean budget | Budget SD (main) | Queue p95 ms | Frames lost / skipped | Final budget |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| steady20 | 20 / 100 | 10 | 50 / 10 | 16.34 → **19.29** | 3.56 → **0.73** (1.60) | 17.0 → **9.0** | 19/1 → **19/1** | 16.98 → **19.33** |
| steady50 | 50 / 100 | 10 | 50 / 10 | 41.72 → **49.21** | 8.29 → **1.21** (3.52) | 12.0 → **8.0** | 22/0 → **16/2** | 32.77 → **48.67** |
| steady100 | 100 / 200 | 10 | 50 / 10 | 91.96 → **101.64** | 11.91 → **2.50** (7.07) | 10.5 → **8.8** | 22/2 → **16/2** | 91.12 → **107.23** |
| cap20rtt150 | 20 / 100 | 150 | 50 / 150 | 15.90 → **19.53** | 5.21 → **0.58** (1.55) | 36.6 → **14.7** | 154/687 → **48/13** | 11.58 → **20.22** |
| cap20rtt250 | 20 / 100 | 250 | 50 / 250 | 16.76 → **19.49** | 6.34 → **0.23** (1.57) | 37.8 → **10.5** | 422/843 → **66/25** | 19.99 → **19.42** |
| rtt150 | 50 / 100 | 150 | 50 / 150 | 42.61 → **50.49** | 15.47 → **0.54** (3.24) | 36.8 → **8.6** | 393/786 → **33/10** | 33.62 → **50.47** |
| rtt250 | 50 / 100 | 250 | 50 / 250 | 45.11 → **49.23** | 17.43 → **0.97** (4.51) | 37.4 → **8.0** | 565/929 → **69/22** | 46.62 → **48.47** |
| cap100rtt150 | 100 / 200 | 150 | 50 / 150 | 80.10 → **102.17** | 30.16 → **1.13** (8.21) | 34.9 → **9.0** | 235/798 → **33/10** | 66.20 → **101.47** |
| cap100rtt250 | 100 / 200 | 250 | 50 / 250 | 90.08 → **98.68** | 36.26 → **1.95** (8.52) | 37.9 → **7.9** | 572/861 → **76/15** | 55.44 → **97.14** |
| slow250 | 50 / 100 | 10 | 250 / 125 | 45.29 → **50.71** | 13.06 → **0.66** (4.02) | 38.8 → **11.4** | 621/168 → **42/0** | 38.18 → **50.34** |
| legacy1000 | 50 / 100 | 10 | 1000 / 10 | 45.78 → **50.23** | 13.69 → **0.77** (5.99) | 38.9 → **22.7** | 718/0 → **238/0** | 32.10 → **51.01** |
| legacy_radio | 100 / 100, 5% loss | 10 | 1000 / 10 | 12.75 → **100.00** | 6.79 → **0.00** (7.42) | 1.0 → **7.8** | 13/40 → **10/31** | 10.00 → **100.00** |
| cap2 | 2 / 100 | 10 | 50 / 10 | 10.00 → **1.08** | 0.00 → **0.11** (0.00) | 38.4 → **14.6** | 4184/0 → **20/1** | 10.00 → **1.00** |
| cap5 | 5 / 100 | 10 | 50 / 10 | 10.00 → **3.06** | 0.00 → **0.83** (0.00) | 39.2 → **28.3** | 4184/0 → **20/1** | 10.00 → **2.94** |
| route | 100 / 100 | 10 → 100 | 50 / 10 | 10.48 → **86.30** | 5.90 → **32.46** (12.77) | 0.9 → **8.4** | 0/0 → **0/0** | 10.00 → **100.00** |
| lan1g | 1000 / 1000 | 1 | 50 / 1 | 1000.00 → **1000.00** | 0.00 → **0.00** (25.42) | 8.0 → **8.0** | 0/0 → **0/0** | 1000.00 → **1000.00** |

- Steady-cap SD is at or under main's in every row: 0.73/1.21/2.50 Mbps at
  20/50/100 Mbps, against 1.60/3.52/7.07 for main and 3.56/8.29/11.91 for
  16ac72a. The PR's queue and loss gains over main hold: queue p95 ≤ 11.4 ms
  for 50 ms and 5.1-cadence reports (main 10.6–39.9 ms).
- Long RTT no longer fills the FIFO: at 150/250 ms, queue p95 is 7.9–14.7 ms
  (was 34.9–37.9).
- After the fix, every lost or skipped frame in every row falls in the first
  5 s: the start at the ceiling, before the first feedback arrives. None falls
  in any 30 s measurement window (checked by sampling the counters at 5 s).
- `legacy1000` (one-second reports, no repaired-loss field) still reaches
  22.7 ms queue p95, and its startup overload loses 238 frames: one-second
  feedback cannot react inside a 40 ms FIFO.
- `cap2`/`cap5`: the fixed 1 Mbps floor lets a 100 Mbps request adapt, and
  frames are delivered (20 lost at startup). Mean budget sits under the cap
  because at 120 fps per-packet overhead and parity make the offered rate
  exceed the budget. `cap5` keeps a 28 ms queue p95: at that rate one frame's
  burst is a large share of the 40 ms FIFO.
- `route`: a lossless 10→100 ms RTT step first cuts to the floor, then rebases
  after 2 s at under a quarter of the load and regains the ceiling with an
  empty queue (final 100 Mbps). The large SD is that transient.
  `route_rebase_requires_lossless_delay_unresponsive_to_load_reduction` checks
  that loss, or a falling RTT from a draining FIFO, keeps the old minimum.
- `legacy_radio`: 5% FEC-repaired loss from a viewer without
  `unrecovered_loss_pct` now keeps the 100 Mbps ceiling. Lost frames still
  cut, so policer detection for those viewers is unchanged (`policer*` rows below).

### Recovery cadence and the 100→20→100 step

Fake clock, queue-only cuts to 36 Mbps at 10 ms base RTT, then clean reports
([review/recovery.txt](review/recovery.txt)):

| Report interval | Time to double before → after | Time to 100 Mbps after |
| ---: | ---: | ---: |
| 50 ms | 600 → 600 ms | 750 ms |
| 250 ms (5.1 viewers) | 1,500 → 500 ms | 750 ms |
| 1,000 ms | 6,000 → 1,000 ms | 1,000 ms |

Growth integrates the real time between reports. Reports more than 2 s apart
grant none: the next report waits for fresh feedback (tested).

`transport_scenarios` (12 s, [before](review/scenarios-before-16ac72a.txt) /
[after](review/scenarios-after.txt)): the 100→20 Mbps step at 10 ms RTT still
first crosses in **60 ms**. Cuts during the 20 Mbps phase drop 16 → 10,
queue p95 is 10.60 → 9.38 ms, lost frames stay at 7, and the ceiling returns in
910 → 1,110 ms once capacity is back (requirement ~1–2 s). Wi-Fi, jitter,
240 fps, RTT 100 and the policer rows are unchanged within three frames.

### Windows batched receive (finding 1)

`batch_receive_rearms_after_a_drained_burst` (Windows-only) passes on the RX
6600 PC at 041711a and fails with `missed burst 1` when the `try_io` wrapper
is removed. End-to-end on the PC's loopback, 1280×720 60 fps sweep, 20 Mbps,
40 key presses, interleaved, `view` with the fix reverted ("before") vs head
([review/windows-mio-e2e.txt](review/windows-mio-e2e.txt)):

| Run | Key→picture p50 / p95 ms | Frame interval p50 / p95 ms |
| --- | ---: | ---: |
| before 1 | 23.9 / 51.6 | 27.7 / 30.6 |
| after 1 | 16.3 / 17.6 | 16.6 / 18.2 |
| before 2 | 24.4 / 52.7 | 27.4 / 30.7 |
| after 2 | 16.5 / 17.1 | 16.6 / 17.6 |

### Compatibility

Mac loopback, noise pattern, 640×360 60 fps, 10 s each, port 47881, main 5.1
binaries from 9fe0ae9 ([review/compat.txt](review/compat.txt)):

| Pair | 20 Mbps | 50 Mbps |
| --- | --- | --- |
| 5.1 host → new viewer | 59.5 fps, exit 0, 0 send errors | 60.1 fps, exit 0, 0 send errors |
| new host → 5.1 viewer (250 ms reports) | 59.8 fps, exit 0, 0 send errors | 59.5 fps, exit 0, 0 send errors |
| new ↔ new | 59.4 fps, exit 0 | 59.5 fps, exit 0 |

No pair cut its applied bitrate during the run. No wire change.

### Reproduction

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
cargo test --release -p latch-engine --test steady_transport transport_scenarios -- --nocapture --exact
cargo test --release -p latch-engine --test transport_scenarios -- --nocapture
cargo test --release -p latch-engine --lib congestion::tests::recovery -- --nocapture
# Before: a worktree at 16ac72a with this steady_transport.rs, assertions removed;
# main: the same with 9fe0ae9's crates/engine/src/congestion.rs.
# Windows (C:\Users\Daniel\dev\latch-gfn-b2):
cargo test --release -p latch-stream --lib batch_receive_rearms
```

Not covered: competing TCP flows (no TCP model; needs a netem bottleneck),
multi-minute runs, multiple seeds, real encoder IDR spikes, and Windows URO
coalescing (still zero coalesced datagrams on this NIC).

## Follow-up: policers and UDP offloads

Follow-up commits `82dd14d` and `2e7fbbb` build on `b8be885`. Tables below
are medians of three interleaved five-second runs per mode, using the same
follow-up binary with `LATCH_UDP_OFFLOAD=0` (before) or `1` (after).
They isolate offloads; they are **not** main-versus-branch CPU comparisons.
CPU µs/Mbit uses process user+system CPU and delivered media for receivers,
successfully sent media for senders. It includes packetization/FEC/reassembly.

| Loopback, 300 Mbps, 120 fps | CPU µs/Mbit before | After |
| --- | ---: | ---: |
| Linux ARM64 sender (final repeat) | 582.36 | 168.19 |
| Linux ARM64 receiver (final repeat) | 319.25 | 89.30 |
| Windows x64 sender (final repeat) | 833.33 | 482.38 |
| Windows x64 receiver (final repeat) | 375.00 | 281.25 |

The retained initial loopback runs delivered 600/600 frames with no send errors.
The final Windows repeat delivered 600/600 except enabled run 3, which sent and
delivered 596 frames (297.451 Mbps sent, 296.295 Mbps received); no send errors.
The final repeat medians supersede the initial Windows medians, which were
614.58→281.25 sender and 291.67→239.58 receiver. Both sets are retained. The final Linux repeat delivered 600/600 throughout;
its initial medians were 593.39→148.55 sender and 346.24→71.54 receiver. Linux is the
shared Oracle A1; Windows is the RX 6600 PC. Windows process CPU accounting is
coarse over five seconds and individual runs vary substantially. Raw logs:
[b2/linux-loopback.txt](b2/linux-loopback.txt),
[b2/linux-loopback-repeat.txt](b2/linux-loopback-repeat.txt),
[b2/windows-loopback.txt](b2/windows-loopback.txt),
[b2/windows-loopback-repeat.txt](b2/windows-loopback-repeat.txt).

| Tailscale, 50 Mbps offered, 120 fps | CPU µs/Mbit before | After |
| --- | ---: | ---: |
| Linux sender | 611.22 | 262.37 |
| Linux receiver | 719.20 | 654.70 |
| Windows sender (offered media denominator) | 375.00 | 187.50 |
| Windows receiver (delivered media denominator) | 688.08 | 1584.52 |

[b2/linux-tailnet.txt](b2/linux-tailnet.txt) records Linux GRO coalescing
25,755–26,798 datagrams per enabled receive run. Linux→Mac delivered 590–600
frames without offload and 598–600 enabled; Mac→Linux delivered 600 throughout.
[b2/windows-tailnet.txt](b2/windows-tailnet.txt) is a saturated, lossy path:
Windows→Mac median delivered 20.003→20.291 Mbps (240→244 frames),
Mac→Windows 22.073→21.763 Mbps (224→217 frames). Windows URO coalesced **zero**
datagrams in both loopback and tailnet logs. USO sends succeeded (6000 loopback
batches/run, 1800 tailnet); URO is implemented and its option/receive path runs,
but hardware coalescing and a URO CPU improvement are unverified. Do not interpret
these Windows tailnet numbers as a throughput or receive CPU win.

Senders segment equal-size runs, permitting a short final datagram, without
changing the wire. Linux falls back to sendmmsg/recvmmsg; Windows to scalar UDP.
Receive truncation is discarded. `LATCH_UDP_OFFLOAD=0` provides the comparison
and an operational escape hatch. Per-message segmentation avoids changing the
shared socket's audio/input datagrams. Darwin send remains scalar.

### Feedback and compatibility

[b2/scenarios-before.txt](b2/scenarios-before.txt) and
[b2/scenarios-after.txt](b2/scenarios-after.txt) compare **b8be885** to the follow-up,
not the original main baseline below. With a token bucket and zero queue growth,
20% overshoot lost 667→22 frames; minimum budget 100000→69246 kbps.
10% overshoot lost 158→22 frames. The 5% case loses mostly parity, so it does
not require a rate cut. Sustained unrecovered loss or repeated lost frames now
cuts regardless of raw loss; repaired 2% random loss leaves the ceiling intact.
5–15 ms ping jitter with no capacity limit previously falsely cut to 78643 kbps;
the follow-up stays at 100000 kbps throughout.

The 100→20 Mbps step at 10 ms RTT first reaches ≤20 Mbps in **160→60 ms**;
queue p95 11.23→10.60 ms, lost frames 19→7, recovery 960→910 ms.
This measures first crossing, not permanent settlement. Subsequent capacity
probing still caused **16 cuts during 2.5–8 seconds** in the final regression.
The ≤60 ms target is met only for first crossing; oscillation-free convergence
remains unmet. Holding a remembered queue capacity would need separate route-change
and jitter validation before trading away the current fast recovery.
Tests now enforce the 60-ms first-crossing bound. Simulations still exclude
sender pacing, codec overshoot and hardware IDR spikes.

[b2/compat.txt](b2/compat.txt) retains ten-second 720p60 H.264 runs for main
`9fe0ae9` serve ↔ new view and new serve ↔ main view, plus both matching pairs.
All connected and decoded around 60 fps with zero reported loss. Final repeat
results are in [b2/compat-repeat.txt](b2/compat-repeat.txt). The repeat connected
and decoded in both directions with zero reported loss; the synthetic sweep
was below the requested 20 Mbps, so this is not a saturation test.
Final [validation](b2/validation-final.txt): fmt, CI clippy, workspace all-target
tests (603 passed, 25 ignored), native audit, Linux proto tests and Windows
release proto build/tests plus engine/stream check passed. Windows source is
`2e7fbbb`; subsequent commits change only tests/docs. No `LatchGfnB*` tasks
remain; installed Windows host still listens on 47850. No display or driver
settings were changed during this continuation.

### Datagram size and reproduction

`crates/client/src/path.rs`'s “Direct” is direct **Tailscale**, not a separate
LAN transport; PeerRelay and DERP also traverse Tailscale. Discovery uses peer
Tailscale addresses (`Node::ipv4` in `crates/core/src/tailscale.rs`), and the
production host authorizes nonlocal clients through Tailscale whois. Loopback
test examples bypass authorization, but do not establish a supported non-Tailscale
remote connection. Both measured tailnet interfaces have MTU 1280. Retain 1200-byte
datagrams; larger-datagram negotiation/probing is deferred until a supported
non-Tailscale path exists. No wire format or capability change is needed for
GSO/USO or GRO/URO.

```sh
cargo build --locked --release -p latch-proto --example transport
# Separate shells, repeat each mode three times, alternate 0 and 1:
LATCH_UDP_OFFLOAD=0 target/release/examples/transport receive 127.0.0.1:47881
LATCH_UDP_OFFLOAD=0 target/release/examples/transport send 127.0.0.1:47881 300 5 120
# Linux: cross-build as below, scp to ~/gfn-b2/transport, run there.
# Windows: C:\Users\Daniel\dev\latch-gfn-b2, cargo build as above;
# set $env:LATCH_UDP_OFFLOAD='0' (then '1') in both PowerShell processes.
# Tailnet: replace loopback destination with peer Tailscale IP; 50 Mbps.
cargo test --locked --release -p latch-engine --test transport_scenarios -- --nocapture
```

The remaining sections preserve the original b8be885 measurements and limits.

## Result and limits

Mac loopback sustained **1.288 Gbps media / 153k packets/s** in the five-second
unlimited run with zero missing frames or send errors. At 300 Mbps / 120 fps,
sender CPU was 27.45% of one core and receiver CPU 17.60%, all 600 frames delivered.
This meets the short-run loopback headroom target, not a long-duration guarantee.
The baseline already exceeded 1 Gbps; these results do **not** show a Mac send
throughput improvement. The change improves feedback response and adds Linux
batch send/receive and Darwin batch receive.

The real Mac↔Oracle A1 Tailscale path sustained 50 Mbps cleanly in its measurement
window, but dropped frames at 100 Mbps and badly at 300 Mbps. No claim of
300 Mbps 4K120 over this path is possible from these runs. Both CPUs have room
at 300 Mbps offered load; the path, Tailscale, shared-machine load and frame
burstiness are not separated by this benchmark. Path saturation results are
reported as received media, never offered bytes presented as delivered traffic.

## Loopback: measured before → after

[loopback.txt](loopback.txt), M3 Pro, 120 fps, 5 seconds each, before/after
interleaved per offered rate. CPU is getrusage user+system time divided by elapsed
wall time; 100% means one core. CPU µs/Mbit divides CPU time by media bits.

| Case | Before | After |
| --- | ---: | ---: |
| 300 Mbps: delivered media Mbps | 299.798 | 299.763 |
| 300 Mbps: received packets/s | 35,616 | 35,612 |
| 300 Mbps: sender CPU / receiver CPU | 26.78% / 22.14% | 27.45% / 17.60% |
| 300 Mbps: sender / receiver CPU µs/Mbit | 893 / 738 | 915 / 587 |
| 300 Mbps: delivered / sent frames | 600 / 600 | 600 / 600 |
| 1 Gbps offered: delivered Mbps | 998.669 | 925.300 |
| 1 Gbps offered: delivered / sent frames | 600 / 600 | 557 / 557 |
| Unlimited: delivered Mbps | 1,373.650 | 1,287.968 |
| Unlimited: received packets/s | 163,190 | 153,011 |
| Unlimited: sender / receiver CPU | 87.66% / 53.25% | 89.41% / 53.20% |
| Unlimited: sender / receiver CPU µs/Mbit | 637 / 388 | 694 / 413 |
| Unlimited: lost frames / send errors | 0 / 0 | 0 / 0 |

Do not hide the 1-Gbps-cadence or unlimited regressions in this one pair. Earlier
runs during heavy concurrent builds varied from ~0.3 to ~0.83 Gbps; they are
retained in [exploratory/](exploratory/). The unlimited run uses 312,500-byte
frames without pacing, whereas the 1 Gbps case uses 1,041,666-byte frames on a
120-Hz grid. They stress different frame sizes; one result cannot substitute
for the other. More isolated, repeated measurements are needed to attribute
small differences and to guarantee the 1 Gbps/120-Hz frame cadence.

## Mac↔Linux

[fair-tailnet.txt](fair-tailnet.txt), five-second interleaved runs, direct Tailscale
path. Linux: Oracle A1 ARM64, four Neoverse N1 cores shared with other jobs.
Both sockets requested 8 MiB; readback Mac 8 MiB / Linux 16 MiB (Linux doubles
SO_SNDBUF/SO_RCVBUF accounting). Both tailscale interfaces use MTU 1280. Datagrams
remain 1200 bytes, shards 1172, FEC blocks at most 64 data shards.

| Path / offered media | Delivered Mbps before → after | Lost frames before → after |
| --- | ---: | ---: |
| Mac→Linux / 50 Mbps | 50.022 → 50.020 | 0 → 0 |
| Mac→Linux / 100 Mbps | 97.719 → 88.896 | 13 → 63 |
| Mac→Linux / 300 Mbps | 90.638 → 98.777 | 349 → 307 |
| Linux→Mac / 50 Mbps | 49.969 → 50.008 | 0 → 0 |
| Linux→Mac / 100 Mbps | 87.619 → 94.497 | 73 → 32 |
| Linux→Mac / 300 Mbps | 25.192 → 10.398 | 532 → 577 |

The first two 25-Mbps attempts in this raw log collided with another benchmark
in this workstream and failed to bind; exclude them. Later 25-Mbps Linux→Mac
runs delivered all 600 frames both ways. Some Mac→Linux runs missed startup
packets before their first received frame, and one ended early after a 300-ms
idle gap. Lost-frame counters exclude frames preceding that first arrival;
compare sent/delivered counts in the raw log as well. This prevents treating
steady-state 50 Mbps as lossless session startup.

At 300 Mbps offered, Linux sender CPU fell 17.30%→15.85%, CPU µs/Mbit 577→528
with sendmmsg. Mac sender CPU was 49.18%→47.60%, CPU µs/Mbit 1639→1617. Mac
receiver CPU at a clean 50 Mbps rose 3.54%→5.64% with batch setup, while Linux
receiver CPU was 2.95%→3.23%. These runs do not show a universal CPU improvement.
No sustainable real-path maximum between 50 and 100 Mbps was established.

## Feedback and FEC simulation

[before-scenarios.txt](before-scenarios.txt), [after-scenarios.txt](after-scenarios.txt).
Twelve simulated seconds, 1-ms ticks, deterministic seed 17. Step is 100→20→100
Mbps at t=2 and t=8 seconds. FIFO bounds queueing at 40 ms. Feedback is delayed
by base RTT; both comparisons use 50-ms reports to isolate controller behavior
from the production baseline's slower 250-ms reports. Host ping minimum samples
are spaced at 20 ms. The simulator exercises real packetization, FEC and ordered
reassembly. IDRs are equal-sized synthetic payloads, scheduled after an RTT;
no hardware IDR bitrate spike is modeled. Sender token-bucket pacing is excluded.

Queue p95 is serialization backlog sampled each millisecond, including a frame's
normal serialization burst; it excludes propagation, jitter and decoder queue.
"Ceiling" is time to ≥95% requested wire budget after t=8s (capacity return for
the step). All sessions initially start at the ceiling, in both versions.

| Scenario | Ceiling ms before → after | Queue p95 ms before → after | Lost frames before → after |
| --- | ---: | ---: | ---: |
| 100→20→100 Mbps, 120 fps, RTT 10 ms | >4000 → 960 | 38.70 → 11.23 | 300 → 19 |
| 200 Mbps, 240 fps, RTT 10 ms | >4000 → 0 | 9.48 → 4.22 | 0 → 0 |
| Independent 2% loss | 0 → 0 | 14.48 → 7.99 | 0 → 0 |
| Bursty Wi-Fi, enter .002/packet, exit .25 | >4000 → 0 | 6.86 → 8.19 | 24 → 15 |
| RTT 100 ms | 0 → 0 | 27.68 → 8.38 | 0 → 0 |
| Uniform 0–6 ms downstream jitter | 3560 → 0 | 9.00 → 8.38 | 0 → 0 |

Step-down convergence to ≤20 Mbps: **3110→160 ms**. This is a few feedback
windows, not a few 10-ms RTTs: that stronger target remains unmet. Step recovered
within 960 ms. Bursty Wi-Fi retains the ceiling rather than reducing rate for
isolated frame damage; persistent parity protection reduces losses. Tests assert
bounded queueing, ≤400-ms cuts, ≤1.5-s recovery, no cuts for repaired 2% loss,
and bounded Wi-Fi losses. Existing slow-feedback FIFO and real noise-stream
relay tests remain in bottleneck.rs.

FEC on Mac (MiB/s, benchmark labels historically say MB/s):

| Shape × 1172 bytes | Encode | Rebuild all parity-count missing data shards |
| --- | ---: | ---: |
| 64+7 | 5004.0 | 2782.6 |
| 64+20 | 1958.3 | 193.8 |
| 128+14 | 556.7 | 272.5 |
| 200+55 | 119.8 | 82.7 |

[fec.txt](fec.txt) includes cold single-rebuild timing and 1000-iteration means
(including allocation/cloning for rebuild). High shard counts are much more
expensive: the production 64-data-shard bound is retained. FEC arithmetic is
unchanged; these are headroom checks, not an encode/decode speedup claim.

## Commands and reproduction

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
cargo build --locked --release -p latch-proto --example transport
python3 docs/measurements/2026-11-03/transport/baseline.py
cargo build --release --manifest-path /tmp/latch-gfn/b-baseline/Cargo.toml --example transport
# One receiver and sender at a time, on separate shells:
target/release/examples/transport receive 127.0.0.1:47881
target/release/examples/transport send 127.0.0.1:47881 300 5 120
# Repeat with rates 1000 and 0 (unlimited), then the baseline binary.
cargo zigbuild --release -p latch-proto --example transport --target aarch64-unknown-linux-gnu.2.39
scp target/aarch64-unknown-linux-gnu/release/examples/transport linux:gfn-b/transport-after
ssh linux '~/gfn-b/transport-after receive 0.0.0.0:47881 100.111.100.57:47881'
target/release/examples/transport send 100.70.121.94:47881 50 5 120
# Reverse, in separate shells; repeat at 100 and 300:
target/release/examples/transport receive 0.0.0.0:47881 100.70.121.94:47881
ssh linux '~/gfn-b/transport-after send 100.111.100.57:47881 50 5 120'
cargo run --release --manifest-path /tmp/latch-gfn/b-baseline/Cargo.toml --example scenarios
cargo test --release -p latch-engine --test transport_scenarios -- --nocapture
cargo test --release -p latch-engine --test bottleneck -- --nocapture
cargo test --release -p latch-proto benchmark_fec -- --ignored --nocapture
cargo zigbuild --release -p latch-proto --tests --target aarch64-unknown-linux-gnu.2.39
# scp executable target/.../release/deps/latch_proto-<hash>, then run it on linux.
```

Remote receivers send probes to prime the return path; senders bind port 47881.
Loopback senders use ephemeral ports. All benchmark processes exit on completion
or 15-second startup timeout. Raw stderr/stdout can interleave in shared logs.
Baseline helper prepares original source in /tmp without touching a checkout;
its receiver shim performs one recv_from, with the same polling/QoS/measurement
harness as after. The recorded tailnet baseline used libc .191 / serde_json .154;
the final baseline script and loopback run pin the workspace's .189 / .151.

## Validation and remaining work

Validation: cargo fmt --all; CI clippy --locked --workspace --all-targets with
-D warnings; cargo test --locked --workspace --all-targets; native audit;
Linux ARM64 proto unit tests ([log](linux-proto-tests.txt)). An early full-suite
run hit an unrelated client refresh test's WouldBlock timeout; its isolated
retry passed. A separate early noise-stream test exposed 50-ms statistics
aliasing at low frame rates; fast feedback and 250-ms display stats are now
separate. Final validation status is recorded in [validation.txt](validation.txt).

Outstanding / unverified:

- Darwin batch send and demonstrated Windows URO coalescing remain open.
  Linux GSO/GRO and Windows USO/URO implementation and measurements are above.
- Larger-datagram probing is deferred: production remote paths use Tailscale.
  Protocol 1 stays at 1200 bytes; see path analysis above.
- Multi-minute sustained 1 Gbps and 300 Mbps / 120-Hz end-to-end streaming.
  Five-second codec-free loopback met >1 Gbps; the 1-Gbps/120-Hz case did not
  keep its requested cadence in the after run. Real-path 300 Mbps failed.
- Timestamped encode-completion→last-packet egress and viewer handoff quantiles;
  no measured microsecond latency guarantee. Clean-path pacing and immediate
  data-before-parity handoff are checked structurally and in regressions.
- Competing-TCP fairness, multi-minute stability and multiple seeds. Steady
  caps, long RTT, slow feedback, low caps and lossless route changes are now
  covered by `steady_transport` (see "Review fixes" above).
- Reference invalidation is a negotiated protocol proposal only, with no wire
  plumbing or encoder implementation. KeyframeRequest keeps last-delivered-frame
  semantics; requested IDRs are tested at max(50 ms, RTT), including repeated
  requests that advance that frame number.
