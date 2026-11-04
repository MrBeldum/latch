# Mac viewer flicker on ten-bit streams, 2026-11-04

Daniel streamed the Windows PC (SDR desktop, so a ten-bit SDR HEVC stream)
to Latch.app built from main 3d5d69e. The picture flickered badly, and
`panel.log` showed `WARN EDR presentation failed: no HDR window` many times a
second. The EDR layer found its window through `NSApplication.mainWindow`,
which is nil whenever another app or window is key or a space is changing.
Each such frame went to egui's wgpu surface, and the next to the EDR layer
again, so two surfaces took turns.

## Real app after the fix

The Latch.app built by `scripts/bundle-macos.sh` at 6feee68+ (dist.noindex,
not installed) ran against a test `serve` on the PC, on port 47898 and bound
to its Tailscale address. It used a scratch data directory and started
windowed:

```
LATCH_DATA_DIR=<scratch> LATCH_TEST_SERVE=100.106.23.40:47898 \
  LATCH_TEST_EXERCISE=3 RUST_LOG=info,latch_client::video=debug,wgpu_hal=error \
  dist.noindex/Latch.app/Contents/MacOS/Latch
# meanwhile, six times: open -a Finder; sleep 2.5; open -a dist.noindex/Latch.app; sleep 3.5
```

The exerciser stepped through stats, toolbar+stats, plain and full screen,
then the same in full screen, then back to a window, every 3 s. The focus
round trips happened in both windowed and full-screen phases, and in full
screen they also moved between the app's space and the desktop.

| 10-bit SDR stream, 74 s | main 3d5d69e (Daniel's report) | this branch |
| --- | --- | --- |
| Presenter for the stream | EDR or wgpu, per frame | wgpu (chosen once: `10-bit SDR`) |
| Seconds with frames on both presenters | many, several warnings a second | 0 of 74 |
| Frames via EDR / via wgpu | mixed | 0 / 1271 |
| Presenter switches | many a second | 0 |
| `presentation failed` warnings | many a second | 0 |
| Exercise steps / focus changes logged | n/a | 41 / 13 |

These counts come from the app's own log, in [the log excerpt](sdr10-real-app.log).
This shell cannot take screenshots, so nothing here looks at the screen. The
"before" column is Daniel's report and log. A before build with the same test
hook was ready but could not be run (see below).

**Not run:** the HDR10 stream in the real app. The PC dropped off the tailnet
(rustc access violations while compiling, then no answer; its known
memory-corruption bugchecks) before the HDR desktop toggle had been started.
The EDR path is covered only by the offscreen Metal tests
(`hdr_patches_keep_their_luminance_while_an_overlay_shows`,
`native_edr_shader_*`) and the presenter unit tests.
