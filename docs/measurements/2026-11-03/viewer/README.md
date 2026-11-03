# Viewer decode and renderer preparation, 2026-11-03

Workstream C, MacBook Pro M3 Pro and Windows 11 / RX 6600. The baseline is
9fe0ae9 with only the native measurement example/timestamps added. Original
tables below precede the shared-texture continuation; see the continuation
section for the final path. Tests use
port **47882** and isolated Windows checkout `C:\Users\Daniel\dev\latch-gfn-c`;
the installed host is not replaced. Other workstreams also used these machines;
these are observed runs, not a controlled claim of performance improvement.

## Timing definition

`Frame.decoded_at` is stamped immediately after synchronous decode returns,
before health checks and publication. `present` samples its age at the end of
the real client's wgpu paint callback **prepare**: IOSurface import or CPU
upload has been enqueued. These are CPU decode-to-render-ready timings, **not
GPU completion, OS present, compositor latency, scanout or input-to-photon**.
No public portable eframe present-completion timestamp is available here.
Decode times exclude health checks; stream values below average the printed
per-second window samples, not individual frames. Replay decode values are
per-frame percentiles.

The native window uses the app's `AutoNoVsync`, maximum frame latency 1,
and immediate `request_repaint` on decoded frames. Its one timer only closes
the measurement window at the requested duration. FrameSlot replaces an
undrawn picture rather than queueing it. GPU-completion retention protects
old IOSurfaces; the renderer always binds the newest one.

## Mac loopback: target 120 fps, 50 Mb/s, 15 seconds

| Requested picture | Decode mean before → after | Decode→prepare p50 before → after | p95 before → after | Observed decode fps before → after |
| --- | --- | --- | --- | --- |
| 1920×1080 @120 | 0.958 → 0.888 ms | 0.141 → 0.143 ms | 0.235 → 0.216 ms | 119.9 → 120.0 |
| 3024×1964 @120 | 1.598 → 1.785 ms | 0.177 → 0.229 ms | 0.255 → 0.286 ms | 102.9 → 73.3 |

These loopback streams use the synthetic sweep's **software** host encoder,
not the Mac hardware encoder. Native-size delivery did not sustain 120 fps.
The renderer prepared 1537/1538 baseline and 1094/1094 after-change decoded
pictures; reduced arrival rate is upstream of presentation. Host CPU load and
encode time vary between runs, so the table does not establish a throughput
regression or improvement in the viewer. Raw runs and computed sample means
are alongside this document.

## Mac paced replay: target 120 fps, 15 seconds

Repeated independent H.264 High IDRs isolate decoder/render throughput from
the software host and network. This is a synthetic throughput test, not
representative inter-frame game video or a streaming measurement.

| Picture | Decoder | Decode p50 / p95 | Decode→prepare p50 / p95 | Decoded / prepared |
| --- | --- | --- | --- | --- |
| 1920×1080 | VideoToolbox hardware | 1.782 / 2.189 ms | 0.183 / 0.342 ms | 1802 / 1792 |
| 3024×1964 | VideoToolbox hardware | 4.122 / 4.837 ms | 0.232 / 0.547 ms | 1802 / 1774 |

Both decode at 120 fps; newest-frame replacement skips some pictures when the
UI/GPU is late. This proves absence of an application 60 Hz cap, not that the
physical display scanned every decoded frame. GPU readback tests independently
check white/black visible pixels for native H.264, HEVC, AV1 and Main10 P010.
The kittest benchmark in `test-gpu-mac.log` includes screenshot readback and is
not a display latency benchmark.

## Windows interactive session

The cached startup probe offers **AV1 > HEVC > H.264 High > Baseline** on
this RX 6600. Independent pictures decode through DXVA for AV1, HEVC and
Main10; the Media Foundation extensions are installed on this machine.
Low-latency decoding needs the negotiated size hint before media-type setup.
The baseline's no-size H.264-only API remains available to existing callers.

