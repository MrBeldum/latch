# Native reference recovery, 2026-11-04

M3 Pro MacBook Pro, synthetic NV12 IOSurfaces; RX 6600 Windows 11.
No deployed host was connected to, stopped, replaced, or rebooted.

## Implementation and default behaviour

VideoToolbox loads the four public LTR keys at runtime. It reads tokens from
`CMSampleBufferGetSampleAttachmentsArray`, acknowledges only pictures no newer
than the delivered base, and forces an LTR refresh. An IDR or newer acknowledged
base rejects stale requests. The two-in-flight boundary discards queued frames
before the refresh; an explicit drop or missing callback falls back to IDR.
Picture identities survive hardware-to-software encoder replacement. Session
pipeline generations remain the protocol's existing generation guard.

The ordinary M3 Pro encoder rejects EnableLTR (-12900). Setting
`LATCH_MAC_LTR=1` opts into EnableLowLatencyRateControl, where both H.264 High
and HEVC accept LTR. **This mode is deliberately not the default:** measured
HEVC throughput regresses, so enabling it by default would violate the no-regression
requirement. Main10 LTR and H.264 Baseline LTR were not independently validated.

Media Foundation H.264 requests four application-controlled LTRs in trust-until
mode (LTRBufferControl=0x10004). At each candidate encoder open, 32 moving
synthetic pictures at the requested configuration undergo two loss/recovery
episodes. Every delivered picture must match uninterrupted MF decoding. Empty
CPU output, shared renderer surfaces, buffering, extra IDRs, setter errors,
decode errors and pixel mismatches reject the probe; a fresh MFT then opens
with IDR recovery. There is no property-readback requirement. Recovery marks
use the high-word trust-until flag as well as the slot bitmap. Picture numbers
continue across MFT/AMF replacements and the software fallback.

The RX 6600 AMD H.264 MFT passed this probe at 640×360/60/8 Mbps even though
its properties cannot be read back. AMF HEVC's existing slot-reuse hardware
test also passed. The AMF H.264 component is never instantiated in production;
only HEVC uses AMF, so no H.264 AMF path was added.

## Exact native decode tests

`cargo test --locked -p latch-engine --lib ltr_recovery_matches -- --nocapture`
encodes 120 pictures at 960×540/30/10 Mbps per codec, dropping frames 20–27
and 60–63, forcing refreshes at 24, 28 and 64. Thus the refresh at 24 is
itself lost. Both VideoToolbox and independent FFmpeg decoders compare every
one of the 108 delivered pictures to uninterrupted decoding, byte for byte.
Both H.264 High and HEVC passed. An additional boundary test covers two queued
frames, an explicitly dropped refresh, a refresh with no callback, stale IDR
bases and software replacement.

## End-to-end impairment simulator

```
LATCH_MAC_LTR=1 cargo test --locked -p latch-engine --test recovery \
  videotoolbox_impairment_simulator -- --ignored --nocapture
```

The existing software simulator now also accepts the native encoder and two
VideoToolbox decoders. 320×192/60/2.5 Mbps, 1800 frames per case, seed 23;
15 ms each way, 4 ms jitter, 10% duplication, 10% extra reordering by 30 ms,
Gilbert–Elliott bursts of mean four datagrams. Burst-enter probabilities
0.0025/0.0075 target approximately 1%/3% loss. Encoder and packet sizes change
the exact losses, so before/after do not imply identical lost frame numbers.
Every delivered picture in all eight native cases matched the uninterrupted
decoder. Raw results: [simulator.txt](simulator.txt).
IDRs/min below excludes the startup IDR. Resume measures declaration of loss
to the next delivered picture, on the simulated clock.

| Codec / target burst loss | IDRs/min, IDR → LTR | Resume p50/p95 ms, IDR → LTR | Mean IDR → recovery bytes |
| --- | ---: | ---: | ---: |
| H.264 High / 1% | 54 → 0 | 50/116 → 47/50 | 23,437 → 766 |
| H.264 High / 3% | 78 → 0 | 50/167 → 47/63 | 23,306 → 850 |
| HEVC / 1% | 40 → 0 | 50/100 → 47/49 | 25,123 → 1,037 |
| HEVC / 3% | 64 → 0 | 50/117 → 47/66 | 25,188 → 814 |

## Live Mac host → Mac viewer

Built the release `serve` and `view` examples. Own host bound to loopback
47900; 1280×720/60, requested 20 Mbps, sweep pattern, 20 seconds per case.
The existing controller adapts bitrate/FEC. `LATCH_TEST_VIDEO_LOSS=1:8` or
`3:8` injects bursts of eight arriving video datagrams. `LATCH_TEST_VIDEOTOOLBOX=1`
selects synthetic IOSurfaces instead of the default software test encoder.
Before: ordinary VideoToolbox mode/IDR recovery. After: `LATCH_MAC_LTR=1`.
The default High viewer is used for H.264; `--hevc` selects HEVC.

Run [live.py](live.py) from the repo root after building both examples; it starts
and terminates only its own server. JSON files beside this report contain
full measurements. Live IDRs/min **includes startup** (and any extra encoder
IDR); resume clocks are the viewer's. These short runs are recovery smoke
tests, not a bandwidth or quality equivalence experiment.

