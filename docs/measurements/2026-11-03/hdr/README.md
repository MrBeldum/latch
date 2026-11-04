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

| Encoder | encode p50 / p95, ms | bytes, 60 static frames |
| --- | ---: | ---: |
| AMF HDR10 | 1.036 / 1.125 | 2,577 |
| MF HDR10 fallback | 2.988 / 3.221 | 2,500,551 |

MF spends approximately the configured 20 Mbps even on these static stripes;
AMF does not pad the stream to the budget. These are static-pattern results.

FFprobe independently confirms **Main 10, yuv420p10le, tv range, bt2020nc,
bt2020, smpte2084**, mastering display metadata (1000-nit BT.2020, D65) and
MaxCLL / MaxFALL of 1000. FFmpeg-decoded luma at the stripe centres is
`[326,508,723]`, versus expected `[326.5364,509.0767,722.6005]`: maximum error
**1.077 code values** on both encoders. The test was extended to print SDR8,
SDR10 and HDR10 comparisons using the same GPU source.

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
  other overlays temporarily use wgpu presentation, so the native video
  layer cannot hide the toolbar, stats or dialogs.
- Mac fmt, CI clippy command, workspace/all-target tests and native audit pass.
  The timing-sensitive network impairment test failed twice with a temporary
  zero-fps report window, then passed on retry; archived main also passed its
  comparison run. No transport changes or assertion weakening were made.
- Windows compilation, clippy and workspace tests are run in the dedicated
  checkout with `-j 2`; see PR validation for final results.

Still unverified: live Mac ScreenCaptureKit HDR capture (this shell lacks
Screen Recording permission), physical-panel EDR/HDR10 brightness/colour and
native-window UI behaviour, Windows viewer hardware Main10 decode/presentation
under a sustained live stream, and 4K/high-fps gaming throughput. The Mac's
advertised peak is an EDR budget relative to a 203-nit diffuse-white convention,
not a photometer measurement. The measured native shader and live decoder paths
do not replace those display tests. HDR cursors use PQ-domain blending; exact
linear-light translucent cursor composition remains an improvement.

API references used for the implementation: [DXGI Output5 capture](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/nf-dxgi1_5-idxgioutput5-duplicateoutput1),
[DXGI HDR10 metadata units](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/ns-dxgi1_5-dxgi_hdr_metadata_hdr10),
[AMF HEVC properties](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/public/include/components/VideoEncoderHEVC.h),
[Apple EDR headroom](https://developer.apple.com/documentation/appkit/nsscreen/maximumextendeddynamicrangecolorcomponentvalue),
[extended linear Display P3](https://developer.apple.com/documentation/coregraphics/cgcolorspace/extendedlineardisplayp3).
