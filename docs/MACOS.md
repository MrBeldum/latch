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
   The first time with a machine, Latch pairs with it: it picks a
   random PIN, sends it to Latch on that machine over Tailscale, and
   Latch there enters it for you. You see the PIN but never need it. If
   Latch is not sharing on that machine, pairing cannot finish: open
   Latch there and set up sharing on its **Sharing** tab first.
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
answered. The status line, with the session clock, the frame rate and
bitrate against what was asked for, and the round trip, is drawn over the
picture's bottom-left corner while **Stats** is open, or in the bar below
the picture when a manual size leaves one.

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
| **Stats** | The Stream performance panel: frame rate and bitrate against what was asked for, round trip, packet loss, encode, assembly, queue and decode times, and the decoder. **Copy diagnostics** puts it all on the clipboard |
| **Full screen** | Toggle; the setting decides how a session starts |
| **PC** (or **Mac**, **Machine**) | Sleep…, Restart…, Shut down…, each asking first, when the machine's owner allows them; Sleep only on a Windows PC, the one kind a wake packet can bring back. **Update Latch Host…** for a PC whose host is older than 3.1 |
| **Stream settings** | The same controls as Settings: quality profile, resolution, frame rate, bitrate and codec. **Apply and reconnect** restarts the session with them in a second or two; Escape or the close button leaves them as they were |
| **Disconnect** | End the session |

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
gets the desktop's own size, a smaller one (720p, say) is scaled down. On
a machine with fewer than four cores, Latch asks for at most 30 fps at
1080p, and more for a smaller picture in proportion to the pixels it
saves: up to 67 fps at 720p, 43 at 900p, 120 at 540p. To get 60 fps from
such a machine, pick 720p (or 540p) and 60 fps.

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
what Latch asks for. Recommended quality is this screen's size at 50
Mbps on every path, so a relayed stream is laggier, not blurrier. The
encoder holds that rate (CBR); a still desktop no longer drops to a
trickle.
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
| Quality | **Recommended** (the default) asks for this screen's own size at 50 Mbps, on every path. **Smooth** is 1080p · 60 fps · 20 Mbps, **Balanced** match screen · 60 · 50, **Sharp** match screen · 60 · 100. **Custom** uses the rows below; changing one of them switches to it |
| Resolution | **Match screen** is this display's own pixel size (3024×1964 on a 14" MacBook Pro), so the picture fills it exactly. **540p**, **720p**, **900p**, **1080p**, **1440p** and **4K** are the standard 16:9 sizes, 960×540, 1280×720, 1600×900, 1920×1080, 2560×1440 and 3840×2160; on a display of another shape they leave a bar above and below. The sizes below 1080p are for a PC that encodes in software or a slow network: fewer pixels leave more bitrate for each and a higher frame rate. The list shows the pixel size of each. The PC switches its display to the size asked for; a PC with no monitor needs a virtual display that lists that size, which setup on the PC arranges for the Virtual Display Driver |
| Frame rate | 30 to 240; the PC's display is switched to match when it can |
| Bitrate | 2 to 150 Mbps; arrow keys step by 1, Shift-arrow by 10. The encoder holds this rate. Large swings mean the path is dropping packets |
| Codec | Auto (HEVC when both ends can), HEVC, H.264 |
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

Settings are saved in `~/Library/Application Support/com.bardbro.Latch/client.toml`,
along with the MAC, LAN address and pairing certificate of each PC Latch
has paired with.

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

## Permissions

- **Local network**: macOS 15 may ask; allow it so the wake broadcast can
  go out.
- **Incoming connections**: if the macOS firewall is on it may ask once,
  the first time the Mac serves a Latch Host install through the stream.
- Nothing else. Latch captures no screen and reads the keyboard and
  mouse only in its own window.

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
`~/Library/Application Support/com.bardbro.Latch/updates/<tag>/`, checked against the
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
- PCs are remembered in `client.toml` with their Tailscale address, MAC and
  the certificate from pairing. While Tailscale on this Mac is off they stay
  listed with when they were last seen, and **Wake** still works over the
  LAN or the public address.
