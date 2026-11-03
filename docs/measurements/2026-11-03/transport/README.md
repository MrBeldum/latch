# Transport / rate control — 2026-11-03

Baseline: `9fe0ae9`. Workstream B, branch `gfn/transport`. Codec-free tests use
production Packetizer, ReedSolomon, wire parser and Reassembler; they exclude
capture, encoders, decoders, rendering and audio. FEC is on (10% initially).
No installed host was replaced or stopped. All manual servers used UDP 47881.

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

- Windows USO/URO, Linux GSO/GRO, and a safe shared-socket Darwin batch-send
  approach. No Windows socket code changed or Windows build was performed.
- MTU discovery, probing, negotiated larger datagrams and fallback. Tailscale
  1280 is confirmed, but direct-LAN 1500 and largest safe payload selection
  were not measured or implemented. Protocol 1 stays at 1200 bytes.
- Multi-minute sustained 1 Gbps and 300 Mbps / 120-Hz end-to-end streaming.
  Five-second codec-free loopback met >1 Gbps; the 1-Gbps/120-Hz case did not
  keep its requested cadence in the after run. Real-path 300 Mbps failed.
- Timestamped encode-completion→last-packet egress and viewer handoff quantiles;
  no measured microsecond latency guarantee. Clean-path pacing and immediate
  data-before-parity handoff are checked structurally and in regressions.
- Step-down within a few low-latency RTTs (160 ms at 10-ms RTT remains too slow),
  prolonged fairness/stability, route changes and adversarial feedback tests.
- Reference invalidation is a negotiated protocol proposal only, with no wire
  plumbing or encoder implementation. KeyframeRequest keeps last-delivered-frame
  semantics; requested IDRs are tested at max(50 ms, RTT), including repeated
  requests that advance that frame number.
