# HDR10 and ten-bit streaming, 2026-11-03

M3 Pro Mac viewer / encoder and Windows 11 RX 6600 host, in a separate
`C:\Users\Daniel\dev\latch-gfn-hdr` checkout. Only test port 47895 was used;
the installed host was not stopped or replaced. Builds are optimized dev/test
profiles, not release builds. Numbers below do not establish 4K120 capacity.

## Format comparison on the Mac

A moving 960×540 neutral ramp, HEVC at 60 fps / 20 Mbps, 125 frames per mode.
Five startup frames excluded from timings and bitrate; no pacing sleep. Bitrate
is the encoded size of the remaining 120 frames at the configured 60 fps,
excluding the initial IDR, FEC and transport. Same sample-code pattern for each
transfer, rather than a perceptually matched HDR/SDR scene. Main10 uses the
existing encoder speed path; SDR8 uses its existing quality path at this size.

| Measurement | Before: SDR8 | SDR Main10 | HDR10 |
| --- | ---: | ---: | ---: |
| encode p50 / p95, ms | 3.643 / 4.441 | 2.763 / 3.499 | 2.494 / 3.347 |
| decode p50 / p95, ms | 0.626 / 2.207 | 0.764 / 1.693 | 0.551 / 2.680 |
| encoded Mbps | 0.121 | 0.251 | 0.251 |
| decoded ramp levels | 219 | 875 | 875 |

This is about four times the gradient precision. Bitrate doubles for this
very compressible ramp; this is not a general bitrate multiplier. Encoding
added no latency in this test. Ten-bit SDR decode added 0.138 ms at the median;
HDR decode p95 added 0.473 ms. A claim of *zero* added latency is not supported.

```
cargo test --locked -p latch-engine --lib color_latency_benchmark -- --ignored --nocapture
```

[Raw measurements](mac-color-benchmark.txt).

## RX 6600 HDR conversion and encoding

The interactive GPU probe uploads FP16 scRGB stripes at 10, 100 and 1000 cd/m²,
converts to BT.2020 PQ RGB10 in the capture shader, then P010 in the D3D11 video
processor. Before encoding it checks the actual GPU texture against ST 2084.
Both AMF and the MF fallback encode sixty frames from that texture. No desktop
colour setting is needed for this probe.

| Format / encoder | encode p50 / p95, ms | bytes, 60 static frames |
| --- | ---: | ---: |
| SDR8 / AMF | 1.056 / 1.443 | 2,535 |
| SDR10 / AMF | 1.032 / 1.630 | 2,535 |
| HDR10 / AMF | 1.037 / 1.090 | 2,577 |
| SDR8 / MF | 2.986 / 3.542 | 2,500,511 |
| SDR10 / MF | 2.991 / 3.517 | 2,500,511 |
| HDR10 / MF | 2.987 / 3.501 | 2,500,551 |

MF spends approximately the configured 20 Mbps even on these static stripes;
AMF does not pad the stream to the budget. These are static-pattern results.

FFprobe independently confirms **Main 10, yuv420p10le, tv range, bt2020nc,
bt2020, smpte2084**, mastering display metadata (1000-nit BT.2020, D65) and
MaxCLL / MaxFALL of 1000. FFmpeg-decoded luma at the stripe centres is
`[326,508,723]`, versus expected `[326.5364,509.0767,722.6005]`: maximum error
**1.077 code values** on both encoders. All three format comparisons above use the same FP16 GPU source. SDR
highlights are clipped to SDR white; this is an encode latency comparison,
not a perceptually matched bitrate benchmark. AMF median encode does not
increase; SDR10 AMF p95 increases by 0.187 ms.

An AMF first access unit was also decoded by Latch's P010 VideoToolbox path,
uploaded through the Metal texture cache, rendered with the wgpu HDR-to-SDR
shader, and compared numerically against the *FP16 source's* luminance. It
passes within seven output-byte values, including compression and quantization.