H.264 High paced IDR replay, 10-second native windows (decode includes D3D11
staging readback; preparation includes enqueued wgpu CPU-plane upload):

| Requested | Decode p50 / p95 | Decode→prepare p50 / p95 | Decoded / prepared |
| --- | --- | --- | --- |
| 1080p60 | 1.405 / 2.140 ms | 0.441 / 0.764 ms | 597 / 597 |
| 1080p120 | 1.282 / 1.969 ms | 0.329 / 0.541 ms | 1194 / 1194 |
| 1080p240 | 1.161 / 2.104 ms | 0.300 / 1.395 ms | 2389 / 2385 |
| 4K60 | 4.417 / 5.096 ms | 1.347 / 1.640 ms | 594 / 594 |
| 4K120 | 6.507 / 8.147 ms | 1.217 / 1.514 ms | 1190 / 1188 |

Main10's 256×256 test also returned DXVA pictures (596 decoded / 594
prepared in a five-second, target-120 window); it ran concurrently with
part of the matrix, so it is capability evidence, not an isolated latency
result. The 64×64 Main10 smoke test separately returned 178/178 pictures.
No frame cap was added; the 4K120 decode p95 already approaches its 8.33 ms
interval. This table predates shared-texture import; see the continuation below.

The own-server Windows loopback run at 1080p120 averaged 2.512 ms decode
and 0.467/1.563 ms decode→prepare p50/p95, preparing 942 of 959 decoded
pictures over 15 seconds. Baseline headless `view` averaged 3.833 ms decode
and 120.3 decoded fps in a ten-second run. The native after-run received only
about 64 pictures/sec overall (76.2 fps averaging the printed stats windows).
Host load/recovery varied and the windows differ; this is **not a controlled
before/after speedup**. The replay matrix is the decoder/display isolation
check. Raw baseline JSON, loopback stats and replay logs are archived.

