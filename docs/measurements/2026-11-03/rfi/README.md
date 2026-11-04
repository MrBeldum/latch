# Recovery frames against keyframes (2026-11-03)

What a lost frame costs before and after reference invalidation
([PROTOCOL.md](../../../PROTOCOL.md#recoveryrequest-14-and-recoveryack-15)).
"Before" is the same build with the viewer's `LATCH_TEST_NO_RECOVERY=1`:
it asks for keyframes exactly as 5.1 does. Loss is induced at the viewer
with `LATCH_TEST_VIDEO_LOSS=<percent>:<burst>` (that share of the host's
video datagrams dropped on arrival, in bursts, seeded), so both runs see a
clean link with the same loss process. Each run is 60 s.

Columns: losses (frames declared lost, once per loss), of them ended by a
keyframe; keyframes and recovery frames delivered a minute (the startup
keyframes, two a run, included); resume = loss declared to the next frame
delivered; freeze = last frame delivered before the loss to the next one
(how long the picture stood still), p50 / p95 in ms; mean IDR and recovery
frame bytes; mean frame bytes (bitrate over frame rate, almost all
P-frames); round trip.

| Host | Loss | Mode | Losses | By IDR | IDR/min | Recovery/min | Resume ms | Freeze ms | IDR B | Recovery B | Frame B | RTT ms |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- | ---: | ---: | ---: | ---: |
| Linux software H.264 | 1 % × 3 | before | 32 | 32 | 33.8 | 0.0 | 113 / 222 | 151 / 256 | 9934 | – | 1563 | 16 |
| Linux software H.264 | 1 % × 3 | after | 32 | 0 | 3.0 | 31.8 | 41 / 54 | 79 / 97 | 19771 | 3281 | 1279 | 16 |
| Linux software H.264 | 3 % × 6 | before | 49 | 49 | 50.7 | 0.0 | 111 / 393 | 165 / 452 | 9619 | – | 1384 | 16 |
| Linux software H.264 | 3 % × 6 | after | 48 | 0 | 2.0 | 47.7 | 42 / 57 | 99 / 120 | 21144 | 3330 | 1154 | 14 |
| Windows AMF HEVC | 1 % × 3 | before | 9 | 9 | 10.9 | 0.0 | 232 / 333 | 445 / 545 | 16529 | – | 1388 | 158 |
| Windows AMF HEVC | 1 % × 3 | after | 6 | 2 | 6.0 | 4.0 | 229 / 233 | 446 / 448 | 22446 | 791 | 710 | 166 |
| Windows AMF HEVC | 3 % × 6 | before | 12 | 12 | 13.9 | 0.0 | 228 / 267 | 470 / 636 | 18325 | – | 1544 | 169 |
| Windows AMF HEVC | 3 % × 6 | after | 10 | 4 | 9.0 | 6.0 | 224 / 266 | 445 / 593 | 10348 | 6384 | 971 | 179 |

Reading it:

* **Linux (software H.264).** Every loss ends with a recovery frame;
  the only IDRs left are the two every session starts with. A recovery
  frame is 2–3 P-frames' worth (3.3 kB), where the IDR that answered a
  loss before was 10 kB, and only that small because rate control
  starved it to fit the frame budget (a full-quality IDR of this picture
  is 20–21 kB, the startup ones). Resume halves (113 → 41 ms at p50) and
  its tail goes (222–393 → 54–57 ms): an IDR waits behind the host's
  keyframe gate and is itself lost more often, being seven times the
  datagrams.
* **Windows (AMF HEVC, RX 6600 → Mac VideoToolbox).** The PC's desktop
  was still, so frames are 100 ms idle repeats (about 11 a second) and a
  round trip is 160–180 ms: resume and freeze are set by those, before
  and after alike. What changes is what crosses the link: recovery frames
  of 0.8 kB (6.4 kB in the burstier run, predicting from an older
  long-term reference) instead of 16–18 kB IDRs. Some losses still ended
  with an IDR (2 of 6, 4 of 10): two startup losses before any frame was
  delivered, and refusals the host logged as "the encoder no longer holds
  it", where an IDR (asked for at startup by the decoder) came after the
  viewer's last frame and dropped every long-term reference. VideoToolbox
  decoded every recovery frame without an error. The 16 decoded luma
  samples at the end of each run (`luma_sample` in the JSON) are within
  4 levels of a loss-free run of the same desktop (`win-no-loss.json`),
  coding noise; a frame predicting from the wrong picture would be off by
  far more. The "before" run is off by up to 15 there: its starved IDR
  was still being refined.

## Simulated network (deterministic)

`cargo test -p latch-engine --test recovery -- --nocapture`: software
encoder (320×192, 60 fps, 2.5 Mbps, a still detailed desktop with a bar
moving across it), the real packetizer with 10% FEC, a seeded impaired
link both ways, the real reassembler, recovery state machine and decoder;
30 s per run. Every decoded frame is checked against the encoder's
reconstruction for it: all equal, recovery frames included.

| Link | Mode | Losses | IDR/min | Resume p50 / p95 ms | Freeze p50 / p95 ms | IDR B | Recovery B | P B | Frames delivered |
| --- | --- | ---: | ---: | --- | --- | ---: | ---: | ---: | ---: |
| 12% random loss, 15 ms one way | before | 48 | 172 | 49 / 448 | 85 / 485 | 19720 | – | 1573 | 1321 / 1800 |
| 12% random loss, 15 ms one way | after | 55 | 8 | 48 / 114 | 84 / 150 | 19825 | 2716 | 1440 | 1518 / 1800 |
| bursts (Gilbert–Elliott), 25 ms one way | before | 31 | 66 | 65 / 165 | 103 / 201 | 19710 | – | 1489 | 1618 / 1800 |
| bursts (Gilbert–Elliott), 25 ms one way | after | 26 | 4 | 64 / 149 | 102 / 185 | 19581 | 2557 | 1439 | 1645 / 1800 |

## Commands

```sh
# Linux host (software encoder, test pattern), cross-built on the Mac:
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39 -p latch-engine --example serve
LATCH_TEST_PATTERN=sweep ./serve --bind 100.70.121.94 --port 47893      # on linux
# Windows host (AMF), in the signed-in session via a scheduled task:
LATCH_WINDOWS_ENCODER=amf serve.exe --bind 100.106.23.40 --port 47893
# Viewer on the Mac; add LATCH_TEST_NO_RECOVERY=1 for "before":
LATCH_TEST_VIDEO_LOSS=1:3 cargo run --release -p latch-stream --example view -- \
  100.70.121.94 --port 47893 --width 1280 --height 720 --fps 60 --kbps 6000 --seconds 60
LATCH_TEST_VIDEO_LOSS=3:6 cargo run --release -p latch-stream --example view -- \
  100.106.23.40 --port 47893 --width 1920 --height 1080 --fps 60 --kbps 20000 --hevc --seconds 60
```

The JSON files here are the view example's output; `recovery` holds the
figures above.

## Not measured

* AMF with motion on the PC's screen: the Windows test pattern runs the
  software encoder, not AMF, and the desktop was left as it was.
* Media Foundation, VideoToolbox, VA-API and NVENC hosts: they refuse
  recovery and send an IDR, as before.
