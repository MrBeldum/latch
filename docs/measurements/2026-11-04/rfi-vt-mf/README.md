# Native reference recovery, 2026-11-04

M3 Pro MacBook Pro, synthetic NV12 IOSurfaces; RX 6600 Windows 11.
No deployed host was connected to, stopped, replaced, or rebooted.

## Second review round (review 60b)

Raw log: [review-fixes-2/stall-budget.txt](review-fixes-2/stall-budget.txt),
the review's `scratch_vt_leak` replaying the committed stall fixture six
times in one process, a fresh HEVC decoder after each cycle.

| Case | Before (d0a96af) | After |
| --- | --- | --- |
| H.264 stalls absorbed before H.264 refuses | 4 (counter checked after increment) | 3 per 10 minutes, then H.264 decoders fail until the oldest stall is 10 min old |
| Fresh HEVC decoder after the H.264 cap | fails ("stalled too often in this process"), for the life of the process | decodes (all 6 cycles) |
| Threads left per stall | 2 (blocked decode worker + blocked invalidate thread) | 1 (the worker; nothing calls into the session while it is blocked) |
| Media Foundation H.264 LTR on Windows hosts | on by default | opt-in with `LATCH_MF_LTR=1` until it has run on the RX 6600 |

The 500 ms Mac host stall watchdog applies to every session; the review
measured the slowest ordinary hardware encode under full GPU load plus
twelve CPU hogs at 60 ms (4K60 H.264), so it has about 8× headroom.

## Review fixes (after the #60 review)

The review of #60 found a freeze, a
corruption and a hang; all on the M3 Pro, synthetic IOSurfaces. Raw logs
are in [review-fixes/](review-fixes/). The sections after this one describe
the first round, before these fixes; where a number changed it is restated
here.

| Case | Before (a6cfc33) | After |
| --- | --- | --- |
| Mac viewer decoding the review's recovery stream (`loss-seed6.bin`) | blocks forever in `VTDecompressionSessionDecodeFrame` (review, 3 of 3) | stall at picture 41 detected at the deadline, session replaced, the next IDR decodes ([log](review-fixes/viewer-watchdog.txt)) |
| H.264 refresh after dropped pictures with long-term marks | sent: viewer references exceed `max_num_ref_frames` in 3/6 seeds at 320×192 and 4/8 at 960×540 (FFmpeg), Apple's decoder stalls in 2/8 | withheld, IDR instead: 0 of 16 H.264 runs ([unsafe 320×192](review-fixes/unsafe-refresh-320x192.txt), [unsafe 960×540](review-fixes/unsafe-refresh-960x540.txt)) |
| Pipelined loss simulator, H.264 runs with a mismatching picture | 3 of 4 seeds (review's harness, 400 frames) | 0 of 16 (8 seeds × with/without IDR and bitrate events, 600 frames) |
| Acknowledged tokens of pictures the viewer lacks | 8–14 per run (review) | 0 in 32 runs |
| Losses recovered by a refresh (pipelined, 3% bursty) | H.264 ~100% (with the corruption above), HEVC ~100% | H.264 36–46%, the rest IDRs; HEVC 91–100% ([log](review-fixes/pipeline-sim.txt)) |
| Network impairment simulator, IDRs/min with LTR (1% / 3%) | H.264 0 / 0, HEVC 0 / 0 | H.264 4 / 2, HEVC 0 / 0 ([log](review-fixes/simulator.txt)) |
| `LATCH_MAC_LTR=1`, H.264 3024×1964@120 | stream froze (killed after 60 s) | first encode fails, ordinary hardware session at ~72 fps ([log](review-fixes/stall-watchdog.txt)) |

What changed:

- **Viewer.** VideoToolbox decode calls run on a worker thread of the
  session and are waited for at most 1 s; a stalled session is abandoned
  (invalidating it blocks too) and recreated, and the viewer asks for an
  IDR. Fixture `crates/stream/tests/fixtures/vt-ltr-overflow.h264` (349 KB,
  stalls on its 22nd picture every time).
- **Root cause of the freeze.** VideoToolbox's H.264 SPS has
  `gaps_in_frame_num_allowed_flag = 0` and `max_num_ref_frames = 10`. Lost
  pictures that marked long-term slots (MMCO 6) leave those slots empty for
  the viewer, whose `frame_num` gap fills the rest with "non-existing"
  short-term pictures; when the encoder marks those slots again, as
  replacements for it, they are additions for the viewer, up to 14 of 10.
  VideoToolbox's refresh does not unmark them. The host now refuses such a
  refresh (any dropped picture carried an LTR token) and sends an IDR.
  HEVC lists its references in every slice and is unaffected.
- **Acknowledgement.** Each recovery records everything after its base as
  missing until the next IDR (lost pictures and frames discarded in flight
  at the boundary); those tokens, and tokens from before a spontaneous IDR,
  are never acknowledged.
- **Low-latency mode.** Still opt-in. LTR only for 8-bit H.264 High/HEVC.
  A failed low-latency encode reopens an ordinary hardware session (the old
  software fallback was the "hang"); a 500 ms stall reopens one too; missing
  the pace (over 1 in 20 frames slower than 2 intervals over 2 s, or one
  over 250 ms) reopens ordinary for the rest of the stream.
- **Media Foundation** (compile-checked only): periodic-content probe with
  three losses (one refreshed from an older slot), slots on ~33 ms and
  ~667 ms cadences, IDR when a slot's first picture is lost, buffering MFTs
  demoted to IDR recovery, an IDR right after the probe.

Commands:

```
cargo test --locked -p latch-stream --lib a_decode_that_never -- --nocapture
LATCH_VT_SIM_SEEDS=8 LATCH_VT_SIM_FRAMES=600 \
  cargo test --locked -p latch-engine --lib ltr_pipeline_with -- --nocapture
LATCH_VT_SIM_SEEDS=6 LATCH_VT_SIM_FRAMES=400 \
  cargo test --locked -p latch-engine --lib unsafe_h264_refresh -- --ignored --nocapture
# add LATCH_VT_SIM_SIZE=960x540 LATCH_VT_SIM_KBPS=600 LATCH_VT_SIM_SEEDS=8 for the stalls
LATCH_MAC_LTR=1 cargo test --locked -p latch-engine --test recovery \
  videotoolbox_impairment_simulator -- --ignored --nocapture
cargo test --locked -p latch-engine --lib h264_3024x1964_low_latency -- --ignored --nocapture
```

The unsafe-refresh runs predate the spontaneous-IDR token floor and show
one lost-token acknowledgement in 2 of the 960×540 seeds (both with many
IDRs). Tokens from before a VideoToolbox-initiated IDR are the likely
cause, and the floor was added for it, but the unsafe runs were not
repeated after it; the safe runs after it show none.

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
