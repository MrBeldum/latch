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

## After the review (PR #59 review fixes)

The review found that every **debug** build panicked when the window opened:
`-[CAMetalLayer setColorspace:]` was sent an untyped pointer, and objc2
checks argument types in debug builds. The runs above used release bundles,
which skip that check. The colour space is now typed, and
`layer_messages_carry_the_types_their_methods_declare` sends each of the
window path's layer messages in a test (it fails with the old type). The
presenter also gained a settling time (300 ms), at most one display-driven
switch a second, and "cannot read the display" counts as no change. On the
Mac a screen is now its display ID. AppKit hands out a new `NSScreen` for
the same display each time the app is activated, so in the real app every
focus change had counted as a display change.

The Windows PC was offline, so these runs used a test `serve` on this Mac
(software H.264, so an **8-bit SDR** stream). The runs are the real
Latch.app built by `scripts/bundle-macos.sh` in `dist.noindex` (not
installed); the debug bundle is the same bundle with the debug binary:

```
LATCH_TEST_PATTERN=sweep target/release/examples/serve --bind 127.0.0.1 --port 47963
./drive.sh <Latch.app> <scratch data dir> <events log>   # in this folder
./summary.sh <scratch data dir>/panel.log
```

`drive.sh` starts the app with `LATCH_TEST_EXERCISE=3`, which steps through
stats, toolbar and full screen. From outside it makes six rounds of: Finder
in front, Latch in front, then the next Space and back (System Events
Ctrl-→ / Ctrl-←). After each action it notes whether the app's window was on
screen (`onscreen.js`, from the CoreGraphics window list, since no
screenshots are possible from this shell). The window left the screen on
each Space switch and came back.

| 8-bit SDR stream, ~60 s each | 75e4f19 debug | this branch, debug | this branch, release |
| --- | --- | --- | --- |
| Starts | **panics** in `Edr::new` (`setColorspace:`, `^v`) | yes | yes |
| Presenter | – | wgpu, chosen once | wgpu, chosen once |
| Presenter switches | – | **0** | **0** |
| Display changes acted on | – | 0 | 0 |
| Seconds with frames on both presenters | – | 0 of 59 | 0 of 58 |
| Frames EDR / wgpu | – | 0 / 6323 | 0 / 6369 |
| `presentation failed` / panics | – | 0 / 0 | 0 / 0 |
| Exercise steps (full screen) / focus changes | – | 21 (3) / 24 | 21 (3) / 24 |
| Space switches that took the window off screen and back | – | 6 + 6 | 6 + 6 |

Before the display-ID change, a debug run of this branch logged a
`video display changed` after every activation. The probe showed the same
window with a new `NSScreen` pointer each time, though no switch followed
for SDR. Logs: [review-fixes-real-app.log](review-fixes-real-app.log).

**Still not run live:** an HDR (PQ) stream in the real window. The test
`serve` only makes 8-bit H.264, and the PC that makes Main10 PQ is offline.
The EDR layer's messages are covered by the new test, and its picture by the
offscreen Metal tests.
