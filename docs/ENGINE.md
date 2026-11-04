# The Latch 5 streaming engine

Latch 4 shared a desktop through Sunshine (C++) and viewed it through
moonlight-common-c (C), OpenH264 (C++) and libopus (C). Latch 5 does both
in Rust, inside the one `latch-host` executable that already runs the
control service and the window.

## Boundary

Everything Latch ships is Rust source compiled by `rustc`. Allowed below
that line, because they are the platform rather than a library we chose:

* operating-system and driver APIs called from Rust: ScreenCaptureKit,
  VideoToolbox, CoreGraphics, CoreAudio (macOS); DXGI, Direct3D 11, Media
  Foundation, WASAPI, SendInput (Windows); the X11 protocol, spoken by the
  pure-Rust `x11rb`, and the PulseAudio/PipeWire server (Linux); Metal,
  Vulkan or DX12 through `wgpu`;
* the GPU vendors' encoders and decoders those APIs reach (NVENC, AMF,
  Quick Sync, Apple's media engine);
* the Windows virtual display driver, which is a driver;
* Rust crates from crates.io that compile no C, C++ or assembler files.
  One of them, `graviola` (the TLS provider for update downloads), holds
  formally verified assembly written as Rust inline `asm!`: `rustc`
  compiles it, no external assembler runs, and it is the only assembly in
  the build.

No crate in the build may compile C, C++ or assembler with `cc`, `cmake`
or `nasm`. `scripts/audit-native.sh` checks the resolved build graph and
CI runs it.

## Shape

```
viewer (latch-stream)                         host (latch-engine, in the control service)
---------------------                         ------------------------------------------
POST /v1/stream, Upgrade ─── TCP 47850 ────▶  service.rs authorizes, engine::server takes the socket
                      ◀── 101 + Accept ─────  picks codec/encoder, starts the pipeline
UDP Hello ──────────────────────────────────▶ binds the viewer's address
                      ◀── Video shards + FEC  capture ─▶ convert ─▶ encode ─▶ packetize
reassemble ─▶ decode ─▶ FrameSlot ─▶ wgpu
                      ◀── Audio + parity ───  loopback capture ─▶ lpcm-rice
cpal ◀─ jitter buffer ◀─ decode
Input (redundant until acked) ──────────────▶ inject (SendInput / CGEvent / XTest)
Pad snapshots (newest, until acked) ────────▶ virtual pads (ViGEmBus / uinput)
                      ◀── PadFeedback ──────  the game's rumble and lights
KeyframeRequest on loss ────────────────────▶ next frame is IDR
```

| Crate | What | Platform code |
| --- | --- | --- |
| `latch-proto` | wire format, Reed–Solomon FEC, packetizer and reassembler, input reliability, control framing, an impairment simulator for tests | socket helpers in `transport` |
| `latch-codec` | H.264 Constrained Baseline encoder and decoder, BGRA→YUV conversion, the `lpcm-rice` audio codec | none |
| `latch-engine` | the host side: capture, encode, audio capture, input injection, display sizing, and the session server | `mac/`, `windows/`, `linux/` |
| `latch-stream` | the viewer side: session, decoders (VideoToolbox, Media Foundation, `latch-codec`), audio playback | `video/` |

### Video paths

