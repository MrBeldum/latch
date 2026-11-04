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
RecoveryRequest on loss ────────────────────▶ next frame predicts from the last delivered
                      ◀── RecoveryAck ──────  (or refused: IDR)
KeyframeRequest (fallback, older hosts) ────▶ next frame is IDR
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
| macOS | ScreenCaptureKit, NV12 IOSurface at the stream size and up to the display's refresh, cursor included | VideoToolbox (HEVC, H.264 High, or Constrained Baseline; negotiated HEVC Main10 from P010), real time, no frame reordering, two frames in flight ([MACOS.md](MACOS.md#sharing-this-mac-the-video-pipeline)) |
| Windows | DXGI Output5 Desktop Duplication (BGRA or scRGB FP16); GPU cursor/colour pass and D3D11 video processor to NV12/P010 | AMF HEVC on AMD, or the GPU's Media Foundation HEVC/AV1/H.264 encoder (Microsoft's CPU encoder for H.264 if needed) ([WINDOWS.md](WINDOWS.md#the-video-pipeline)); `latch-codec` if neither works |
| Linux (X11) | MIT-SHM `GetImage` through `x11rb` of only the rectangles X Damage reports, cursor from XFixes | NVENC or VA-API from NV12 the CPU converts, when a probe at start finds one ([below](#linux-hardware-encoding)); else `latch-codec`, threads by slice |

| Viewer | Decode |
| --- | --- |
| macOS | VideoToolbox (hardware-probed AV1, HEVC Main/Main10 or H.264), IOSurface → Metal; NV12, or P010 for a negotiated ten-bit stream (HDR drawn by the native EDR layer where available) |
| Windows | Media Foundation + D3D11/DXVA for hardware-probed AV1, HEVC Main/Main10 and H.264 High; Baseline with `latch-codec` fallback. Shared NV12 D3D11 texture → D3D12/wgpu; staging readback when unavailable and for ten-bit P010 (drawn by the native HDR10 swapchain where available) |
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
viewer probes platform codecs before offering AV1 > HEVC > H.264 High >
Baseline. Windows probes actual pictures through the installed synchronous
MFT and D3D11 manager, caching the result; missing codec extensions or drivers
leave that codec out of negotiation. Mac checks `VTIsHardwareDecodeSupported` and requires a decoded hardware
picture too; a capability query alone can succeed on an unusable decoder.
Only Baseline streams have the ten-access-unit `latch-codec` safety net.
High, HEVC and AV1 have the session's instead (`session::HARDWARE_PROBATION`):
a decoder that cannot be created, gives no picture for ten access units, or
fails three times without one ends that attempt, and the session asks the
host again without the codec (down to Baseline), with a notice saying why;
the codec is not offered again until the viewer restarts.
`LATCH_TEST_BREAK_DECODER=hevc,h264-high` (or `all`; `hevc=silent` for a
decoder that gives nothing) breaks decoders on purpose to test this.
Unknown codec names in an offer are skipped by protocol-1 hosts.

Main10 input is supported by the platform decoders. Mac callers can choose
P010 with `new_decoder_with_format`; its two 16-bit Metal planes retain ten
bits. The normal session still requests NV12/SDR. Windows accepts NV12 or
P010 MFT output, reducing P010 to eight bits for the current SDR renderer.
With a D3D12 renderer on the same adapter, NV12 pictures use a bounded pool
of shared D3D11 textures opened as wgpu plane views. A D3D11 fence completes
the GPU copy before publication; the renderer retains each surface through
GPU completion before the decoder can reuse it. Headless tools, P010, odd
picture sizes, pool exhaustion and unsupported sharing use staging readback.
A failed import disables sharing for subsequent pictures.
Neither path negotiates HDR, tone maps, or changes the display's colour mode.

The client requests `AutoNoVsync` and maximum frame latency 1, wakes egui on
every published picture, and samples the newest `FrameSlot` in the paint
callback. There is no frame timer or 60 Hz cap. Mac native buffers are retained
until previous GPU submissions complete, rather than for an arbitrary three
frames. This retention is not a presentation queue. The compositor, VRR and
scanout still determine physical display latency; immediate mode may tear.
`cargo run --release -p latch-client --example present -- --port 47882`
measures decode completion to renderer preparation on the viewer clock;
it does not measure GPU completion, OS present or photons.

Linux remains Baseline software decode. VA-API needs a Rust bitstream-to-VA
picture/slice parameter path (loading libva alone is not a decoder), plus
synchronized dma-buf import into wgpu. It is deferred; no native codec
library or C/C++ build is added.

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

VideoToolbox LTR recovery is runtime-gated (the low-latency mode is opt-in
with `LATCH_MAC_LTR=1`, and a session that stalls or misses its pace reopens
without it). Windows H.264 MFTs must pass a loss/decode self-test at open
before advertising recovery; the engine reuses `latch-stream`'s Media
Foundation decoder for this check. An H.264 refresh is withheld when the
viewer's references would exceed `max_num_ref_frames`, which freezes Apple's
decoder; the Mac viewer also bounds every VideoToolbox decode call. See the
platform documents.

### Audio

48 kHz 16-bit, coded losslessly with `lpcm-rice` (fixed linear prediction
and Rice codes, like FLAC's fixed predictors). Silence costs a few bytes a
frame and a desktop's stereo sound about 0.5–1 Mbps; coding it costs
microseconds. The decoded samples are exactly the captured ones, which
lets tests check audio bit for bit. Opus is gone.

* **5 ms frames, sent as soon as captured.** Captures hand over 5 ms
  frames (240 samples); each is its own packet the moment the capture
  period holds it. WASAPI loopback runs at the endpoint's engine period
  (10 ms on most drivers, and low-latency `IAudioClient3` streams refuse
  the loopback flag), so a Windows host sends two packets every 10 ms.
* **Surround when both ends have it.** A viewer whose output device has
  more than two channels asks for them (`StreamRequest.audio_channels`);
  the host captures the device's own layout up to that (WASAPI channel
  mask, PulseAudio channel map) and sends `lpcm-rice` version 2, 80
  samples a packet for 5.1 and 60 for 7.1 so the worst case always fits
  a datagram. Stereo is still version 1, byte for byte, so older viewers
  are unaffected. The encoder budget keeps 1.2 Mbps more of the wire for
  each pair beyond stereo.
* **Each channel to its own speaker.** Streams carry WAVE order (FL FR FC
  LFE BL BR SL SR); output devices name theirs, and the orders differ
  (CoreAudio's 7.1 is L R C LFE Ls Rs Rls Rrs, sides first; ALSA's 5.1 is
  FL FR RL RR FC LFE). The viewer reads its device's layout (macOS:
  `kAudioDevicePropertyPreferredChannelLayout`, expanded by AudioToolbox,
  or the preferred stereo pair; Windows: the endpoint's mix-format channel
  mask; Linux: the ALSA channel map that PulseAudio's and PipeWire's
  plugins report, else ALSA's default order) and routes every channel to
  the speaker of the same position (`latch_codec::audio::mix_onto`). A
  speaker the device lacks is folded in: back and side surrounds stand in
  for each other (−3 dB each when both land on one), the rest go to the
  front pair at the usual downmix weights (centre −3 dB, LFE −6 dB), and
  a mono device gets the stereo mix's mean. A device that names no
  speakers is taken as Latch's layout of its channel count.
* **Windows output at the engine's smallest period.** cpal's WASAPI
  stream stays at the engine's default 10 ms period. The Windows viewer
  opens the endpoint itself as an `IAudioClient3` shared stream at the
  smallest period the driver offers (2.67–3 ms on drivers that support it;
  others offer only their default), or the period another stream already
  fixed, keeps two periods written ahead (`LATCH_AUDIO_PERIODS`) on an
  MMCSS "Pro Audio" thread, reopens on a lost device or a new default, and
  falls back to cpal on any failure (`LATCH_AUDIO_OUTPUT=cpal` forces
  it).
* **A jitter buffer that follows the path.** The viewer measures arrival
  jitter and capture bursts and steers the least audio left before each
  device pull to a 2 ms margin plus whatever part of a burst the jitter
  does not already spread, trimming playback speed by at most 0.2 % to
  hold it. A real underrun (not the host going quiet: it sends nothing
  for silence) widens the margin by 2 ms, which narrows again by 0.1 ms
  a second. Devices that state their range get a 2.5 ms buffer
  (`LATCH_AUDIO_BUFFER` overrides it, in frames or `default`).
* **The viewer's microphone, opt-in.** With **Send my microphone** on and
  a host that answers with `Accept.mic`, the default input is mixed to
  mono, resampled to 48 kHz and sent as `lpcm-rice` 5 ms frames with the
  same parity as host audio (datagram kind 13). A Linux host makes a
  "Latch Microphone" source itself (a null sink and a source over its
  monitor, through PulseAudio's native protocol or PipeWire's pulse
  server) and removes it after the session. Windows and macOS cannot turn
  sound into a microphone without a driver, so the host plays it into a
  virtual cable's output (VB-CABLE, BlackHole, or the output named by
  `LATCH_MIC_DEVICE`) and programs record the cable's input.
* **Measured glass to glass.** `LATCH_TEST_AUDIO=clicks[:period_ms]` on a
  host replaces its sound with a click on every wall-clock quarter second,
  delivered a device period at a time; `LATCH_AUDIO_PROBE=1` on the viewer
  notes when each click reaches the speaker (callback time plus the
  device's own playback delay) and `view` reports them as
  `audio_click_ms`. `LATCH_AUDIO_TRACE` on a Windows host logs loopback
  packet sizes and QPC ages.

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
* **UDP + FEC + a recovery frame on loss**, as GeForce NOW does, keyframe
  where the encoder cannot, without RTSP, ENet or a second encryption layer.
  Input is reliable by redundancy, not timers.
* **Newest frame wins** on the viewer (`FrameSlot`), every frame is decoded
  (P-frames depend on each other), and a lost frame stops decoding until a
  recovery frame that predicts from the last frame delivered, or a
  keyframe. The software encoder makes one by rolling back to that picture
  (bit-exact), AMF from a long-term reference; the others answer with an
  IDR. Recovery maps pair pipeline generation with picture identity, so
  queued frames cannot alias a reference after capture reconstruction
  ([PROTOCOL.md](PROTOCOL.md#recoveryrequest-14-and-recoveryack-15)).
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
* **Sound stays bounded, and short.** The fixed 40 ms preroll is gone:
  the jitter buffer above holds what the measured path needs, with the
  same ±0.2 % clock steering and a 120 ms hard cap trimmed back to the
  current target. The 30-minute ±300 ppm simulation still has no gaps and
  no trims, at about 27 ms buffered on its old jittery path and 11–15 ms
  on a quiet LAN-like one. No second controller is added.
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

## HDR and ten-bit colour

Protocol 1 negotiates colour separately from codec. Missing capabilities keep
8-bit limited-range BT.709. HEVC can select SDR Main10 or HDR10 (BT.2020 NCL,
PQ, P010); the pipeline reports its actual colour before Accept. A colour
change after Accept ends the session for renegotiation. Every HDR IDR has
ST 2086 and content-light prefix SEI, independent of the encoder API.

Windows retains scRGB from DuplicateOutput1 for Advanced Color desktops.
The cursor/rotation shader converts linear scRGB (80 cd/m² per unit) to a
1000-nit BT.2020 PQ RGB10 target for HDR, or sRGB RGB10 for ten-bit SDR;
the video processor converts that to P010 with explicit DXGI colour spaces.
For an SDR stream from an HDR desktop the same pass maps wide-gamut colours
into BT.709 toward their own luminance and rolls highlights up to the
1000-nit ceiling off above 75% of SDR white on max(R,G,B), keeping hue
(`latch_proto::color::hdr_to_sdr`). A failed Output5 acquisition
does not silently retry the legacy BGRA conversion that can produce black.

Viewers retain P010 through decode and upload. macOS uses a native float
Metal sublayer for HDR video because egui 0.31 fixes its main framebuffer
to BGRA8; ten-bit SDR goes through the wgpu P010 shader. HDR is rendered in extended linear Display P3, with a smooth
highlight shoulder at the display's current EDR headroom. Windows uses a
10-bit child swapchain: G2084/P2020 for an HDR display and PQ stream, G22/709
for SDR Main10. PQ falls back to a GPU SDR tone map only when native HDR
cannot be presented, never because egui draws over the picture: the Mac
layer sits below egui's, which is cleared to transparent over the video,
and Windows cuts the child window around everything egui draws above the
background layer. Native renderers sample chroma at the colour
description's siting (`chroma`, left by default). Eight-bit SDR keeps the
existing rendering path. Which presenter draws is decided per stream (and
again when the window's display changes and the change has held for
300 ms, at most once a second), never per frame. A display that cannot be
read for a moment is no change. A native
presenter that fails hands that stream to wgpu instead of being retried
frame by frame, which alternated surfaces (a flicker).

See [colour measurements](measurements/2026-11-03/hdr/README.md) for tests,
measurements and platform limitations.
