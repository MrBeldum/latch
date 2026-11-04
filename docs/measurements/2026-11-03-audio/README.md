# Audio latency, surround and bandwidth, 2026-11-03

Before is `a622ff6` (5.1.0's audio path plus the measurement tools: 10 ms
capture reads, a fixed 40 ms preroll); after is this branch.

## Glass to glass on the Windows PC

Host and viewer both on the Windows PC (RX 6600; default output **Steam
Streaming Speakers**, a virtual device): `serve` on 127.0.0.1:47886 with
`LATCH_TEST_PATTERN=still LATCH_TEST_AUDIO=clicks` (a click every
wall-clock quarter second, in 10 ms periods as WASAPI loopback delivers
them), and `LATCH_AUDIO_PROBE=1 view 127.0.0.1 --port 47886 --width 640
--height 360 --fps 30 --kbps 4000 --seconds 30 --audio --mute`, run from a
scheduled task in the signed-in session. Builds interleaved, 75–120 clicks
a run. Three other workstreams were compiling on the PC throughout.

| `audio_click_ms` p50 (p95) | before | after |
| --- | --- | --- |
| round 1 | — (the baseline tree was not at `a622ff6`) | 79.9 (85.5), 90.8 (104.4), 77.7 (82.5) |
| round 2 | 90.3 (100.3), 84.8 (87.7) | 83.4 (96.1), 68.8 (75.8) |
| round 3 | 83.7 (86.9), 85.2 (87.6), 88.8 (89.0) | 90.4 (101.0), 82.7 (87.7), 83.3 (86.0) |

Round 2's first runs are left out: the baseline's lost its stream and this
branch's held an 80 ms queue while another test build of this branch
shared the CPU.

