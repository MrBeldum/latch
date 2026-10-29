# Rust rewrite execution record — 2026-10-29

Latch 5 streams with its own engine, written in Rust. Sunshine (host),
moonlight-common-c (viewer transport), OpenH264 (viewer decode on Windows
and Linux), libopus (audio) and ring (TLS) are gone from the source, the
build, the installers and the packages. `scripts/audit-native.sh`, run in
CI, finds no crate in `latch-host`'s build graph that compiles C, C++ or
assembler on macOS, Windows or Linux (x86-64 and ARM64). The one assembly
left is inside `graviola`, the TLS provider used for update downloads:
formally verified routines written as Rust inline `asm!`, compiled by
`rustc`. Operating-system and driver APIs (ScreenCaptureKit, VideoToolbox,
DXGI, Direct3D 11, Media Foundation, WASAPI, the X11 protocol, the
PulseAudio protocol, ALSA through cpal) are the boundary, as the
[brief](RUST_REWRITE_PROMPT.md) set out.

The design is in [ENGINE.md](ENGINE.md), the wire format in
[PROTOCOL.md](PROTOCOL.md). Raw results are in
[measurements/2026-10-29](measurements/2026-10-29/).

## What shows it is right

Each line names evidence that does not rest on the code's own claims.

| Claim | Evidence |
| --- | --- |
| No C, C++ or assembler is compiled | `scripts/audit-native.sh` (CI job `native-audit`) walks the real feature-resolved build graph on four targets; eight crates ship such files without compiling them, each checked and listed with its reason in `scripts/audit-native.allow` |
| The H.264 decoder decodes correctly | ITU-T H.264.1 conformance bitstreams: 38 streams, 6,244 pictures equal to the reference YUV byte for byte (CI job `codec-conformance`); 21 more streams from FFmpeg's libx264 and from macOS's hardware encoder decode byte-identical to FFmpeg; 100,000 corrupted access units cause no panic and the next IDR always decodes; 1080p at 8 Mbps decodes in 5.1 ms a frame on one core of an M3 Pro (VideoToolbox does it in hardware on a Mac; this decoder serves Linux and a Windows viewer whose Media Foundation fails) |
| The encoder's stream is valid | FFmpeg, VideoToolbox and the Rust decoder each decode it to exactly the encoder's own reconstruction, across sizes, contents, bitrates and thread counts |
| The encoder is efficient | Against x264 `superfast zerolatency` (what Latch 4's Sunshine used) on identical frames: desktop content is faster on the 2-core ARM host (720p typing 3.4 ms vs 4.9; still 1.3 vs 5.1) at higher PSNR; natural video is within 0.5 dB at the same bitrate but 1.9–2.2× slower on the Mac and 1.9–2.9× slower on the ARM host ([tables](measurements/2026-10-29/encoder-vs-x264/)) |
| The picture that arrives is the one drawn | Loopback session with the test pattern: the decoded frame matches the drawn pattern at 44.6 dB luma PSNR |
| Input is reliable and fast | A key press reaches a decoded picture in 19.7 ms p50 over loopback, and in 33 ms p50 / 53 ms p95 over the tailnet to the ARM host (13 ms round trip), timed on the viewer's own clock |
| The transport survives a bad network | Through a relay with 3% loss, bursts, duplication, reordering and 10 ± 4 ms delay both ways, FEC rebuilds what it can, lost frames recover by keyframe, and 41 back-to-back key presses arrive exactly once, in order (CI, all platforms) |
| It costs less than Latch 4 | stuzha (2-core ARM Linux) sharing its real desktop at 720p60, 10 Mbps, with audio: the engine used about 3% of a core and 20 MB; Latch 4's Sunshine used 71% and 115 MB on the same desktop. The Mac viewer used 1.3% against 5.3% ([baseline](measurements/2026-10-29/latch4-baseline/), [Latch 5](measurements/2026-10-29/latch5-live-still/)) |
| It holds up over time | Ten minutes of motion on every frame (the sweep pattern) from the ARM host over the tailnet at 720p60, 8 Mbps: median 49.8 fps (p5 42.8), one second in 600 with loss, host memory flat at 15 MB, viewer at 17–18 MB ([run](measurements/2026-10-29/latch5-10min-motion/)) |
| The real service works | The 5.0 `latch-host --background` on stuzha admitted the Mac by its Tailscale identity, imported the owner's display script from Latch 4's Sunshine configuration, ran it before the session and its undo after, and used 1–2% of a core and 23 MB ([run](measurements/2026-10-29/latch5-service-stuzha/)); 4.1.1 was restored afterwards |
| Independent review | Four reviewers read the protocol, the session lifecycle, security and the platform code without the authors' notes; a fifth tried to refute each of their 40 findings and confirmed 37. All 37 are fixed, most with a regression test that failed before the fix |
| It builds and passes everywhere | CI on commit `1f4a0a7`: all eight jobs green — Linux x86-64 and ARM64, Windows and macOS (clippy denying warnings, every test, both end-to-end sessions on each), the conformance suite, the native audit, and the deployment self-tests. On the Windows runner that includes Media Foundation encoding into the Media Foundation decoder, a Desktop Duplication frame, WASAPI loopback, and the setup script's retirement of a dummy Latch 4 engine; on the macOS runner, VideoToolbox's fallback to its software encoder when the virtual Mac has no hardware one |

## Verified on the real machines (2026-10-29, evening)

- **Windows, on Hermes** (Windows 11, AMD Radeon RX 6600, the Virtual
  Display Driver): DXGI capture of the virtual display through AMD's
  Media Foundation encoder (`AMDh264Encoder`) to the Mac at 1080p60;
  SendInput landing where the letterboxed picture says (a point at
  480,270 of 1920×1080 reached 133,150 of the 800×600 desktop); the same
  PC as a viewer decoding with Media Foundation on the GPU (DXVA), a key
  press reaching the picture in 171 ms over a 142 ms round trip to
  Korea. Then the real install, over SSH, as a user would: the old
  BroLink-era engine, its folder, service, tasks and firewall rules
  retired (a backup kept in `C:\Users\Daniel\latch4-backup-20261029`),
  Latch 5 started in the signed-in session, Tailscale authorization, and
  the virtual display set to the viewer's 1920×1080 for the session and
  back to 800×600 after it. The install found three faults, fixed before
  merging: Windows' own Block rules for a new `latch-host.exe` beat
  setup's Allow rules; the installer could not name the user from SSH;
  the retire step left Latch 4's cursor-guard task.
- **Linux, on stuzha and the VPS relay node**, upgraded by
  `deploy/native/migrate-legacy.sh` (backups in `~/latch4-backup-20261029`):
  the owner's own display and desktop units kept, the engine unit
  retired, Sunshine's configuration moved aside, stuzha's display script
  imported and run per session. Two faults found and fixed: the
  migration started rather than restarted the running service, and
  `--setup` alone left a drop-in requiring the removed engine unit.

## Not verified

- **A Mac as a host.** It needs Screen Recording and Accessibility granted
  to Latch.app by the owner (setup asks); this shell has neither, so live
  ScreenCaptureKit capture, CoreGraphics input and HEVC end to end have
  not run.
- **Sessions of hours, and energy.** The longest session ran ten minutes;
  no energy measurement was taken.
- **Full-motion encoding on the ARM host at 1080p**: about 3.5× x264's
  time; 720p meets 30 fps at the median with spikes above budget on a
  shared host.

## Rollout

Released as 5.0.0. Hermes, stuzha and the relay node were upgraded on
2026-10-29 as above; the Mac takes the release app. A Latch 4 viewer
finds no stream on a Latch 5 host and offers its own update.

## Commands

```bash
export PATH=$HOME/.cargo/bin:$PATH
cargo test --workspace --all-targets                    # everything CI runs
sh scripts/fetch-h264-conformance.sh                    # ITU bitstreams (gitignored)
cargo test --release -p latch-codec --test conformance -- --nocapture
sh scripts/h264-crosscheck.sh && H264_CROSSCHECK=1 cargo test --release -p latch-codec --test decode_crosscheck
sh scripts/h264-encoder-compare.sh                      # against x264 (FFmpeg as reference only)
bash scripts/audit-native.sh                            # no C, C++ or assembler
cargo run --release -p latch-engine --example serve -- --bind 100.x.y.z    # a bare host
cargo run --release -p latch-stream --example view -- 100.x.y.z --keys 30  # viewer, key-to-picture timing
```
