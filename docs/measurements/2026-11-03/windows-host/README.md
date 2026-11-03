# Windows host, RX 6600 → M3 Pro Mac

Windows 11, Radeon RX 6600, VDD 11.30.4.434, Tailscale to the development
Mac. Baseline is main `9fe0ae9`; baseline HEVC was explicitly enabled with
`LATCH_WINDOWS_HEVC=1` so this compares the hardware HEVC paths. The original
baseline display list offered at most 120 Hz. The new list adds 240/360 Hz
at 1080p and 1440p. All viewer runs ask for 50,000 kbps (transport reserves
its FEC/header/audio allowance), HEVC, and last eight seconds. Tables average
`seconds[2:]`, excluding the first two seconds of startup. These are decoded
frame rates, not presentation rates or input-to-photon measurements.

## Reproduce

Build `cargo build --locked --release -p latch-engine --examples` on Windows
and `cargo build --locked --release -p latch-stream --example view` on the Mac.
Run the Windows host in the signed-in interactive session through a scheduled
task, hidden with a wscript wrapper (SSH is session 0):

```powershell
$env:RUST_LOG='latch_engine=info'
# Leave unset for the default; 'mf' forces Media Foundation for comparison.
$env:LATCH_WINDOWS_ENCODER='amf'
.\target\release\examples\serve.exe --bind 100.106.23.40 --port 47880 --telemetry --prepare-display
```

Keep a continuously animated window on the desktop: a WPF canvas with an
80×240 yellow rectangle moving across a blue 400×300 window over one second,
auto-reversed and repeated indefinitely. The earlier and final matrices use
the same animation. This is desktop motion, not a game or full-screen noise.
On the Mac, repeat for every table mode:

```sh
target/release/examples/view 100.106.23.40 --port 47880 \
  --width 1920 --height 1080 --fps 240 --kbps 50000 --seconds 8 --hevc
```

`host_ms` comes from each frame's capture→send telemetry, averaged by the
viewer. The baseline stamp was taken after `Capture::grab`; the new stamp is
taken immediately after `AcquireNextFrame`, before GPU copy/composition.
Neither includes the time between a game's rendering and DXGI acquisition.
Network RTT/loss affect achieved fps and keyframe recovery; they are recorded
in the JSON and must not be mistaken for encoder throughput.

For capture→encode throughput without the network, in the interactive task:

```powershell
$env:LATCH_WINDOWS_ENCODER='mf' # repeat with 'amf'
.\target\release\examples\windows_capture.exe 6 NUL 1920 1080 240 50000 hevc
```

This example switches the display and restores its previous topology on exit.
It encodes idle repeats too (`idle=0`), so it measures the capture/encode loop
rather than the viewer's delivered fps. Do not compare its latency directly
with `host_ms`, which also includes packetization/send queueing.

## Driver setup and restoration

Only Latch's generated setup step writes driver XML. Dump it with
`LATCH_DUMP_VDD_SCRIPT` and the host unit test
`the_step_lists_every_mode_keeps_the_file_and_restarts_only_on_change`, then
run the dumped script elevated. The fixture test in
`scripts/test-virtual-display.ps1 -SetupScript <dumped-script>` verifies
custom timing preservation, repeated setup, and rejected-list rollback.

The driver accepted 98 entries including 1080p/1440p at 360 Hz and 8K sizes.
The full resolution/rate cross-product made the monitor disappear; setup
rolls back that failure. Seeds are therefore not a complete list of every
mode the driver can support. Latch enumerates all modes supplied by the
driver without filtering custom rates, but arbitrary requested modes still
need provisioning through setup. This portion of the original brief remains
incomplete.

Tests use only port 47880 and `LatchGfnA-*` tasks, never the installed host or
`LatchInput`. The original XML SHA-256 is
`A720F0A1BA5C740EEAE0CDD055A9E30754DA56B0DF049768454314641EB5591B`.

## Capability reporting after #45