**No measurable change on this PC.** The branch's buffer settles at 20–40
ms here (median of the per-second "queued"), close to the old fixed 40 ms
preroll, because the jitter it measures on this loaded machine is 5–6 ms
on top of 10 ms bursts; and the output device adds 41–51 ms of its own
that no buffer setting changes (cpal opens WASAPI in shared mode, where the
period stays the engine's 10 ms whatever is asked for). The Mac viewer,
where CoreAudio does take a 2.5 ms buffer, could not be measured: its lid
was closed, which turns its speakers off (CoreAudio refused even `afplay`).

## Windows output: IAudioClient3 against cpal (2026-11-04)

The same viewer binary (`d3c5298`), interleaved three times: cpal's shared
WASAPI stream (`LATCH_AUDIO_OUTPUT=cpal`, the path above) against the
direct `IAudioClient3` stream keeping two engine periods written ahead (the
default) and one (`LATCH_AUDIO_PERIODS=1`). Same setup as above: `serve`
and `view` on the PC, 127.0.0.1:47886, `LATCH_TEST_AUDIO=clicks`,
`LATCH_AUDIO_PROBE=1 view ... --seconds 30 --audio --mute`, from a
scheduled task in the signed-in session (`RUST_LOG=latch_stream::audio=info`
names the period). The PC's only output endpoint is **Steam Streaming
Speakers**: the monitor's HDMI audio endpoint is not present, and there is
no other device. Its driver offers a single engine period,
`GetSharedModeEnginePeriod` → default = least = most = 480 frames (10 ms),
so the low-latency stream runs at 10 ms here too; what changes is the
buffering around it. Both paths stamp the device delay the same way
(frames written but not yet played by `IAudioClock`, plus
`GetStreamLatency`), so the click times compare.

| `audio_click_ms` p50 (p95), run 1 / 2 / 3 | median "queued" | device |
| --- | --- | --- |
| cpal: 83.7 (87.1) / 92.2 (106.3) / 67.5 (74.2) | 37.5 / 40.9 / 17.9 ms | 42–43 ms |
| IAudioClient3, 2 periods: **61.1 (63.2) / 65.9 (66.8) / 70.8 (73.6)** | 16.5 / 16.4 / 17.1 ms | 40–44 ms |
| IAudioClient3, 1 period: 61.5 (64.6) / 56.2 (59.2) / 53.5 (71.3) | 16.6 / 16.5 / 18.4 ms | 30–41 ms |

**About 18 ms off the median click (84 → 66 ms), and steadier:** the
jitter buffer holds 16–17 ms in every run instead of 18–41. cpal hands the
callback whatever of its ring is free, so the pulls come in uneven sizes
and the buffer settles higher. One period ahead is not usable: in runs 2
and 3 nearly every wake (3 030 of 3 030, 3 031 of 3 034) found the
device's buffer empty, so the sound glitched every 10 ms. Two periods ran
3 041 wakes with one empty, at start. (The closing count is logged only
when the player is dropped before `view` exits, so not every run has it.)

On a device whose driver offers a smaller period (the inbox HDA driver on
most onboard and HDMI audio: 128–144 frames, 2.67–3 ms at 48 kHz) the
stream runs at it and two periods ahead are 5.3–6 ms where cpal keeps
20 ms or more; that could not be measured on this PC.

## Jitter buffer, simulated

`crates/stream/src/audio.rs`'s long-session simulation: 30 minutes of
packets with random delay into a device pulling on its own clock, drifting
±300 ppm from the host's, on fake time. "Buffered" is the mean queued audio
plus the device pull over the last minute, the delay the buffer adds.

```
cargo test --release -p latch-stream --lib -- audio_latency_stays_bounded a_quiet_path --nocapture
```

5.1.0's path (10 ms frames, up to 10 ms of random delay, 512-frame pulls):

| drift | before | after | gaps / cuts |
| --- | --- | --- | --- |
| −300 ppm | 42.3 ms | **26.0 ms** | 0 / 0 both |
| 0 | 45.4 ms | **26.7 ms** | 0 / 0 both |
| +300 ppm | 48.4 ms | **27.2 ms** | 0 / 0 both |

(Before printed 31.6 / 34.7 / 37.7 ms queued; the 10.7 ms pull is added
here so both columns measure the same thing.)

A quiet LAN (1 ms of jitter) into 120-frame pulls, after only (before has
the 40 ms preroll whatever the path):

| capture | −300 ppm | −20 ppm | +300 ppm |
| --- | --- | --- | --- |
| 5 ms periods | 11.3 ms | 12.2 ms | 11.7 ms |
| 10 ms periods (WASAPI loopback) | 13.8 ms | 14.8 ms | 14.3 ms |

No gaps and no cuts in any run.

## Bandwidth, and a cheaper mode under starvation

`lpcm-rice` is lossless, so its rate follows the content. The worst case
(white noise, every channel verbatim) from `max_coded_len`, with the 16-byte
audio header, about 80 bytes of IPv6/UDP/WireGuard and one parity packet
per four:

| layout | samples a packet | packets/s | worst payload | worst on the wire |
| --- | --- | --- | --- | --- |
| mono (microphone) | 240 | 200 | 0.78 Mbps | 1.16 Mbps |
| stereo | 240 | 200 | 1.54 Mbps | 2.12 Mbps |
| 5.1 | 80 | 600 | 4.66 Mbps | 6.40 Mbps |
| 7.1 | 60 | 800 | 6.22 Mbps | 8.54 Mbps |

Desktop and game sound codes to about 0.5–1 Mbps for stereo, silence to a
few bytes a frame. The encoder budget keeps 1.2 Mbps of the wire for audio,
and now 1.2 Mbps more for each pair beyond stereo.

On a starved path (a few Mbps in all) sound is then a large share. Options,
cheapest first; none is implemented here:

1. **Stereo instead of surround** when the path's budget falls below about
   8 Mbps: the host already mixes any layout down (`Resampler::with_layout`),
   so this is the host choosing `Layout::Exactly(2)` and a 240-sample stereo
   frame. It saves 2–4 Mbps at worst and 800/600 → 200 packets a second
   (the header overhead alone of 7.1 is about 0.8 Mbps). It needs a
   mid-session layout change, which v2 frames can carry (the channel count
   is in each frame) but the viewer's mixer does not yet follow.
2. **Fewer bits, still `lpcm-rice`**: zeroing the low 4 bits of each sample
   (12-bit sound, noise about −72 dBFS, below most rooms) would save about a
   quarter of the rate once the codec signals "wasted bits" as FLAC does
   (a reserved header bit; without it the Rice codes carry the zeros). A
   protocol addition, negotiated like `audio_channels`.
3. **A lossy codec**: Opus would be about 96–128 kbps stereo, but every
   Opus encoder is C, which the engine's boundary forbids; a pure-Rust MDCT
   codec is a project of its own.

Recommendation: 1, then 2, both driven by the congestion controller's
budget, before any lossy codec.
