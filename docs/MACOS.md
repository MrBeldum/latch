# macOS client (Apple Silicon)

Latch for the Mac is one native app: every machine on your Tailscale
account, a Connect button each, a power menu, the stream itself, and a
**Sharing** tab so a Windows PC or another Mac can open this desktop. Latch's stream protocol is compiled in, video is captured with ScreenCaptureKit
and encoded/decoded with VideoToolbox (H.264 and HEVC), audio is coded losslessly
with `lpcm-rice`, and the picture is drawn in Latch's own window under a toolbar.

## Requirements

- macOS 13+ on Apple Silicon
- [Tailscale](https://tailscale.com/download/mac), signed in with the same
  account as your other machines
- Screen Recording and Accessibility permissions, which macOS asks
  for when setting up sharing (so others can see this Mac's desktop and send
  input)

## Install a prebuilt app

Public releases download with `curl`. If the repository is private, install
the [GitHub CLI](https://cli.github.com) and sign in
(`brew install gh && gh auth login`) so the script can fall back to `gh`.

```bash
curl -fsSL https://raw.githubusercontent.com/MrBeldum/latch/main/scripts/install-macos-release.sh | bash
```

It checks the release SHA-256, verifies the ad-hoc signature, swaps the app
into `/Applications` (the live copy is not deleted first) and opens it.

By hand: download `latch-macos-arm64.tar.gz` from the
[latest release](https://github.com/MrBeldum/latch/releases/latest), then

```bash
tar xzf latch-macos-arm64.tar.gz
xattr -dr com.apple.quarantine Latch.app
open Latch.app
```

The `xattr` step is required for a browser download. The app is ad-hoc
signed rather than signed with an Apple Developer ID, and macOS 15 reports
such apps as damaged instead of offering "Open anyway". Removing the
quarantine flag is how you tell macOS you fetched it deliberately.

## Build on a Mac

```bash
./scripts/install-macos.sh
```

builds for `aarch64-apple-darwin` (pure Rust, no CMake or C compilers needed),
bundles `dist.noindex/Latch.app`, and installs it to `/Applications`. The `.noindex` folder keeps build copies out of app search.
A locally built app never carries the quarantine flag.

### Signing

`bundle-macos.sh` signs ad-hoc by default. For a build other people can
download without the quarantine step, sign with a Developer ID and
notarize:

```bash
CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" ./scripts/bundle-macos.sh
ditto -c -k --keepParent dist.noindex/Latch.app dist.noindex/Latch.zip
xcrun notarytool submit dist.noindex/Latch.zip --keychain-profile "notary" --wait
xcrun stapler staple dist.noindex/Latch.app
```

## First run

1. If Tailscale is missing or signed out, a banner at the top of
   **Machines** says so; **Get Tailscale** opens the download page.
2. **Machines** lists every machine on your Tailscale account, this Mac
   first. A dot and one line say whether each is ready, asleep, or
   missing something on its side.
3. Click **Connect** (**Wake and connect** for a machine that is asleep).
   There is no pairing: being on the same Tailscale account is the check.
   If Latch is not sharing on that machine, open Latch there and set up
   sharing on its **Sharing** tab first.
4. The desktop appears.

Command-1 opens **Machines**, Command-2 **Sharing** and Command-comma
**Settings**; Escape goes back to **Machines**.

## While streaming

The picture fills the window and nothing else is drawn over it. With
**Match screen** the picture is this display's own size, so there is no
bar on any side; a window is resized to the stream's proportions when the
session starts, for the same reason. A manual 16:9 size on a display of
another shape leaves a bar above and below.

**Control-Option** (Ctrl+Alt on Windows and Linux) is the host key, as in
VMware or VirtualBox. Press it and the mouse is freed and the toolbar drops
over the top of the picture; press it again, or click the picture, and the
toolbar goes away and the mouse is captured back. The keys are shown when
a stream starts, and on the toolbar itself. Before the first picture
arrives, a **Cancel** button under the progress ends the attempt without
the host key. A menu or a question keeps the toolbar there until it is
answered.

**Stats** (or the host key with **S**, or **F6** as in GeForce NOW:
Control-Option-S) lays a stats overlay over the picture's top-right
corner without taking the mouse or showing the toolbar: resolution, codec
and decoder; frames a second received, decoded and drawn against what was
asked for; bitrate now against the host's current target and the ceiling;
round trip; frame jitter; packet loss as it happened and after FEC; frames
lost, skipped before decoding and decoded but never drawn; decode time;
how long a decoded picture waits to be drawn; and an estimated latency
with its breakdown (host capture to send, half the round trip plus frame
assembly, decoder queue plus decode, waiting to be drawn plus half a
refresh of this display). Values worth a look turn amber.

The mouse is captured by default: the Mac cursor is hidden and held in
place and raw movement is sent, which is what games read for the camera
(a game that ignores the cursor's position, such as Genshin Impact, turns
only with this). The PC draws its own cursor in the picture, so only one
pointer is ever visible. The **Mouse** menu can switch to **Free**, where
the Mac cursor's position is sent 1:1 instead and nothing is captured.

| Toolbar item | What it does |
|--------------|--------------|
| **● Machine name · 1920×1080 · 60 fps · HEVC** | The stream as negotiated (the numbers show on wide windows); the dot turns amber while the connection is poor |
| **Direct · 38 ms / Relayed via Tokyo · 210 ms / Via your relay · 60 ms** | The path Tailscale found and its round trip. Amber means every packet goes through one of Tailscale's shared relays, which adds delay; **Connection details** under the machine list say why and what would give a direct path. What Latch asks for is the same on every path |
| **Mouse** | **Captured, for games** (the default): a click on the picture hides the cursor and sends raw movement, which games read; the host key frees it. **Free, for desktops**: the Mac cursor's position is sent 1:1 and nothing is captured. The choice is remembered, and is also in Settings |
| **Keys** | Ctrl+Alt+Delete, Windows key, Alt+Tab, Escape, Print Screen; and **Command acts as Ctrl** |
| **Stats** | The stats overlay (above). **More → Stream performance** opens the full panel: frame rate and bitrate against what was asked for, round trip, packet loss, encode, assembly, queue and decode times, and the decoder. **Copy diagnostics** puts it all on the clipboard |
| **Full screen** | Toggle; the setting decides how a session starts |
| **PC** (or **Mac**, **Machine**) | Sleep…, Restart…, Shut down…, each asking first, when the machine's owner allows them; Sleep only on a Windows PC, the one kind a wake packet can bring back. **Update Latch Host…** for a PC whose host is older than 3.1 |
| **Stream settings** | The same controls as Settings: quality profile, resolution, frame rate, bitrate and codec. **Apply and reconnect** restarts the session with them in a second or two; Escape or the close button leaves them as they were |
| **Disconnect** | End the session |
| **2 gamepads** | Shown while gamepads are connected to this Mac; hover for which, and whether the host takes them (green), or why not (amber) |

On a narrow window everything but **Stream settings** and **Disconnect**
folds into one **More** menu.

Everything else on the keyboard goes to the PC as pressed, in the order it
was pressed. With **Command acts as Ctrl** on (the default), ⌘C, ⌘V, ⌘Z and
the rest do on Windows and Linux what they do on the Mac, and ⌘ can stay
held across several of them; a Mac at the other end always gets ⌘ as
Command. While the stream has the keys, ⌘Q, ⌘H and ⌘+/⌘−/⌘0 go to the PC
too (⌘Q closes the PC's window, not Latch): quit or hide Latch from the menu
bar or the Dock, after **Disconnect**, or while the toolbar or a panel has
the focus. ⌘Tab and ⌘Space stay on the Mac.

### Gamepads

Gamepads connected to the Mac (Xbox Wireless, DualSense, DualShock 4,
Switch Pro and other MFi pads, over USB or Bluetooth) go to the host as
they are: a Windows PC gets an Xbox 360 pad for an Xbox pad and a
DualShock 4 for a PlayStation one, a Linux host an Xbox 360 pad for each.
The game's rumble comes back to the pad, on each handle and, on pads that
have them, the triggers. macOS gives gamepads to the frontmost app only,
so keep the stream's window in front. A Mac host takes no gamepads.

### Clipboard

With Latch Host 3.1 or newer on the PC, the clipboard follows you both
ways: text copied on the PC is in the Mac's clipboard a second later, and
⌘V on the PC pastes the text the Mac has (Latch sends it to the PC first,
then presses Ctrl+V there). Text only, up to 32 KB; an image or a file on
either clipboard is left alone, so ⌘V with a screenshot on the Mac's
clipboard, or after copying a file on the PC, pastes what the PC itself last
copied. Keys typed right after ⌘V wait for the paste to land (up to two
seconds). Short notices under the toolbar say when something crossed. With
an older host, ⌘V pastes what the PC last copied.

A Linux machine gets ⌘C, ⌘X and ⌘V as Ctrl+Insert, Shift+Delete and
Shift+Insert, which GTK and Qt apps, Firefox, Chromium and terminals all
take for copy, cut and paste; Ctrl+C in a terminal would interrupt the
running program. Latch Host there puts pasted text in the selection as
well as the clipboard, because terminals paste the selection on
Shift+Insert. To copy in a terminal, select the text and press ⌘C.

A Linux VPS draws a desktop of one fixed size and encodes on the CPU, so
Latch never asks it for more than that size: a larger resolution setting
gets the desktop's own size, a smaller one (720p, say) is scaled down.
Latch Host there times its software encoder on its own CPU at that size
and reports the frame rate it keeps up with; Latch asks for no more than
that, and more for a smaller picture in proportion to the pixels it saves
(a host measured at 30 fps at 1080p is asked for up to 67 at 720p, 120 at
540p). To get 60 fps from a slow machine, pick 720p (or 540p) and 60 fps.

## Only connecting from this Mac

On a Mac, **Share this Mac** on the **Sharing** page decides whether Latch
runs in the background so other machines can connect here. Turn it off and
this Mac only connects to the others: the background service and the
streaming engine stop, opening Latch no longer starts them, and nothing of
Latch runs once its window closes. **Share this Mac** turns it back on.

### Laggy? Look at the path

Tailscale connects the Mac and the PC directly when it can punch through
both routers. When it cannot, every packet goes through a relay: your own
peer relay if you run one (**Relay** in Settings), otherwise one of
Tailscale's (DERP). A relay adds a detour, and the round trip it adds is a
floor under how quickly the PC answers a click; it changes nothing about
what Latch asks for. Balanced quality is this screen's size at its own
refresh rate on every path, so a relayed stream is laggier, not blurrier;
the host's rate control lowers the bitrate only when the network does not
carry it.
Tailscale's own relays are shared, so a stream through one can also
stutter at busy times; a peer relay you run is yours alone.

Latch shows which case you are in next to each PC (**Relayed via Tokyo ·
210 ms**, **Via your relay · 60 ms**) and, when relayed, **Connection
details** under the list says which router is in the way and what would
fix it: UPnP or NAT-PMP turned on in the PC's router, a UDP port forwarded
to the PC, or IPv6 on both networks. Both machines report their own side
(`tailscale netcheck`); the PC's report needs Latch Host 3.1.

## A machine's menu

The **…** button at the end of each row offers what that machine allows:
**Wake** (asleep, and Latch has learned how to wake it; sends the packet
without connecting), **Test waking it from this network** (awake), **Sleep**,
**Restart…** and **Shut down…** (when that machine allows remote power
actions), and **Copy Tailscale address**. Restart and shut down ask first;
both force-close programs there.

**Test waking it from this network** sends the wake packet to the awake
machine and asks Latch there whether it arrived. A pass means waking it
from where you are will work; a failure means the packets are not reaching
it from this network (see below).

## Settings

| Setting | What it changes |
|---------|-----------------|
| Quality | Profiles that follow this display, as GeForce NOW's do. **Balanced** (the default) asks for this screen's own size at its own refresh rate (120 Hz on a ProMotion MacBook Pro, 144–360 on a fast monitor). **Competitive** is 1080p at the display's refresh rate, up to 360 fps. **Cinematic** is this screen's size up to 4K, up to 120 fps, at half again the bitrate. Each profile's bitrate is a ceiling: 40 Mbps at 1080p60, growing as pixels to the ¾ and frame rate to the ½ (50 Mbps at 1080p60, 87 at 1440p120, 113 at 4K60, 160 at 4K120), half again for Cinematic, never under 5.1's 50 Mbps (Cinematic 100); the host comes down from it whenever the network carries less. **Custom** uses the rows below; changing one of them switches to it, starting from what the profile asked for. The PC's encoder sets the only other limits: a larger picture than it takes is scaled down in proportion. Settings saved by 5.1 move Smooth, Balanced and Sharp to Competitive, Balanced and Cinematic once |
| Resolution | **Match screen** is this display's own pixel size (3024×1964 on a 14" MacBook Pro), so the picture fills it exactly. **540p**, **720p**, **900p**, **1080p**, **1440p**, **4K**, **5K** and **8K** are the standard 16:9 sizes, 960×540, 1280×720, 1600×900, 1920×1080, 2560×1440, 3840×2160, 5120×2880 and 7680×4320; on a display of another shape they leave a bar above and below. The sizes below 1080p are for a PC that encodes in software or a slow network: fewer pixels leave more bitrate for each and a higher frame rate. The list shows the pixel size of each. The PC switches its display to the size asked for; a PC with no monitor needs a virtual display that lists that size, which setup on the PC arranges for the Virtual Display Driver |
| Frame rate | 30 to 500, and this display's own rate (marked); the PC's display is switched to match when it can |
| Bitrate | 2 to 1000 Mbps in steps (fine at the low end); arrow keys step once, Shift-arrow ten times. The most the encoder spends; the host's rate control comes down from it when the path drops packets |
| Codec | Auto (the best both ends have: AV1, then HEVC, then H.264), AV1, HEVC, H.264. A codec either end lacks falls back to H.264 |
| Send my microphone | Off by default. On, this Mac's microphone plays on the PC as a microphone: by itself on Linux; on Windows and macOS through a virtual cable installed there (VB-CABLE, BlackHole). macOS asks for the microphone the first time |
| Open streams full screen | How a session starts |
| Capture the mouse | The toolbar's Mouse choice, remembered |
| Command acts as Ctrl | Off makes ⌘ the Windows key on Windows and Linux machines; a Mac always gets ⌘ as Command |
| App to open | The app on the PC to launch; "Desktop" is the whole screen. The list fills in after the first connection |
| Offer to sleep a machine after a session | Off by default. Asleep, Tailscale is off there; this Mac can only wake it from its own network |
| Keep Latch up to date | Updates this app and the Windows PCs' Latch Host; **Check now** checks at once |
| Relay | Whether a relay of your own is in use, and the grant it needs |
| About | Version, licence, source, and the open-source notices |

The stream rows are the panel that **Stream settings** opens during a
session, and **Next stream** under them says exactly what the next connect
asks for; the machine list shows the same. A session also reconnects by
itself, at the new size, when this Mac's window moves to a display of a
different size.

Settings are saved in `~/Library/Application Support/io.github.MrBeldum.Latch/client.toml`,
along with the MAC and LAN address of each PC Latch has seen.

## Waking a PC from another network

Asleep, the PC's Tailscale is off, so Latch cannot reach it over the
tailnet. Leave the PC on (Latch keeps it awake while plugged in)
if you want **Connect** from anywhere.

The wake packet is sent to the LAN broadcast, to the PC's LAN address and
to the PC's public address. Over the internet only the last can arrive,
and only if the PC's router forwards UDP 9 to the PC. A [Tailscale subnet
router](https://tailscale.com/kb/1019/subnets) on the PC's network (a NAS,
a Raspberry Pi, or a router that runs Tailscale) is the alternative. Use
**Test wake** while the PC is on to find out which situation you are in.

## Sharing this Mac: the video pipeline

ScreenCaptureKit renders the display straight into an IOSurface at the
stream's size (it scales, with bars where the shapes differ), and
VideoToolbox encodes that surface; only compressed bytes reach Latch.

- **Which display.** The main one, or the one whose CGDirectDisplayID is
  in `LATCH_MAC_DISPLAY` in the host's environment. The stream request
  does not name a display yet.
- **How often.** Every refresh of a display no faster than the stream
  (a ProMotion panel reports no fixed rate and gets every refresh too);
  for a slower stream, every k-th refresh with half a refresh of slack,
  since a gap of exactly 1/fps lets timing jitter skip every other one.
  The frame grid then takes the stream's rate from what arrives.
- **Surfaces.** Five, so the two frames in the encoder and the newest
  picture never leave the window server without one to draw into.
- **Encoder settings.** Hardware required (software only if it fails),
  real time, no frame reordering, `MaxFrameDelayCount` 0, the stream's
  rate as `ExpectedFrameRate` and (macOS 15+)
  `MaximumRealTimeFrameRate`, no periodic keyframes, BT.709 tags.
  Bitrate is whatever the server asks for; VideoToolbox took 1 Gbps.
  For 8-bit hardware HEVC through 248,832,000 pixels/second (1080p120),
  `PrioritizeEncodingSpeedOverQuality` is false; above that budget, and
  for H.264, Main10 and software, it is true. This M3 Pro crossover
  restores HEVC quality while retaining the fast path at 240 fps and
  3024×1964@120. Other chips may have a different crossover.
- **No low-latency rate control.** `EnableLowLatencyRateControl` sounds
  right but is built for calls: on an M3 Pro it made a 1080p frame take
  9.4 ms instead of 4.7, dropped frames (45 of 60 fps at 1080p60), and
  sent a fifth of the bitrate asked for.
- **Two frames in flight.** A frame goes to the encoder without waiting
  for the one before, and leaves the moment it is done, so the media
  engine overlaps them. A stream whose interval is shorter than one
  frame's encode time (240 fps at 1080p, 120 at 3024×1964) keeps its
  rate; a slower one sees no difference.
- **10-bit.** HEVC Main10 from P010 (`x420`) surfaces works and is tested
  with synthetic surfaces. Viewers now negotiate ten-bit SDR or HDR10;
  ScreenCaptureKit selects P010 and canonical HDR capture with a PQ colour
  space for HDR, and VideoToolbox writes matching BT.2020/PQ tags. HDR
  capture requires Apple Silicon and macOS 15; unavailable Main10 capture
  or encoding retries SDR8 before Accept.

A requested keyframe is the next picture returned to the server. Queued
pre-request frames are discarded until the fresh IDR, so they cannot delay
loss recovery or make the server request another IDR. Retries share that
request until the IDR leaves. A bitrate change first completes the
pictures already in the encoder, so every later picture is encoded at the
new rate (on an M3 Pro the first one after a 20 → 1 Mbps cut came out at
half the size). The completed old-rate pictures still leave, in order:
later P-frames reference them, and dropping them would cost an IDR at
every step of a ramp.

### Measured limits (M3 Pro)

`cargo test --release -p latch-engine --lib realtime_benchmark -- --ignored --nocapture`
submits a moving synthetic picture from prefilled IOSurfaces on the
stream's grid, as capture does, and times each frame from submission to
the encoder's output. `LATCH_VT_BENCH=hevc:1920x1080@240:80000,...`
picks the cases, `LATCH_VT_IN_FLIGHT=1` measures one frame at a time.
Two runs on 2026-11-03, the Mac busy with other builds:

| Case | 5.1 p50 / p95 ms | 5.1 fps | Now p50 / p95 ms | Now fps |
| --- | --- | --- | --- | --- |
| HEVC 1080p60, 50 Mbps | 9.8–10.5 / 13–44 | 51–61 | 4.9–5.3 / 8.1–8.3 | 60.7 |
| HEVC 1080p120, 50 Mbps | 9.3–9.7 / 12–14 | 99–105 | 4.7–4.8 / 8.0–8.3 | 121 |
| HEVC 1080p240, 80 Mbps | 8.7–9.8 / 9–20 | 86–114 | 4.2–4.6 / 6.2–6.6 | 241–242 |
| HEVC 3024×1964@120, 100 Mbps | 22.9–23.6 / 24–90 | 31–43 | 11.2–11.8 / 17 | 120 |
| H.264 High 1080p240, 80 Mbps | 8.4–9.6 / 10–92 | 40–116 | 10.0 / 11.2 | 192 |
| H.264 High 3024×1964@120 | 27–33 / 60–65 | 28–32 | 27–28 / 29–32 | 70–72 |
| Baseline 1080p240, 80 Mbps | 8.3–9.3 / 10–43 | 73–115 | 9.7 / 10.6–10.7 | 191–197 |

H.264 runs out of encoder at about 195 fps at 1080p and 72 at
3024×1964; asked for more, its frames queue behind each other (hence
~10 ms and ~27 ms). HEVC is the codec for fast or large streams. 4K60
HEVC takes 14 ms a frame.
The table predates HEVC's quality path: up to 1080p120 an HEVC frame
now takes about 7 ms (p95 8–9 ms), still inside a 120 Hz frame.

Picture quality, from the paced synthetic matrix in
[measurements/2026-11-03/mac-host](measurements/2026-11-03/mac-host/README.md)
(medians of three, FFmpeg PSNR against the exact source pictures, a
forced IDR every second; 5.1 → the speed hint alone → now):

| Case | PSNR dB | IDR max kB |
| --- | --- | --- |
| HEVC 1080p60, 20 Mbps, moving | 40.61 → 34.72 → 42.41 | 925 → 830 → 1329 |
| HEVC 1080p60, 20 Mbps, still | 42.32 → 38.69 → 43.32 | 925 → 1018 → 1279 |
| HEVC 1080p120, 50 Mbps, moving | 44.27 → 40.72 → 47.70 | 1021 → 1215 → 1970 |
| HEVC 1080p240, 80 Mbps, moving | 44.17 → 39.36 → 39.36 | 925 → 1066 → 1066 |
| HEVC 3024×1964@120, 100 Mbps, moving | 43.70 → 36.18 → 36.18 | 2524 → 2794 → 2794 |
| H.264 High 1080p60, 20 Mbps, still | 43.18 → 39.33 → 39.33 | 925 → 844 → 844 |
| H.264 High 1080p120, 50 Mbps, still | 45.18 → 42.92 → 42.92 | 1031 → 1254 → 1254 |

5.1's figures at 240 fps and 3024×1964 come from the 26–45 fps it
managed there. H.264 ignores `PrioritizeEncodingSpeedOverQuality` (its
output is bit-identical either way). It trails 5.1 by up to 4.5 dB
on some cases and gains a little on others; the low-latency rate control
gets 1 dB back at 1080p60/20 but doubles the encode time, so H.264 stays
as it is. HEVC's quality path buys its PSNR with larger IDRs.

Keyframes stay large. VideoToolbox holds P-frames to `DataRateLimits`
but not keyframes: an IDR is 20–40× a P-frame at 1080p (0.8–2 MB).
Nothing found bounds the IDR alone. `ConstantBitRate` and the macOS 26
VBV keys pad P-frames and leave the IDR as it was, `MinAllowedFrameQP`
takes effect only at a keyframe and then holds for the whole GOP (every
P-frame capped too), and `BaseFrameQP` works only under the low-latency
rate control above. So a lost frame is best answered without an IDR
(reference invalidation), and the IDR that remains is paced on the wire.

VideoToolbox recovery uses the public macOS 12 LTR API, loaded at runtime.
`EnableLTR` must succeed before recovery is advertised. Only tokens attached to
pictures at or before the viewer's delivered base are acknowledged, then
`ForceLTRRefresh` requests a P-frame. Queued pictures before that submission
are discarded; a dropped refresh requests an IDR. Requests cannot cross an
IDR, an already acknowledged newer base, or an encoder replacement.

On M3 Pro the ordinary mode rejects LTR. `LATCH_MAC_LTR=1` opts into
`EnableLowLatencyRateControl`, where H.264 High and HEVC accept it. This mode
remains opt-in because of its throughput/rate-control cost documented above.
The IOSurface test `ltr_recovery_matches_uninterrupted_decoding` compares
uninterrupted and dropped-frame output byte for byte in VideoToolbox and
FFmpeg, including a lost recovery frame. No Screen Recording permission is
needed. Unsupported sessions retain IDR recovery.

For a permission-free live host, add `LATCH_TEST_VIDEOTOOLBOX=1` to
`LATCH_TEST_PATTERN=sweep` when running `serve`. It feeds synthetic NV12
IOSurfaces to VideoToolbox through the existing CPU picture pipeline. This
measures conversion plus encode, rather than ScreenCaptureKit or the
asynchronous capture pipeline. `LATCH_TEST_VIDEO_LOSS=1:8` or `3:8` on
`view` drops datagram bursts; always use a private test port.

## Permissions

- **Local network**: macOS 15 may ask; allow it so the wake broadcast can
  go out.
- **Incoming connections**: if the macOS firewall is on it may ask once,
  the first time the Mac serves a Latch Host install through the stream.
- Only connecting, nothing else: Latch captures no screen and reads the
  keyboard and mouse only in its own window.
- **Microphone**: asked for only when **Send my microphone** is on, the
  first time a stream starts with it. Until it is allowed (System
  Settings > Privacy & Security > Microphone), macOS hands Latch silence,
  so the PC hears nothing.
- Sharing this Mac with a viewer that sends its microphone needs a virtual
  cable such as [BlackHole](https://existential.audio/blackhole/): Latch
  plays the viewer's microphone into its output, and apps on this Mac pick
  BlackHole as their microphone. `LATCH_MIC_DEVICE` names another output.
- Sharing this Mac also needs **Screen Recording** (the picture and the
  sound, through ScreenCaptureKit) and **Accessibility** (the viewer's
  keyboard and mouse). macOS asks once, when you set up sharing; after
  that the switches are in System Settings > Privacy & Security, and the
  **Sharing** tab has a button that opens each pane. macOS applies a
  newly allowed permission only after Latch restarts, so once a pane has
  been opened the tab offers **Restart Latch**, which restarts the
  background service that records and moves the pointer.

## Uninstalling

`/Applications/Latch.app/Contents/MacOS/Latch --uninstall` stops the
background service, removes its login item (under the names earlier
versions used too), removes Latch from the macOS firewall's list if setup
added it (macOS asks for an administrator), and resets Latch's Screen
Recording and Accessibility permissions. Running it again is harmless.
Then drag `Latch.app` to the Trash; settings stay in
`~/Library/Application Support/io.github.MrBeldum.Latch` until you delete
that folder.

## Connected, but the picture is black

A connection can carry valid video that contains only black pixels.
Latch now detects this after five seconds and shows a persistent message
instead of presenting the stream as healthy. **Restart stream** reconnects
and, for Desktop, resets the PC's capture session. If no video arrives or
decoding fails, the message distinguishes those problems too.

The notice then asks the PC what it can see and says which cause it is.

On a PC with no monitor attached, it is usually the colour mode. Windows
carries on composing the desktop in HDR or wide colour, as the monitor
that is now gone once asked for, on a placeholder display that reports no
luminance at all. Anything converting that desktop to an ordinary picture
turns every frame black, even though the PC's own desktop draws perfectly
normally — which is why the stream looks broken and the PC does not.

Where that mode is one the display supports, the notice offers **Turn off
HDR on the PC**, which does exactly that over the control API and starts a
fresh capture. Where Windows is enforcing it on a placeholder display it
refuses every switch away from it: the mode is a consequence of having no
display rather than a setting, and only giving the PC a display clears it.
Attach a monitor, plug in an HDMI/DisplayPort dummy plug, or install a
virtual display driver on the PC, and select that display for streaming.

The notice names the other causes too — a desktop that is genuinely black
because the display is asleep, a lock screen the PC cannot capture, or a
capture failing while the desktop plainly has a picture.

## Updates

The app checks GitHub for a new release about every six hours and twenty
seconds after it starts. A newer version is downloaded to
`~/Library/Application Support/io.github.MrBeldum.Latch/updates/<tag>/`, checked against the
SHA-256 GitHub publishes for the asset and against its own code signature,
and moved over `/Applications/Latch.app` once no stream is running; the
app then relaunches. The Mac also sends the new `latch-host.exe` to each
PC whose Latch Host is older and already speaks `/v1/update` (3.1+).

A PC still on 3.0 cannot take that, and nobody may be at the PC to install
by hand. So the stream's **PC → Update Latch Host…** does it through
the stream: this Mac serves the new `latch-host.exe` and a short
PowerShell script on its own Tailscale address (TCP 47851, to that one PC
only, for ten minutes), presses Win+R on the PC, types one line
(`powershell -ep bypass -c "irm http://<mac>:47851/u.ps1|iex"`) and
presses Enter. A PowerShell window on the PC fetches the executable, checks
its SHA-256 against the one in the script, stops the old service, swaps
the file where it stands (found from the running process, or
`%LOCALAPPDATA%\Latch`), registers it to start at logon and starts it.
The stream is not interrupted; the toolbar reports each step and the lobby
shows the new version a few seconds later. The PC's desktop has to be
unlocked and in front, since the Run box needs it. From then on updates
arrive by themselves.

Public releases need no GitHub login. If the repository is private, the
app uses, in order: `github_token` in `client.toml`, `LATCH_GITHUB_TOKEN`
in the environment, and the token git has stored for github.com (which is
there after `install-macos-release.sh` or any `git` use with the osxkeychain
helper). Settings has the switch and a **Check now** button; the line under
it says what happened last. A copy that is not running from an app bundle
(a development build) reports the new version but does not replace itself.

## Staying reachable

- Every PC in the list, and this Mac, shows a warning while its Tailscale
  node key expires. Open the machine in the
  [admin console](https://login.tailscale.com/admin/machines) and choose
  **Disable key expiry**; the warning goes away at the next scan.
- PCs are remembered in `client.toml` with their Tailscale address and
  MAC. While Tailscale on this Mac is off they stay
  listed with when they were last seen, and **Wake** still works over the
  LAN or the public address.

## Viewer decode and high refresh

The viewer offers hardware-supported codecs in AV1 > HEVC > H.264 High >
Baseline order. A cached probe requires both VideoToolbox's hardware query
and a decoded hardware picture, including AV1 on M3 or newer hardware; older hosts skip the new name and negotiate HEVC/H.264.
AV1 sized OBUs are submitted with an `av1C` format description. All codecs
retain the IOSurface → Metal path. HEVC Main10 callers can select P010
output without losing ten-bit samples; normal sessions still request NV12
and SDR. HDR transfer functions and display negotiation are future work.

Frame arrival wakes egui immediately. `AutoNoVsync` and a maximum frame
latency of one avoid an application 60 Hz cap on ProMotion. The newest
picture is prepared for rendering without an extra frame queue; previous
IOSurfaces live until their GPU submissions complete. Physical scanout and
compositor delay remain display-dependent. See the [measurements and
native viewer command](measurements/2026-11-03/viewer/README.md).

### Transport measurement (2026-11-03)

The viewer drains UDP in reusable 32-datagram `recvmsg_x` batches and hands
completed video to decode within the batch. Unsupported batch receive falls
back on the nonblocking socket. Host sends retain the shared unconnected UDP
socket and scalar sends. See [transport measurements](measurements/2026-11-03/transport/README.md)
for measured headroom and outstanding MTU/offload work.

### HDR viewing

P010 decoder output remains ten-bit through the Metal texture cache. A
separate RGBA16Float CAMetalLayer opts into extended dynamic range and uses
extended linear Display P3. PQ is decoded to absolute luminance, converted
from BT.2020 to P3, and mapped into NSScreen's current EDR headroom. The
viewer advertises HDR only when the native path initializes on an EDR
screen; moving to an SDR screen compresses highlights to that screen.
Ten-bit SDR also uses the float layer, preserving gradations. The existing
eight-bit SDR shader and surface path remain in use for legacy streams.

The float layer is inserted *below* wgpu's Metal layer. While it shows,
wgpu's layer is made non-opaque and the video callback writes transparent
pixels over the picture, so CoreAnimation composites the toolbar, stats,
settings and toasts over HDR video at SDR white; the picture's own values
never change with the controls. Chroma is sampled at its signalled siting
(left: a quarter chroma texel right of the centred position).

The native shader has offscreen Metal numerical tests (headroom, left
chroma siting), and an egui test toggles an overlay over HDR patches and
checks the EDR values and egui's transparency stay put; the full HDR
encode/decode/wgpu SDR tone-map test compares against source pixels. Live
ScreenCaptureKit HDR capture still needs a Screen Recording authorized
process. See [measurements](measurements/2026-11-03/hdr/README.md).