This branch does not import #45's shared settings/status types. After #45
merges, add a Windows branch in `latch_engine::video::limits(codec)` returning
`crate::windows::limits(codec)` (currently every non-Mac codec returns the
codec-level ceiling). `host::streamer::capacity` already calls that function
for each entry in `video::encodable()`, then publishes `streamer.codecs`.

For AMF HEVC, query `AMFComponent::GetCaps` on the capture adapter in an MTA:
`GetInputCaps` supplies width/height ranges; `HevcMaxBitrate` is bits/sec;
`HevcMaxThroughput` is 16×16 macroblocks/sec. Convert the latter to a pixel
rate with `*256`, and divide by `ceil(width/16)*ceil(height/16)` for the
hardware's rate at a requested size. `amf::max_fps` already queries it to
avoid configuring rate control above the encoder's hardware rate (capture
is not throttled). Do not substitute observed desktop fps as a hard cap:
content, display/compositor, and networking can lower it. Query on the
selected adapter, cache outside streaming, and invalidate on adapter changes.
The RX 6600 probe returned widths 128–8192, heights 128–4352,
`HevcMaxThroughput=3626903`, `HevcMaxBitrate=1000000000`, max level 186
(HEVC 6.2), one hardware instance and 16 streams. That maps to
`max_side=8192`, `max_pixels=35651584`, `max_pixel_rate=928487168`, and
`max_kbps=1000000` before intersecting codec level limits. Rounded down at
macroblock boundaries, the rate is 444 fps at 1080p, 251 fps at 1440p, and
111 fps at 4K. These are driver-advertised ceilings, not promised throughput.

Keep MF/non-AMD codecs unknown or separately probed rather than using RX 6600
constants for every Windows host. AV1 must remain absent on this GPU.

## Limits of the evidence

- Hardware AV1 negotiation/framing compiles and has unit coverage, but the RX
  6600 has no AV1 encode MFT; no AV1 end-to-end claim is made.
- LTR and intra-refresh probes do not implement loss recovery. Main10 probing
  does not implement HDR capture, signaling, or presentation.
- Physical displays, NVIDIA/Intel encoders, device-loss fallback, HDR desktop
  capture, rotations, and game workloads have not been revalidated here.
- The 100 ms idle repeat policy remains. No frame-rate whitelist or 240→120
  mapping remains in the Windows streaming selector; seeded modes and actual
  encoder/display limits still bound achievable rates.

## Reference and HDR probe results

`cargo test --locked -p latch-engine --lib hardware_amf -- --ignored --nocapture --test-threads=1`
must run in the interactive session. On the RX 6600:

- LTR: AMF opened with one LTR slot, reported frame 0 marked at index 0,
  and reported reference bitfield 1 on frame 6 when forced to use that LTR.
  The encoder made a P-frame, not an IDR, for that forced reference. This
  establishes encoder-side marking/use, but no decoder loss/drop recovery
  experiment or recovery protocol is implemented.
- Intra refresh: the encoder opened with 64 CTBs per slot and emitted one
  initial IDR followed by seven non-IDRs. This is weaker evidence: it does
  not verify the encoded CTB refresh pattern or prove recovery after loss.
- Main10: AMF accepted P010 with 10-bit color depth and Main10 profile, and
  emitted an IDR with SPS `general_profile_idc=2`. The surface was a probe,
  not an HDR image; color metadata, fidelity, and HDR presentation remain
  untested and unimplemented in this streaming path.

The earlier MF property probes in `capabilities.txt` expose LTR controls for
HEVC and accept Main10 output negotiation. H.264 setter success and
`IsModifiable=E_NOTIMPL` are not treated as proof of usable recovery features.

For ABI/property definitions see AMD's [AMF HEVC header](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/public/include/components/VideoEncoderHEVC.h)
and [capability interfaces](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/public/include/components/ComponentCaps.h).

## Local MF versus AMF, final production configuration

Same checkout, six seconds per mode, no network. MF was measured first;
these are sequential runs on a shared PC, not interleaved repetitions.