| Codec / loss | IDRs/min, before → after | Resume p50/p95 ms, before → after | IDR before → recovery after, bytes |
| --- | ---: | ---: | ---: |
| H.264 / 1% | 14.9 → 3.0 | 30.3/35.5 → 26.3/30.4 | 10,172 → 470 |
| H.264 / 3% | 29.9 → 3.0 | 26.9/30.1 → 26.2/32.7 | 10,383 → 521 |
| HEVC / 1% | 14.9 → 3.0 | 22.5/31.8 → 25.2/30.1 | 8,137 → 1,171 |
| HEVC / 3% | 29.9 → 6.0 | 27.4/30.5 → 25.7/31.5 | 8,301 → 1,036 |

All 26 observed LTR-mode loss episodes ended through a recovery frame, with
zero reported decode errors. Delivered frame rates ranged 57.5–59.0 fps
under loss. ScreenCaptureKit and real screen capture are not covered.

## Why VideoToolbox low-latency LTR stays opt-in

Two frames in flight, prefilled synthetic IOSurface pool, two-second runs;
HEVC requested rates 50/80/100 Mbps. These are encoder throughput measurements,
without conversion, transport or loss. The existing benchmark can select
low-latency mode with LATCH_VT_BENCH_LTR=1.

```
LATCH_VT_BENCH=hevc:1920x1080@120:50000,hevc:1920x1080@240:80000,hevc:3024x1964@120:100000 \
LATCH_VT_SECONDS=2 cargo test --release -p latch-engine --lib realtime_benchmark -- --ignored --nocapture
# Repeat with LATCH_VT_BENCH_LTR=1
```

| HEVC request | Ordinary → LTR-mode fps | Encode p50 ms | Output kbps |
| --- | ---: | ---: | ---: |
| 1080p120 / 50 Mbps | 121.1 → 107.3 | 7.79 → 18.72 | 31,628 → 13,146 |
| 1080p240 / 80 Mbps | 242.2 → 101.6 | 4.88 → 19.69 | 79,903 → 10,571 |
| 3024×1964@120 / 100 Mbps | 120.8 → 46.9 | 11.42 → 43.21 | 102,292 → 12,162 |

Raw: [ordinary](throughput-before.txt), [LTR mode](throughput-ltr.txt).

## Validation and outstanding Windows work

Passed: Mac workspace/all-target tests (714 passed, 34 ignored), workspace
doc tests, CI clippy on Mac, x86_64-pc-windows-msvc and
aarch64-unknown-linux-gnu, and audit-native. Also passed on the Mac:
`cargo check --locked --workspace --all-targets --release --target x86_64-pc-windows-msvc`.
This is a compile check, not a linked Windows executable or native release run. Windows engine unit tests passed on
the PC (83 tests at the earlier validated revision, 9dda649). The #53 scratch edge
harness passed 64 seeds; the wrap harness passed 65,560 frames. FFmpeg
independently decoded eight seed streams plus the wrap stream, all exact.
Scratch sources were reused temporarily and removed; [log](review53-scratch.txt).

Windows release compilation on the PC crashed in rustc, first with 0xc0000409
and then invalid compiler metadata. A -j2 retry, -j1 retry and clean release
rebuild were attempted. The PC became fully offline on Tailscale during the
clean retry. The owner confirmed its known RAM faults and instructed us not
to wait or reboot remotely. The final Windows release **cross-check on Mac**
passed; it is separate from a native Windows build/run.

**Windows live tests and final Windows hardware verification did not run.**
Earlier AMD H.264 and AMF HEVC smoke tests passed before the outage, but the
latest ignored-control negative test, final-head Windows unit suite, independent
FFmpeg checks of MF probe dumps, and Windows→Mac loss measurements remain
unverified. No claim of completed Windows validation is made. No deployed
host was changed, and no remote reboot was attempted.

Cleanup is deferred by the owner until the PC is reachable. Remove:

- Build directory: `C:\Users\Daniel\dev\latch-rfi2` (including logs, dump
  files and any still-running cargo/rustc processes targeting that directory).
- Scheduled task: `LatchTestRfiMF` (hidden hardware test launcher).
- Firewall allow rule: `LatchTest rfi-vt-mf tests`.
- Any auto-created Block rules whose application path is beneath that build
  directory. No other test tasks or firewall rules were created.

[cleanup-windows.ps1](cleanup-windows.ps1) performs those scoped removals from
an elevated SSH session. Preserve wanted logs/dumps before removing the directory.

## API references

- [Apple SDK LTR acknowledgement tokens](https://developer.apple.com/documentation/videotoolbox/kvtencodeframeoptionkey_acknowledgedltrtokens)
- [Apple WWDC21 low-latency mode and LTR](https://developer.apple.com/videos/play/wwdc2021/10158/)
- [Microsoft LTR buffer/trust mode](https://learn.microsoft.com/en-us/windows/win32/medfound/codecapi-avencvideoltrbuffercontrol)
- [Microsoft LTR reference bitmap and subsequent-picture restrictions](https://learn.microsoft.com/en-us/windows/win32/medfound/codecapi-avencvideouseltrframe)
