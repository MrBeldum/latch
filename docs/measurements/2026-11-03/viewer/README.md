# Viewer decode and renderer preparation, 2026-11-03

Workstream C, MacBook Pro M3 Pro and Windows 11 / RX 6600. The baseline is
9fe0ae9 with only the native measurement example/timestamps added. Tests use
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
interval. Shared-texture import would remove the staging round trip.

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

- Windows shared D3D11/D3D12 textures and fences are not implemented. DXVA
  decode still uses staging readback followed by wgpu upload. No claim that
  decode/display can never bottleneck at any resolution or rate is justified.
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

## API references

The implementation follows [Microsoft's HEVC decoder media-type and low-latency
contract](https://learn.microsoft.com/en-us/windows/win32/medfound/h-265---hevc-video-decoder),
[Apple's format-description API](https://developer.apple.com/documentation/coremedia/cmvideoformatdescriptioncreate(allocator:codectype:width:height:extensions:formatdescriptionout:)),
and the [AV1 bitstream specification §5.5](https://aomediacodec.github.io/av1-spec/av1-spec.pdf).
