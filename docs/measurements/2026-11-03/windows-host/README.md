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