```
# Interactive Windows user session, not SSH session 0:
cargo test --locked -p latch-engine --lib scrgb_pq_p010_hardware_roundtrip -- --ignored --nocapture
# Set LATCH_HDR_PROBE_DIR to save hdr-amf.hevc and hdr-mf.hevc.
ffprobe -v error -show_streams -show_frames hdr-amf.hevc
ffmpeg -v error -i hdr-amf.hevc -frames:v 1 -c copy hdr-amf-first.hevc
# Mac:
LATCH_HDR_RENDER_STREAM=hdr-amf-first.hevc LATCH_HDR_RENDER_STRIPES=1 \
  cargo test --locked -p latch-client --lib hdr_encode_decode_render_matches_source_numerically -- --ignored --nocapture
```

[GPU probe](windows-hdr-probe.txt), [AMF VUI](windows-amf-vui.json),
[MF VUI](windows-mf-vui.json), [AMF fidelity](hdr-amf-fidelity.json),
[MF fidelity](hdr-mf-fidelity.json), [AMF → Mac renderer](windows-amf-mac-render.txt).

## Live PC desktop → Mac viewer

1280×720 requested at 60 fps / 20 Mbps. The actual VDD desktop was mostly idle;
100 ms idle repeats explain the low average fps. The Tailscale RTT was roughly
150 ms. Desktop activity and congestion adaptation varied between runs. The
following are arithmetic means of ten one-second samples, not an encoder-only
benchmark or a controlled bitrate comparison.

| Desktop → stream | host capture-to-output ms | decode ms | video Mbps | fps |
| --- | ---: | ---: | ---: | ---: |
| SDR → SDR8 | 3.170 | 2.554 | 0.272 | 11.6 |
| SDR → SDR10 | 3.181 | 2.793 | 0.465 | 14.4 |
| HDR → HDR10 | 3.306 | 2.993 | 0.210 | 12.0 |
| HDR → legacy SDR8 | 3.048 | 2.918 | 0.704 | 16.7 |

The requests/Accept logs confirm SDR10 and HDR10 negotiation, including the
9/16/9 CICP description and metadata. All samples report no video problem,
including the HDR desktop viewed as legacy SDR8: no sustained black capture.
The live path adds 0.011 / 0.136 ms host time and 0.239 / 0.439 ms decode time
for SDR10 / HDR10 versus SDR8 in these runs. These differences include scheduler
noise and changed desktop activity; they do not prove zero added latency.

The VDD supports HDR. A scoped ignored test switched it from mode 0 (SDR) /
flags 81 to mode 2 (HDR) / flags 115 for each live test. Its Drop guard restored
both HDR and WCG switches and checked flags 81 / mode 0 afterwards. The complete
control API colour report [before](windows-display-before.json) and
[after](windows-display-after.json) is identical. Test tasks, server and temporary
firewall allowances were removed; the original application-block rules were
re-enabled. No installed host was deployed to or stopped.

```
# Interactive PC task; bind is restricted by temporary Tailscale firewall rules:
serve --bind 0.0.0.0 --port 47895 --telemetry
# Mac, add --ten-bit for SDR10; add both --ten-bit --hdr for HDR10:
view 100.106.23.40 --port 47895 --width 1280 --height 720 --fps 60 \
  --kbps 20000 --hevc --seconds 10
# Interactive PC, with the test host already running; automatically restores
# both original switches after at most 120 seconds or hdr-toggle-stop:
cargo test --locked -p latch-engine --lib hold_hdr_display_for_live_test -- --ignored --nocapture
```

[Live summary](live-summary.json), [SDR8](hdr-live-sdr8.txt),
[SDR10](hdr-live-sdr10.txt), [HDR10](hdr-live-hdr10.txt),
[HDR → SDR8](hdr-live-hdr-to-sdr8.txt),
[HDR state restoration](windows-hdr-state-test.txt),
[legacy SDR state restoration](windows-hdr-to-sdr-state-test.txt).

## Checks and limits

