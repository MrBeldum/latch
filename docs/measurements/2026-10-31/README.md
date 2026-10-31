# Engine optimisation, 2026-10-31

Frame rate, bitrate, smoothness and latency of the streaming engine,
measured on the viewer's own clock with `crates/stream/examples/view.rs`
(per-second stats, key press to decoded picture, and the interval between
decoded frames, polled every 250 µs) against `crates/engine/examples/serve.rs`
streaming the synthetic desktop (`LATCH_TEST_PATTERN=sweep|noise`).

- **Loopback:** both ends on a MacBook Pro (M3 Pro).
- **Real path:** host on stuzha (Oracle A1, 4× Neoverse N1, shared with
  other work), viewer on the Mac, over Tailscale (12–15 ms round trip).

Runs of the two builds were interleaved, because stuzha's load varies
(other jobs used up to 2.4 of its 4 cores during these runs).

## 5.0.1 (main) → this branch

stuzha → Mac, 1080p60, 20 Mb/s, 40 key presses per run, three interleaved runs each:

| | 5.0.1 | this branch |
| --- | --- | --- |
| frames per second | 42.4 | **60.2** |
| key to picture p50 | 35–39 ms | **30–31 ms** |
| key to picture p95 | 47–52 ms | **39–40 ms** |
| frame interval p99 | 30–33 ms | **21–23 ms** |

Loopback, 1080p60: 46.9 → 59.9 fps, key to picture p50 24.5 → 14.5 ms, p95 25.7 → 16.5 ms.

## #32 (what the machines ran) → this branch

stuzha → Mac, interleaved, each build with its own viewer:

| | #32 | this branch |
| --- | --- | --- |
| key to picture p50 (1080p60 sweep) | 46.7–47.9 ms | **33.4–35.2 ms** |
| key to picture p95 | 58.1–59.9 ms | **41.4–44.6 ms** |
| host capture → send | 8.0 ms | **6.9 ms** |
| 1080p noise at 30 Mb/s: fps (min) | 54.2 (46.9) | **59.8 (58.9)** |
| 1080p noise: host capture → send | 15.4 ms | **10.9 ms** |

#32's viewer waited for the next datagram before handing a completed frame
to the decoder. That made each frame about one frame interval late, and
frames reached the decoder in pairs. With the fix, against the same host:
frame interval sd 10.2 → 2.0 ms, p99 34.6 → 21 ms, key to picture p50
42 → 30 ms.

## Components

| Change | Evidence |
| --- | --- |
| Frames on a fixed grid (`video::Pacer`) | 5.0.1 paced from the end of each encode: 60 fps ran at 42–50 |
| Media threads at user-interactive QoS / highest priority | loopback frame interval sd 1.8 → 1.35 ms; 720p encode 2.3 → 1.4 ms |
| latch-codec NEON + real-time governor | [codec-simd](codec-simd/), [codec-realtime](codec-realtime/): stuzha 1080p motion 34.9 → 12.2 ms mean, 38.9 → 60 fps; desktop 25–40% faster, bit-identical where no effort was traded |
| FEC SSSE3/AVX2 split-nibble multiply | x86 encode 440 → 2700 MB/s, rebuild 4.7× faster; ARM 1.4–1.6× |
| Data shards before parity | a frame that loses nothing never waits for parity arithmetic |
| Congestion: back off from the rate actually sent; standing-queue test on the lowest ping of each window; parity floor by base round trip; pace only under loss or queueing | unit tests and `tests/bottleneck.rs`; on a clean path frames leave without pacing delay |
| Mac viewer: VideoToolbox IOSurface → Metal, no CPU copy | 3024×1964: viewer memory 40 → 14 MB, CPU 4.6 → 3.7%, decode 1.90 → 1.63 ms |
| Viewer-drawn pointer (Linux and test-pattern hosts) | the pointer moves with no network delay in free mouse mode |
| Windows: NV12 texture ring, cached views, 1.5-frame VBV, HEVC behind `LATCH_WINDOWS_HEVC=1` | CI only; not yet run on a GPU encoder |
| macOS host: VideoToolbox real-time settings | 1080p HEVC encode p50 9.4 → 9.25 ms, max 41 → 26 ms; VideoToolbox ignores `DataRateLimits` for keyframes (≈55× a P-frame), with or without low-latency rate control |

## Not measured

- Hermes (Windows, AMD RX 6600) was offline: the Windows pipeline changes
  ran on CI's software encoder only.
- A Mac as host still lacks Screen Recording and Accessibility permission.
- Real congestion could not be produced on demand. The adaptive bitrate and
  pacing rest on unit tests, `tests/bottleneck.rs`, and one natural episode
  (7% loss at 33 Mb/s on Wi-Fi) that motivated backing off from the sent rate.
