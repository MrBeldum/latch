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
KeyframeRequest on loss ────────────────────▶ next frame is IDR
```

| Crate | What | Platform code |
| --- | --- | --- |
| `latch-proto` | wire format, Reed–Solomon FEC, packetizer and reassembler, input reliability, control framing, an impairment simulator for tests | none |
| `latch-codec` | H.264 Constrained Baseline encoder and decoder, BGRA→YUV conversion, the `lpcm-rice` audio codec | none |
| `latch-engine` | the host side: capture, encode, audio capture, input injection, display sizing, and the session server | `mac/`, `windows/`, `linux/` |
| `latch-stream` | the viewer side: session, decoders (VideoToolbox, Media Foundation, `latch-codec`), audio playback | `video/` |

### Video paths

| Host | Capture | Encode |
| --- | --- | --- |
| macOS | ScreenCaptureKit, NV12 IOSurface at the stream size, cursor included | VideoToolbox (HEVC, H.264 High, or Constrained Baseline), real time, no frame reordering |
| Windows | DXGI Desktop Duplication, BGRA texture; cursor drawn in; Direct3D 11 video processor to NV12 at the stream size | Media Foundation H.264 encoder (the GPU's MFT, else Microsoft's), or HEVC from the GPU's MFT when `LATCH_WINDOWS_HEVC=1` ([WINDOWS.md](WINDOWS.md#the-video-pipeline)); `latch-codec` if neither works |
| Linux (X11) | MIT-SHM `GetImage` through `x11rb`, cursor from XFixes | `latch-codec` software encoder, threads by slice |

| Viewer | Decode |
| --- | --- |
| macOS | VideoToolbox (HEVC or H.264) |
| Windows | Media Foundation H.264 decoder (DXVA when present), with `latch-codec` behind it |
| Linux | `latch-codec` |

The viewer lists what it decodes; the host picks the first it can encode.
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
every frame it was asked for.

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
  are unaffected. The viewer mixes any stream layout onto its device
  (5.1 into headphones, stereo onto the front of a 5.1 set). The encoder
  budget keeps 1.2 Mbps more of the wire for each pair beyond stereo.
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
  same parity as host audio (datagram kind 9). A Linux host makes a
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
* **The bitrate follows the path** (`engine::congestion`). #32's wire-budget
  controller owns adaptation, including the audio/FEC/header allowance.
  Cuts use the lower of current budget and actual successful wire throughput;
  a target above the encoder's output cannot relieve congestion. Host pings
  run at 10 Hz; each report uses the lowest RTT in its window to distinguish
  standing queues from transient jitter. Parity has a base-RTT floor of 15%
  at 40 ms and 20% at 100 ms (30% cap).
* **Send immediately on clean paths.** The bounded two-frame sender remains,
  but its token bucket is disabled until loss or queue pressure arrives.
  Congestion enables the existing 2.5×-budget, sixteen-datagram bucket; two
  clean seconds release it. Raw loss of at least 1.5% also enables pacing,
  including loss FEC repaired, without necessarily cutting bitrate.
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
  captures the main display and lets ScreenCaptureKit scale it.

## Compatibility

Latch 5 viewers speak only protocol 1; Latch 5 hosts speak only protocol 1.
A Latch 5 viewer that finds a Latch 4 host (no `streamer.protocol` in
`/v1/status`) sends it the update it already knows how to send
(`/v1/update`, as a Mac does for Windows hosts today) or says which
machine to update. A Latch 4 viewer finds no GameStream port on a Latch 5
host and offers its own update. Pairing data, Sunshine's configuration and
the engine folders of Latch 4 are removed by Latch 5's setup.

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
with a sixteen-datagram burst. The encoder allowance subtracts audio and
FEC/header overhead from that budget. A session controller cuts by 20% after
two pressure reports, at most once per 500 ms, from the lower of actual wire
throughput and budget, when unrecovered loss exceeds
3%, a frame is lost, the lowest host RTT in the report window rises above its session minimum
by max(10 ms, 25%),
or sender age exceeds 50 ms. After two clean seconds it adds 5% of the
requested ceiling per second, capped at that ceiling. The wire floor is
max(1 Mbps, ceiling/10). Recovered random loss alone does not reduce rate;
legacy viewers' raw loss is tolerated through 3%. Parity is capped at 30%.
Rate setters run between frames when the target changes by at least 5%.

Host and viewer UDP request 8 MiB in both directions through the one
`latch_proto::transport::configure_socket` helper. Readback is logged once per session, including Linux sysctl caps;
macOS requests fall back by halves if the kernel refuses a large buffer.
Status and viewer stats expose requested/applied bitrate, sender queue age,
send errors/retries and effective socket buffers. Bytes count successful
media sends only. ENOBUFS uses the platform constant (10055 on Windows).

Viewer receive uses mio readiness and bounded 64-datagram batches. Completed
frames are handed to decode before waiting for more readiness: waiting at the
end of a receive batch held each complete picture until the next datagram. Video
shard storage is reused with a 4096-buffer pool cap. Decode queues are
bounded by eight frames and 50 ms of age; exceeding either restarts at an
IDR rather than decoding an increasing backlog. A missing initial IDR
triggers recovery on the first complete P-frame, or after 500 ms without
a delivered keyframe. Requests retry at max(100 ms, RTT); host duplicate
requests with the same last-delivered frame are throttled for 250 ms.