| Host | Capture | Encode |
| --- | --- | --- |
| macOS | ScreenCaptureKit, NV12 IOSurface at the stream size and up to the display's refresh, cursor included | VideoToolbox (HEVC, H.264 High, or Constrained Baseline; HEVC Main10 from P010 plumbed), real time, no frame reordering, two frames in flight ([MACOS.md](MACOS.md#sharing-this-mac-the-video-pipeline)) |
| Windows | DXGI Desktop Duplication, BGRA texture; cursor drawn in; Direct3D 11 video processor to NV12 at the stream size | AMF HEVC on AMD, or the GPU's Media Foundation HEVC/AV1/H.264 encoder (Microsoft's CPU encoder for H.264 if needed) ([WINDOWS.md](WINDOWS.md#the-video-pipeline)); `latch-codec` if neither works |
| Linux (X11) | MIT-SHM `GetImage` through `x11rb` of only the rectangles X Damage reports, cursor from XFixes | NVENC or VA-API from NV12 the CPU converts, when a probe at start finds one ([below](#linux-hardware-encoding)); else `latch-codec`, threads by slice |

| Viewer | Decode |
| --- | --- |
| macOS | VideoToolbox (HEVC or H.264) |
| Windows | Media Foundation H.264 decoder (DXVA when present), with `latch-codec` behind it |
| Linux | `latch-codec` |

The viewer lists what it decodes; the host picks the first it can encode,
then fits the request to that encoder (`video::Limits`, reported to
viewers as `streamer.codecs`; see `docs/PROTOCOL.md`, "Encoder limits").
Latch adds no ceiling of its own beyond the request range (16384 pixels a
side, 500 fps, 1 Gbps): the codec's top level, the encoder's own limit
where it has one (VideoToolbox, found by trying; NVENC and VA-API, as
their drivers say), and on the software path the frame rate this CPU was
measured to encode (`software::measure_fps`, held by the server through
`software::ceiling`, 30 fps at 1080p until measured; on a Linux host
whose hardware has some codecs, only the others are held, and the rate is
in their `max_pixel_rate` rather than `max_fps`). Sizes are counted in coded 16-pixel blocks.
`latch-codec` decodes Constrained Baseline only, so a host asked for
`h264-baseline` configures its hardware encoder for that profile. A
Windows viewer asks for Constrained Baseline only, so that when Media
Foundation gives no picture (the first ten access units decide) the
stream continues in `latch-codec` from the next keyframe; a Mac viewer
does the same for Baseline streams.

The decoder's reconstruction kernels were adapted from the MIT-licensed
`pith-h264` (see `crates/codec/src/h264/decode/SOURCE.md`); everything it
refuses (CABAC, B slices, 8×8 transforms, interlace) was removed rather
than left unreachable.

The software encoder is built for desktops, not films: a macroblock whose
source pixels did not change since the last frame costs a skip and no
search; changed macroblocks get a small motion search seeded by their
neighbours and a scroll detector, and intra prediction. Rate control is a
per-frame QP from a one-frame VBV, so a frame never takes much longer than
its interval on the wire. Its hot loops (colour conversion, SAD, residual
reconstruction, deblocking) use NEON or SSE/AVX2 through `std::arch`, and
a governor keeps each frame's encode under 65% of the frame interval by
trading search effort, not frames, so a slow or busy host still sends
every frame it was asked for. The Linux host hands it what X Damage says
changed: only those rectangles are read from the X server and converted
(split across cores), and outside them macroblocks are skipped without
being compared; the bitstream is the one a full comparison gives.

### Linux hardware encoding

`latch-engine/src/linux/{nvenc,vaapi,hardware}.rs`. Both are loaded with
`libloading` when first needed; nothing is linked, and the structures
mirror the vendors' headers (sizes, offsets, bit positions, GUIDs and
version words checked against nvEncodeAPI.h 12.0 and libva 2.x).

* **NVENC** (`libnvidia-encode.so.1`, `libcuda.so.1`): H.264 Baseline or
  High, HEVC Main, AV1 Main; preset P1 with ultra-low-latency tuning, CBR
  with a one-frame VBV, no B-frames, an infinite GOP with IDRs (and
  parameter sets) on request, BT.709 VUI; bitrate changes by
  reconfiguration.
* **VA-API** (`libva.so.2`, `libva-drm.so.2`, for each codec the first
  render node whose driver opens its encoder, so an Intel iGPU and an AMD
  card each give what they have): H.264 Constrained Baseline or High and
  HEVC Main, the low-power entry point first, driver-written headers, one
  reference (marked `RPS_ST_CURR_BEFORE` for HEVC, as FFmpeg does), CBR
  with a one-frame HRD, the level from size, rate and bitrate (HEVC's High
  tier where Main's bitrate is too low). Without packed headers HEVC has
  no conformance window, so it codes only sizes in whole minimum coding
  blocks (8 pixels or the driver's): 1366 × 768 or 1600 × 900 are not
  offered HEVC. AV1 is not offered: VA-API leaves its headers to the
  application. The host's user must be able to open the node (the
  `render` group on Ubuntu); the log says so when it cannot.

Each backend and codec is probed once per process, on the host's
capacity thread when its service starts (open, an IDR that must carry its
parameter sets, a P-frame), and what it takes is asked of the driver then
(NVENC's maximum width, height, macroblocks and macroblock rate; VA-API's
maximum picture size and HEVC block sizes); anything missing or failing
leaves `latch-codec`. `LATCH_LINUX_ENCODER=auto` (default) `| software |
nvenc | vaapi` chooses; under `auto` a viewer that asks for Constrained
Baseline keeps `latch-codec`, whose output that decoder is tested
against, and hardware Baseline is not probed. A codec is given up for the
process only when an open fails in a way that lasts (no such profile or
entry point, a driver that does not initialize, a node the user may not
open, NVENC's device and version errors); a GeForce card's session limit,
memory or a size the encoder does not take fail only that open. H.264
then encodes in `latch-codec`, for that session or that size; a codec
that will not open when a session starts gives way to the next one the
viewer takes, and one that cannot go on after the desktop is resized
(`video::Unencodable`) ends the session as `ENCODER_FAILED`, which the
viewer reconnects from to negotiate another. Neither backend has run on a
GPU yet: the Linux host used for testing has libva and a virtio render
node its user may not open, so only the fallback has run.

**Wayland** is not captured. It takes the ScreenCast portal (D-Bus, which
pure Rust can speak) and a PipeWire stream, whose Rust bindings are
generated from and compiled against libpipewire's C headers, outside the
boundary above; the portal also asks a person to approve each session,
which an unattended host cannot. Under a Wayland session only Xwayland
windows are seen.

### Audio

48 kHz stereo 16-bit, 10 ms frames, coded losslessly with `lpcm-rice`
(fixed linear prediction and Rice codes, like FLAC's fixed predictors).
Silence costs a few bytes a frame and a desktop's sound about 0.5–1 Mbps;
coding it costs microseconds. The decoded samples are exactly the
captured ones, which lets tests check audio bit for bit. Opus is gone.

### Gamepads

A viewer that offers gamepads gets as many slots as the host can emulate
(`Accept.gamepads`) and sends each pad's whole state whenever it changes;
the host plugs a virtual pad in on a slot's first state, applies only the
newest state of each slot, and unplugs every pad when the session ends.
Rumble and light-bar colours come back the same way ([PROTOCOL.md](PROTOCOL.md#gamepads-912)).

| Host | Virtual pads |
| --- | --- |
| Windows | ViGEmBus (installed by setup, [WINDOWS.md](WINDOWS.md#gamepads)), spoken to through `DeviceIoControl`: Xbox 360 pads (XInput) and DualShock 4 pads, up to 8; rumble, the player LED and the light bar come back from the driver's notifications |
| Linux | `/dev/uinput` devices identical to what `xpad` makes for an Xbox 360 pad, up to 16, with `FF_RUMBLE`; needs write access to `/dev/uinput` ([deploy/native/README.md](../deploy/native/README.md#gamepads)) |
| macOS | none: a Mac cannot add a gamepad without a driver, so `Accept` says why in `gamepad_problem` |

| Viewer | Reading pads |
| --- | --- |
| macOS | GameController (Xbox, DualSense, DualShock 4, Switch Pro, MFi), value-changed handlers on Latch's own dispatch queue; rumble through Core Haptics, an engine per handle and trigger motor |
| Windows | Windows.Gaming.Input polled every millisecond: `Gamepad` (with trigger motors), and Sony pads as `RawGameController` in their DirectInput layout |
| Linux | evdev, a thread per pad blocked in `read`; `FF_RUMBLE`; the host's own virtual pads are skipped |

A pad keeps its slot while it is connected, across streams. A test-pattern
host (`LATCH_TEST_PATTERN`) plugs pads into an in-memory bus that rumbles
back each pad's triggers, unless `LATCH_TEST_GAMEPADS=system` sends them to
the platform's own. None of this compiles C: ViGEmBus's IOCTL layouts are
declared in Rust from its MIT-licensed headers, and evdev/uinput are ioctls
through `libc`.

## Decisions

* **No pairing.** The control service's authorization (same Tailscale
  account, or the tailnet of a tagged host) already guards `/v1/update`,
  which replaces the executable. A stream needs no stronger check. The
  session token travels inside WireGuard.
* **UDP + FEC + keyframe on loss**, as Moonlight did, without RTSP, ENet or
  a second encryption layer. Input is reliable by redundancy, not timers.
* **Newest frame wins** on the viewer (`FrameSlot`), every frame is decoded
  (P-frames depend on each other), and a lost frame stops decoding until a
  keyframe.
* **Frames on a fixed grid.** Every pipeline takes frames on a grid 1%
  faster than the asked-for rate (`video::Pacer`): capture and encode time
  comes out of the interval instead of adding to it, a screen refreshing at
  the stream's rate gives up every refresh, and a late frame moves the
  grid rather than bursting.
* **The bitrate follows the path** (`engine::congestion`). One controller
  owns the wire budget, including audio, FEC and headers. It starts at the
  requested ceiling. New viewers report every 50 ms; display statistics retain 250 ms windows.
  Host pings run at 50 Hz.
  Two pressure windows trigger cuts; one does when under half of what was
  sent arrived and the rest is lost or queued (a capacity drop). Queue cuts
  repeat no faster than max(50 ms, base RTT), loss cuts only after a base RTT
  plus a report window. A smoothed positive delay gradient detects filling
  queues; sustained delay above max(6 ms, 10% of base RTT, 3× measured ping
  jitter) also counts, so 5–15 ms of Wi-Fi jitter is not a queue. Recovered
  random loss alone does not cut bitrate; uncorrected loss or lost frames do,
  at any raw loss (a policer). Parity has a base-RTT floor of 15% at 40 ms and 20%
  at 100 ms (30% cap), with fast attack and a two-second decay after bursts.
* **Send immediately on clean paths.** The bounded two-frame sender remains,
  with no token-bucket delay on a clean path. Queue or severe sustained loss
  enables the 2.5×-budget, sixteen-datagram bucket. Radio loss repaired by FEC
  does not enable pacing. Clean feedback releases pacing after at least
  200 ms (two report intervals for slower peers).
* **Data before parity.** A frame's data shards go out before any parity
  is computed, so a frame that loses nothing never waits for FEC.
* **Media threads at high priority** (user-interactive QoS on macOS,
  highest priority on Windows) and media sockets with 8 MB buffers, so a
  keyframe's burst is not dropped by the sender or the receiver.
* **Sound stays bounded.** #32's viewer keeps its 40 ms preroll and existing
  smoothed ±0.2% clock steering, with a 120 ms hard cap trimmed to 50 ms.
  A 30-minute ±300 ppm simulation verifies no gaps or trims and 25–45 ms
  mean queue depth. This already bounds latency; no second controller is added.
* **A Mac viewer never copies a picture**: VideoToolbox decodes into an
  IOSurface that the renderer samples as Metal textures.
* **One viewer at a time.** A new authorized viewer takes the host over;
  the old one gets `End` code 1.
* **Display sizing** stays where it was: a Linux desktop that follows its
  viewer is resized by the engine (`xrandr` semantics through RandR) or by
  the owner's prep command; Windows sets the virtual display's mode; macOS
  captures the main display (or `LATCH_MAC_DISPLAY`) and lets
  ScreenCaptureKit scale it.

## Compatibility

Latch 5 viewers speak only protocol 1; Latch 5 hosts speak only protocol 1.
A Latch 5 viewer that finds a Latch 4 host (no `streamer.protocol` in
`/v1/status`) sends it the update it already knows how to send
(`/v1/update`, as a Mac does for Windows hosts today) or says which
machine to update. A Latch 4 viewer finds no GameStream port on a Latch 5
host and offers its own update. Pairing data, Sunshine's configuration and
the engine folders of Latch 4 are removed by Latch 5's setup.

Gamepads are negotiated inside protocol 1: a viewer before them never
asks (`StreamRequest.gamepads` defaults to false), and a host before them
never answers with slots (`Accept.gamepads` defaults to 0), so neither side
ever sends datagram kinds 9–12 to a peer that does not know them.

## Evidence

What shows the engine is right, other than its authors:

* the ITU-T H.264.1 conformance bitstreams: the decoder's output must equal
  each stream's reference YUV byte for byte;
* VideoToolbox and FFmpeg as independent decoders of the encoder's
  output, which must equal the encoder's own reconstruction byte for byte;
* known-pixel test patterns through the whole pipeline, compared after
  decode;
* bit-exact audio through the whole pipeline;
* the impairment simulator (loss, bursts, reordering, duplication, delay)
  for the transport, and measured loss on the real campus Wi-Fi path;
* input-to-photon timing measured on the viewer's own clock: the viewer
  presses a key, a pattern on the host changes colour, the viewer times
  until it decodes the change;
* CPU, memory and energy of host and viewer, measured against Latch 4 at
  the same size, frame rate and bitrate;
* `scripts/audit-native.sh`: no C, C++ or assembler in the build graph.

## Cadence and transport budgets

All host pipelines use the same start-to-start deadline scheduler. Work time
is inside the interval; a deadline missed by a whole interval resets the grid to the
actual capture time, without catch-up trains (`video::Pacer`). Idle repeats remain bounded at 100 ms.
Encoding feeds a two-frame bounded channel to a dedicated video sender;
backpressure preserves encoded references. Cancellation may discard queued
media. Audio, input acknowledgements and pings bypass the video queue.

Under pressure video uses a wire-byte token bucket at 2.5 times the wire budget,
with a sixteen-datagram burst. The encoder allowance reserves audio separately,
then compounds header overhead with rounded FEC rather than adding the two.
Cuts use the lower of current budget and actual successful send rate, bounded
by received throughput +7% (a standing FIFO delivers its bottleneck rate; a
viewer without `received_kbps` is credited the offered rate less its loss).
Each cut removes 20%, or 40% for queue delay above 20 ms or loss above 10%.
Loss in a window that delivered more than is now sent predates the last cut
and is ignored, as is a queue still falling within three horizons of a cut
(horizon: 2× base RTT plus one report window, two for windows ≥200 ms). Sender age above
20 ms is also pressure. Uncorrected loss above 3% (raw loss for legacy
reports) or lost frames in two windows running count at any raw loss. Loss
without queue delay is treated as a policer: a smoothed received rate (+7%
headers) caps the climb at 97% of it for three seconds, during which one
damaged window near it cuts again. A 50 ms window holds only a few frames, so
received/sent ratios are too coarse to be a signal on their own; they gate only
the one-window collapse cut, together with loss above 10% or delay above 20 ms. The
ping-jitter estimate starts at 3 ms and learns only from falling delay outside
congestion. The wire floor is a fixed 1 Mbps, whatever the ceiling, so a
high-quality session can still fit a 2–5 Mbps link.

A cut remembers the sustainable rate (what got through, raised to whatever the
smoothed received rate later shows). Clean feedback grows the budget at
doubling per 400 ms of actual elapsed time between reports, but only to 97% of
that rate (92% for coarse ≥200 ms windows). Above it, one probe is in flight at
a time: first ~2.5 ms of queue per horizon (1–5%), doubling only while probes
survive a horizon, and stepping up to the rate that last congested before
accelerating past it. A probe whose own feedback shows over 3 ms of queue
drops under the held rate by as much as it went over, so its excess drains as
fast as it built. With no throughput or loss feedback at the cut, there is no
estimate and recovery is purely time-based. Reports more than 2 s apart grant
no growth: the controller waits for fresh feedback. A lossless RTT plateau
that persists for 2 s through a fourfold budget reduction with no local queue
is a route change: the RTT minima are rebased and the estimate dropped; loss or
a falling RTT (a draining FIFO) cancels the evidence. Raw legacy loss under 10%
without lost frames is treated as repaired radio loss. Rate setters run between
frames at a 5% target change.
This remains a heuristic controller; it is not a full GCC implementation.

Linux sends up to 32 datagrams with `sendmmsg` and receives with `recvmmsg`.
macOS receives up to 32 with `recvmsg_x`; sends retain `send_to`, since Darwin's
batch-send API disallows destination addresses on the shared unconnected socket.
Batch storage is reused, and data flushes before parity computation. Batches
never wait to fill. Unsupported batch receive syscalls fall back to scalar I/O
on the nonblocking media socket. Windows retains its existing scalar socket
path; USO/URO and Linux UDP GSO/GRO are not implemented here. Protocol 1 retains
the 1200-byte datagram limit; route MTU discovery/negotiation remains outstanding.

Host and viewer UDP request 8 MiB in both directions through the one
`latch_proto::transport::configure_socket` helper. Readback is logged once per session, including Linux sysctl caps;
macOS requests fall back by halves if the kernel refuses a large buffer.
Status and viewer stats expose requested/applied bitrate, sender queue age,
send errors/retries and effective socket buffers. Bytes count successful
media sends only. ENOBUFS uses the platform constant (10055 on Windows).

Viewer receive uses mio readiness and bounded 32-datagram syscall batches on
Linux/macOS (64 scalar receives on Windows). Completed
frames on the batch path are handed to decode after each datagram, before
processing the rest of the batch or waiting for readiness: waiting at the
end of a receive batch held each complete picture until the next datagram. Video
shard storage is reused with a 4096-buffer pool cap. Decode queues are
bounded by eight frames and 50 ms of age; exceeding either restarts at an
IDR rather than decoding an increasing backlog. A missing initial IDR
triggers recovery on the first complete P-frame, or after 500 ms without
a delivered keyframe. Requests retry at max(50 ms, RTT), and at once when
another frame is lost meanwhile: that may be the recovery keyframe itself.
The host (`KeyframeAnswers`) ignores a repeat naming the same last-delivered
frame until a keyframe sent since the request it repeats has been out for a
round trip (smoothed RTT less its variation), or 250 ms have passed, so
duplicates cost nothing and a lost recovery keyframe is replaced about a
round trip later instead of after a 250 ms hold. Requested
IDRs are gated at max(50 ms, smoothed RTT), including requests that advance
the last-delivered frame. See [transport measurements](measurements/2026-11-03/transport/README.md)
for codec-free throughput, CPU, FEC and impairment results and remaining targets.