- PQ EOTF/OETF known value and roundtrip, BT.2020↔709 matrices, limited-range
  endpoints, default-off negotiation and HDR SEI escaping are unit tested.
- Mac synthetic Main10 roundtrip: **64.52 dB luma PSNR**, verified by FFmpeg
  and Latch's P010 decoder. HDR VUI is checked independently.
- Native Metal shader test: 1000-nit PQ produces **3.85 EDR units** with a
  four-unit headroom shoulder in an RGBA16Float render target. The native
  HDR10 HLSL shaders compile through Windows' system compiler.
- Existing SDR CPU and zero-copy GPU renderer tests still pass. Controls and
  overlays are composited over native video (see *Review fixes* below).
- Mac fmt, CI clippy command, workspace/all-target tests and native audit pass
  (675 passed in the final serial run).
  The timing-sensitive network impairment test failed twice with a temporary
  zero-fps report window, then passed on retry; archived main also passed its
  comparison run. A later parallel loopback run stalled in its existing
  key-to-picture loop and was stopped; all three loopback tests passed in
  1.84 s on serial retry, and the complete serial workspace run passed.
  No transport changes or assertion weakening were made.
- Windows compilation, CI clippy and workspace/all-target tests pass with
  `-j 2` in the dedicated checkout.
- The explicit Windows Main10 decode probe cannot run on this PC: the
  Microsoft HEVC decoder COM class is not registered (`0x80040154`). The
  viewer consequently does not advertise HEVC here, preserving Baseline
  fallback. No codec-store installation or paid extension was installed.
  [Decoder probe output](windows-main10-decoder.txt).

Still unverified: live Mac ScreenCaptureKit HDR capture (this shell lacks
Screen Recording permission), physical-panel EDR/HDR10 brightness/colour and
native-window UI behaviour, Windows viewer hardware Main10 decode/presentation
with the HEVC decoder extension installed, and 4K/high-fps gaming throughput. The Mac's
advertised peak is an EDR budget relative to a 203-nit diffuse-white convention,
not a photometer measurement. The measured native shader and live decoder paths
do not replace those display tests. HDR cursors use PQ-domain blending; exact
linear-light translucent cursor composition remains an improvement.