## Commands

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
cargo build --release -p latch-engine --example serve -p latch-client --example present
LATCH_TEST_PATTERN=sweep target/release/examples/serve --port 47882
# In another shell:
target/release/examples/present --port 47882 --width 1920 --height 1080 --fps 120 --seconds 15
target/release/examples/present --port 47882 --width 3024 --height 1964 --fps 120 --seconds 15
# Generate one independent replay access unit (FFmpeg is a test tool only):
ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=120 -frames:v 1 -c:v h264_videotoolbox -realtime 1 -bf 0 -f h264 replay-1080.h264
target/release/examples/present --replay replay-1080.h264 --width 1920 --height 1080 --fps 120 --seconds 15
# Likewise 3024x1964, or 3840x2160 for Windows.
cargo test -p latch-client video::tests::decoded_frame_reaches_the_render_target -- --ignored --nocapture
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
bash scripts/audit-native.sh
```

For Windows GUI tests, SSH is session 0. `windows-probe.ps1` is launched by
`windows-probe-launch.vbs` (`Wscript.Shell.Run ..., 0, False`) from the one-off
`LatchGfnViewerProbeC` task, with `New-ScheduledTaskPrincipal -LogonType
Interactive -RunLevel Limited` for the signed-in user. Each measurement process
has a 30-second cleanup timeout. The example requests an inactive window; no
keys, pointer movement or display-mode changes are sent. The scripts and raw
Windows results are archived with this report. Only this workstream's server,
viewers and task registrations are removed after testing.

## Limits and remaining work

- Windows NV12 now uses shared D3D11/D3D12 textures with a fenced GPU copy
  and retention through GPU completion. P010 and unsupported sharing still
  use staging readback/upload. No claim that decode/display can never
  bottleneck at any resolution or rate is justified.
- Linux VA-API is deferred: runtime loading alone cannot supply codec picture
  and slice parameters. Baseline software decode remains unchanged.
- AV1 is decode-only in this branch; there is no AV1 streaming host encoder.
- P010 retains Main10 samples on Mac. Normal sessions remain SDR NV12; HDR
  transfer functions, metadata, display negotiation and tone mapping are not
  implemented. Windows currently reduces P010 output to SDR eight-bit planes.
- Baseline alone has the ten-access-unit pure-Rust fallback. High/HEVC/AV1
  have no pure-Rust software decoder; startup probes gate their advertisement.
- 144–360 Hz physical scanout, tear behaviour, GPU-present completion and
  input-to-photon latency were not measured.

## Continuation: final shared-texture path

The client prefers D3D12 on otherwise equivalent adapters and requests NV12
plane views. The decoder uses that adapter, GPU-copies into at most six
shareable textures, and waits for its D3D11 fence before publishing. wgpu
opens and caches those resources; a surface is reused only after all frames
holding it have gone, including the GPU-completion callback. Every fifteenth
picture is copied to staging for health sampling on the next picture. A
sample keeps its own staging texture through size/format changes. Odd-sized
pictures, P010, exhausted pools and unavailable sharing retain the CPU path.
A failed import disables sharing for subsequent pictures.

The new Windows replay matrix pairs `--cpu` with the default shared path on
**the same release binary and D3D12 backend**, using inter-frame elementary
streams, not the independent-IDR replay in the original table. It isolates
the transfer path rather than comparing whole releases. These six-second
runs are single observations on shared machines; compilation overlapped the
first H.264 cases, so they do not establish a controlled speedup. Commands:

```powershell
# In the interactive session, from the isolated checkout:
target\release\examples\present.exe --replay stream-1080.h264 --codec h264-high --width 1920 --height 1080 --fps 120 --seconds 6 --cpu
target\release\examples\present.exe --replay stream-1080.h264 --codec h264-high --width 1920 --height 1080 --fps 120 --seconds 6
# Repeat with HEVC/AV1 and 1080p60/120/240 or 4K60/120.
```

| Codec / requested picture | Decode p50 CPU → shared (ms) | Decode→prepare p50 CPU → shared (ms) | Decoded / prepared CPU → shared |
| --- | --- | --- | --- |
| h264-high 1080p60 | 4.335 → 4.104 | 0.415 → 0.134 | 357/357 → 358/358 |
| h264-high 1080p120 | 9.950 → 10.103 | 0.398 → 0.130 | 617/614 → 455/453 |
| h264-high 1080p240 | 10.147 → 3.727 | 0.443 → 0.112 | 512/477 → 1427/1423 |
| h264-high 4K60 | 14.148 → 10.761 | 1.365 → 0.133 | 354/353 → 355/354 |
| h264-high 4K120 | 14.436 → 10.723 | 1.433 → 0.119 | 408/407 → 546/545 |
| hevc 1080p60 | 5.332 → 4.706 | 0.365 → 0.130 | 358/357 → 357/357 |
| hevc 1080p120 | 5.372 → 4.923 | 0.385 → 0.122 | 714/681 → 725/667 |
| hevc 1080p240 | 4.791 → 4.217 | 0.537 → 0.123 | 1217/1193 → 1404/1374 |
| hevc 4K60 | 12.713 → 9.519 | 1.394 → 4.328 | 357/324 → 358/325 |
| hevc 4K120 | 14.391 → 16.234 | 5.410 → 12.041 | 402/351 → 373/352 |
| av1 1080p60 | 7.714 → 4.598 | 9.072 → 0.136 | 359/290 → 358/345 |
| av1 1080p120 | 4.979 → 4.232 | 0.379 → 0.141 | 715/714 → 714/683 |
| av1 1080p240 | 4.761 → 3.922 | 0.512 → 0.129 | 1233/1122 → 1424/1353 |
| av1 4K60 | 12.726 → 8.356 | 1.299 → 0.176 | 358/337 → 358/357 |
| av1 4K120 | 17.304 → 16.704 | 8.973 → 6.919 | 345/317 → 417/340 |

Raw p95 values and all counts are in `windows/c3-summary.json`. One HEVC
1080p60 CPU run failed during surface configuration (`Invalid surface`);
the table uses its separate retry and preserves the failed log. The 4K120
runs did not sustain 120 decoded pictures/sec. AV1 4K120 preparation was
also slow; the shared path does not eliminate the GPU/decoder bottleneck.
All three codecs selected DXVA and D3D12 on the RX 6600.

Fresh Mac sweep loopback checks used the same original baseline binary and
the final release build, ten seconds each at 120 requested fps on port 47882:

| Picture | Decode window mean before → after | Decode→prepare p50 before → after | p95 before → after | Decoded / prepared before → after |
| --- | --- | --- | --- | --- |
| 1080p120 | 1.181 → 1.173 ms | 2.458 → 2.947 ms | 7.322 → 7.302 ms | 645/578 → 613/556 |
| 3024×1964@120 | 2.062 → 1.899 ms | 0.264 → 0.252 ms | 2.685 → 2.698 ms | 198/197 → 217/217 |

These runs did not sustain 120 fps: the software sweep host and shared
machine load limited delivery. They are included alongside the earlier
higher-throughput observations, not selected as an improvement claim. Raw
logs and window means are in `mac/` and `mac-c3-summary.json`.

Validation on the continuation: Mac `cargo fmt --all -- --check`, CI clippy,
`cargo test --locked --workspace --all-targets`, release examples, native-code
audit, and the ignored GPU pixel-readback test passed. Cross-target Windows
clippy for stream/client passed; Windows native workspace clippy and release
`present` build passed. Windows native debug tests failed first at a linker
error after a rustc crash; retries without incremental compilation and in
release also hit rustc `STATUS_ACCESS_VIOLATION` in dependencies (`egui`,
`naga`, `windows`) and LLVM invalid debug-info diagnostics. The release
workspace retry was stopped after the same compiler failures recurred.
Consequently a passing Windows workspace test suite and the new Windows
wgpu test cannot be claimed. As a separate interactive smoke check, OS
window screenshots showed the expected light-top/dark-bottom test pattern
for H.264 High, HEVC and AV1 through the built D3D12 viewer. These screenshots
are not calibrated colour or pixel-equality tests; only the test windows
were raised temporarily without changing keyboard focus. No deployed binary, display mode or driver was
changed. The Mac CI runner exposed a false-positive VideoToolbox hardware
query; Mac offers now also require a cached, successfully decoded hardware
picture. The local Mac checks passed again after that correction.

The final Windows viewer also attempted the deployed Linux host at
`100.70.121.94:47850`, 1080p120 for ten seconds, without input. Session setup
failed with socket error **10057** before any picture arrived; the log is
`windows/c3-linux-live.log`. Transport changes are outside this workstream,
so this does not count as a successful live remote-stream validation. The
original own-server loopback and the final paced replay matrix remain the
successful Windows decode/presentation evidence.

Cleanup confirmed no `LatchGfnViewer*C` task registrations and no test
`present`/`serve` processes left in the isolated checkout. The deployed host
remained PID 6084 in interactive session 1, at
`C:\Users\Daniel\AppData\Local\Latch\latch-host.exe`, listening on 47850.
No display modes or drivers were changed; other workstreams' tasks and
`LatchInput` were left alone. The Mac test server on 47882 was stopped.

## API references

The implementation follows [Microsoft's HEVC decoder media-type and low-latency
contract](https://learn.microsoft.com/en-us/windows/win32/medfound/h-265---hevc-video-decoder),
[Apple's format-description API](https://developer.apple.com/documentation/coremedia/cmvideoformatdescriptioncreate(allocator:codectype:width:height:extensions:formatdescriptionout:)),
and the [AV1 bitstream specification §5.5](https://aomediacodec.github.io/av1-spec/av1-spec.pdf).