| Requested | MF fps | AMF fps | MF capture→encode p50 ms | AMF p50 ms |
| --- | ---: | ---: | ---: | ---: |
| 1920×1080 @ 60 | 57.1 | 59.5 | 4.93 | 2.93 |
| 1920×1080 @ 120 | 105.7 | 118.3 | 6.16 | 2.91 |
| 1920×1080 @ 240 | 111.8 | 202.0 | 6.23 | 2.92 |
| 2560×1440 @ 120 | 82.6 | 117.9 | 8.75 | 4.68 |
| 2560×1440 @ 240 | 82.6 | 157.4 | 9.43 | 4.69 |
| 3840×2160 @ 60 | 53.4 | 58.6 | 15.47 | 9.68 |
| 3840×2160 @ 120 | 56.4 | 90.2 | 14.88 | 9.69 |

The direct path measurably beats MF here, so AMD HEVC defaults to AMF.
The unpaced two-input encode-only test reached 413/233/108 fps on changing
noise at 1080p/1440p/4K. Full desktop capture/encode is slower; reaching the
isolated encoder throughput in a real stream is not established.

The unpaced capture-only diagnostic (`hardware_capture_rate`, three seconds
per mode on the same animated desktop) reached 223.0 updates/s at 1080p240,
191.0 at 1440p240, and 107.5 at 4K120, with no conversion, encoder, network,
or Latch pacer. It still includes the retained desktop GPU copy and cursor
composition. This bounds what this DXGI/DWM workload supplied during the
run, rather than proving a hardware ceiling or explaining every remaining
capture/encode cost. `hardware_pipeline_keyframes` passed 96 frames with a
forced IDR every seven frames at a deliberately overloaded 1000 fps grid.

## Main → final Mac viewer results

| Requested | Before fps | After fps | Before capture→send ms | After ms |
| --- | ---: | ---: | ---: | ---: |
| 1920×1080 @ 60 | 60.1 | 59.8 | 4.42 | 2.99 |
| 1920×1080 @ 120 | 113.8 | 118.9 | 4.93 | 3.02 |
| 1920×1080 @ 240 | 115.3 | 217.3 | 4.41 | 3.00 |
| 2560×1440 @ 120 | 100.9 | 102.1 | 5.82 | 4.76 |
| 2560×1440 @ 240 | 115.7 | 213.4 | 7.54 | 4.73 |
| 3840×2160 @ 60 | 61.1 | 59.1 | 13.74 | 9.75 |
| 3840×2160 @ 120 | 76.3 | 96.2 | 13.07 | 10.00 |

Raw final viewer samples are `final-*.json`; `comparison-final.json` is the
aggregate. Earlier single-input AMF samples remain in `after-*.json` and
`comparison.json`. The continuation's first pipelined pass is retained in
`pre-preset-summary.json`: it was substantially slower and variable
(1080p240 148 fps, 1440p240 71 fps) before the preset-order correction and
while the shared machine/path differed. This is not an interleaved or
controlled A/B benchmark: the final rerun cannot attribute all fps changes
to that correction. RTT averaged 164–172 ms in the final pass. Mean reported
loss was 0.96% at 1440p120 and 1.67% at 4K120; other rows reported zero.
The local MF/AMF comparison and hardware-only probes are the stronger
evidence for choosing AMF; these viewer numbers show actual delivery on
this path, including variation and congestion behavior.

## Validation and final state

See `validation.txt`. Both platforms passed workspace clippy and tests; the
Windows engine tests and interactive IDR/capture probes were rerun after the
last production change. `scripts/audit-native.sh` passed. Initial Windows
Rust 1.98 compiler crashes were resolved by disabling incremental builds and
reducing jobs; no code workaround or toolchain change was introduced.

The original VDDPATH absence, original XML hash, and 800×600@60 display were
restored. The temporary protected XML and test firewall rules were removed,
and no `LatchGfnA-*` task or test process remained. The deployed host stayed
at PID 6084 on TCP 47850 throughout. `restoration.txt` and
`restored-modes.txt` record the verification. Nothing was deployed or merged.