API references used for the implementation: [DXGI Output5 capture](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/nf-dxgi1_5-idxgioutput5-duplicateoutput1),
[DXGI HDR10 metadata units](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/ns-dxgi1_5-dxgi_hdr_metadata_hdr10),
[AMF HEVC properties](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/public/include/components/VideoEncoderHEVC.h),
[Apple EDR headroom](https://developer.apple.com/documentation/appkit/nsscreen/maximumextendeddynamicrangecolorcomponentvalue),
[extended linear Display P3](https://developer.apple.com/documentation/coregraphics/cgcolorspace/extendedlineardisplayp3).

## Review fixes: overlays, HDR desktop → SDR, chroma siting

Follow-up to the independent review of PR #55.

**Controls over HDR video.** Before, any toolbar, stats, settings, toast or
the initial hint switched native presentation off and the wgpu shader
tone-mapped the whole picture to SDR: a 1000-nit patch went from 3.85 EDR
units to about 1.0. Now the picture stays HDR whatever is shown.

| Check | Before | After |
| --- | --- | --- |
| Mac: 1000 / 203-nit patch in the EDR layer, stats hidden → shown | 3.85 → ≈1.0 (SDR tone map) | 3.85 → 3.85, 1.00 → 1.00 (bit-identical) |
| Mac: egui alpha over the picture outside / under the overlay | 1 / 1 (opaque SDR copy) | 0 / ≈200⁄255 (overlay composited over HDR) |
| Windows HDR desktop: 203/400/600/1000-nit patches, overlay hole off → on → off | native window hidden, SDR tone map | scRGB 0.908 / 1.797 / 2.678 / 4.473 each time, bit-identical; hole shows the parent |

The Windows numbers are DXGI duplication of a real Advanced Color desktop
(the PC's virtual display, peak 1670 nits, SDR white 240 nits) with the
HDR10 child window presenting a P010 frame. Patch ratios are 1 : 1.979 :
2.949 : 4.926 against 1 : 1.970 : 2.956 : 4.926 expected; duplication on this
virtual display reports every pixel, SDR content included, at 0.358 × nits/80,
so absolute values carry that gain. The run also found that the HDR10
presenter had never drawn: its full-screen triangle was counter-clockwise
and D3D11's default rasterizer culled it (the viewer cannot select Main10 on
this PC, which lacks the HEVC decoder extension, so no live session had used
it). [Live transcript](windows-hdr10-overlay-live.txt),
[what the compositor showed](windows-hdr10-overlay-window.png),
[Mac tests](mac-review-fixes.txt).

```
# Mac (Metal):
cargo test -p latch-client --lib video:: -- --ignored --skip hdr_encode_decode --skip bench_
cargo test -p latch-client --lib controls_and_stats_are_reported_as_overlays_over_the_video
# Interactive PC session, three processes: HDR held and restored by the first.
latch_engine-*.exe --ignored hold_hdr_display_for_live_test
latch_client-*.exe --ignored hdr10_child_window_keeps_hdr_under_overlays   # LATCH_HDR_PATCH_DIR=<dir>
latch_engine-*.exe --ignored hdr_desktop_patches_reach_sdr_capture_apart   # same LATCH_HDR_PATCH_DIR
```

**HDR desktop → SDR stream.** The capture shader clipped each channel at SDR
white. It now desaturates out-of-BT.709 colours toward their own luminance
and compresses highlights above 75% of SDR white up to the 1000-nit ceiling
on max(R,G,B); both scale about neutral, so hue is kept exactly. SDR content
below the knee is unchanged. WARP shader output at SDR white 203 nits,
matching `latch_proto::color::hdr_to_sdr` within one code:

| scRGB patch (nits) | Before (sRGB8) | After (sRGB8) |
| --- | --- | --- |
| neutral 203 / 400 / 600 / 1000 | 255 / 255 / 255 / 255 | 240 / 251 / 253 / 255 |
| neutral 100 | 186 | 186 |
| orange (1000, 500, 0) | (255, 255, 0) — turns yellow | (255, 188, 0) |
| blue (40, 80, 600) | (123, 168, 255) | (72, 101, 253) |
| BT.2020 green, 1000 | (0, 255, 0) | (0, 255, 145) |
| black | (0, 0, 0) | (0, 0, 0) |

On the live Advanced Color desktop (SDR white 240 nits, the patches at the
duplication gain above) the SDR capture path gave **149 / 203 / 236 / 248**
for the four neutral patches: ordered, never black, not clipped together.
The SDR-desktop path is unchanged.

```
cargo test -p latch-proto --lib hdr_highlights_stay_apart_in_sdr_and_keep_their_hue
# Windows (WARP, works over SSH):
cargo test -p latch-engine --lib fp16_sc_rgb_hdr_desktop_keeps_highlights_in_sdr -- --nocapture
```

**Chroma siting.** `ColorDescription` now carries `chroma` (`left` by
default, H.273 type 0; optional on the wire). The Mac EDR and Windows HDR10
shaders sample chroma a quarter chroma texel right of the centred position
for left siting. A coloured edge in left-sited chroma (chroma sample k on
luma column 2k) now renders exactly the new colour on column 2k and the
midpoint on 2k−1, in both SDR and PQ modes, on Metal and WARP; centred
sampling fails the same check. The legacy eight-bit shader is unchanged.
VideoToolbox, AMF and MF Main10 streams all probe as `chroma_location=left`,
matching the Windows video processor's `*_LEFT_*` output.
[Probe](chroma-location.txt).

Still unverified after these fixes: a physical EDR/HDR10 panel (CoreAnimation
composition was checked through egui's alpha and the EDR layer's values, not
by photographing a screen), the actual ScreenCaptureKit chroma siting of a
live HDR capture, and the Windows viewer's full Main10 session (no HEVC
decoder extension on this PC).
