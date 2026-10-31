<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/brand/logo-dark.svg">
  <img src="docs/brand/logo.svg" alt="Latch" width="64" height="64">
</picture>

# Latch

Your machines, on any of your machines: stream a desktop full screen over
Tailscale. Mac, Windows, Linux, and a VPS container all run the same Latch
app. Each one lists the others and can Connect; each one can share its own
desktop.

Latch 5 streams with its own engine, written in Rust and built into the
one `latch-host` executable that also runs the control service and the
window (`latch-engine` on the sharing side, `latch-stream` on the viewing
side, `latch-proto` for the wire format, `latch-codec` for the software
H.264 codec and the audio codec). Video and sound travel over UDP with
Reed–Solomon forward error correction and a keyframe on loss, inside
Tailscale's WireGuard. A Mac shares with ScreenCaptureKit and VideoToolbox,
Windows with Desktop Duplication and the Media Foundation H.264 encoder (the
GPU's, else Microsoft's, else `latch-codec`), and Linux with X11 and
`latch-codec`. Sound is 48 kHz stereo, coded losslessly with `lpcm-rice`.
Nothing else is installed, and no crate in the build compiles C, C++ or
assembler (`scripts/audit-native.sh` checks). [docs/ENGINE.md](docs/ENGINE.md) has the design and
[docs/PROTOCOL.md](docs/PROTOCOL.md) the wire format.

[Tailscale](https://tailscale.com) is the path and the identity check. A
machine answers only peers signed in to the same Tailscale account.

## How a session goes

1. Open Latch. **Machines** lists every machine on your Tailscale
   account, with a dot and a line for what it can do right now. The
   **Sharing** tab sets this machine up so others can Connect here.
2. Click **Connect**. If a Windows PC is asleep, Latch wakes it and waits.
   Connections connect directly using Tailscale authentication without
   pairing PINs.
3. The desktop appears, full screen by default, and nothing else: the
   mouse is captured the moment you click the picture, as in a game, so
   games that read raw mouse movement work. **Ctrl+Alt** (Control-Option
   on a Mac) is the host key, as in a hypervisor: it frees the mouse and
   drops a toolbar over the
   picture with the stream details, whether the path is direct or relayed,
   the mouse mode, a **Keys** menu for Ctrl+Alt+Del and friends and what the
   Command key does, stats, full screen, the PC's power menu, **Stream
   settings** (the default asks for this screen's own size at 50 Mbps, on
   every path) and **Disconnect**. Click the picture, or press Ctrl+Alt
   again, and the toolbar goes away. The clipboard follows you both ways.
4. Leave the PC on if you want to connect from anywhere. Asleep, Tailscale
   is asleep too: this Mac can wake it only from that PC's own network
   (or if the router forwards UDP 9). Settings can offer sleep after a
   session; that is off by default.

## Install

### Windows PC

1. Install [Tailscale](https://tailscale.com/download/windows) and sign in
   with the account you use on the Mac.
2. Download `latch-windows-x64.zip` from the
   [latest release](https://github.com/MrBeldum/latch/releases/latest)
   and unzip it. Run `latch-host.exe`, or `install-host.ps1` for
   shortcuts and start-at-logon.
3. On the **Sharing** tab, click **Set up as administrator**. Windows
   asks once, naming the request Windows PowerShell: it copies Latch into
   `%ProgramFiles%\Latch\Setup`, checks the copy, and runs it as
   administrator. Setup opens TCP and UDP 47850 to Tailscale addresses only,
   removes Latch 4's streaming engine (Sunshine) and its services, turns
   Fast Startup off, keeps the PC awake on mains power, and arms the network
   card for Wake-on-LAN. The same window lists every other machine on the
   account; **Connect** opens their desktop.

`latch-host.exe --uninstall` (or Uninstall in Settings > Apps, after
`install-host.ps1`) undoes setup and puts the power settings back. Details
in [docs/WINDOWS.md](docs/WINDOWS.md).

### Mac (Apple Silicon)

Install [Tailscale](https://tailscale.com/download/mac) and sign in. One
command downloads the app, clears the quarantine flag and copies it to
`/Applications`:

```bash
curl -fsSL https://raw.githubusercontent.com/MrBeldum/latch/main/scripts/install-macos-release.sh | bash
```

Or download `latch-macos-arm64.tar.gz` from the
[latest release](https://github.com/MrBeldum/latch/releases/latest) and

```bash
tar xzf latch-macos-arm64.tar.gz
xattr -dr com.apple.quarantine Latch.app
open Latch.app
```

The `xattr` step is needed for a browser download because the app is
ad-hoc signed rather than Developer-ID signed and notarized
([docs/ROADMAP.md](docs/ROADMAP.md) lists what that needs). Open Latch and
set up sharing on the **Sharing** tab so a Windows PC (or another Mac) can
Connect here; macOS asks for Screen Recording and Accessibility, and the
tab links both settings panes and offers to restart Latch once they are
allowed.
`Latch.app/Contents/MacOS/Latch --uninstall` removes the login item and the
permissions. Details, including the toolbar and keyboard behaviour, in
[docs/MACOS.md](docs/MACOS.md).

### Linux / VPS (Docker)

A container that shares a virtual desktop on the tailnet, so Mac and
Windows Latch can Connect to it. Several copies on one host are several
machines. See [deploy/node/NODE.md](deploy/node/NODE.md).

```bash
cp deploy/node/env.example deploy/node/.env
# paste a Tailscale auth key, pick TS_HOSTNAME
docker compose -f deploy/node/docker-compose.yml up -d --build
```

A desktop can also run natively under systemd on the VPS, beside the
relay, as this project's own VPS does: [deploy/native/README.md](deploy/native/README.md).
The packet relay (when two NATs cannot punch through) is a separate kit:
[deploy/relay/RELAY.md](deploy/relay/RELAY.md).

## Upgrading from Latch 4 or BroLink

An older install updates to Latch 5 and keeps its settings. Latch 5
machines stream only with Latch 5 machines: a Latch 5 viewer that finds an
older host sends it the update (or says which machine to update), and an
older viewer finds no stream on a Latch 5 host and offers its own update.
Setup removes the old engine, its pairing data and its services.

- **Mac:** a BroLink app moves to `Latch.app` the first time it runs, and
  its login item moves with it.
- **Windows:** the host updates in place and keeps working. Run
  **Set up as administrator** once on the **Sharing** tab: it removes the
  `LatchStream` service, the `LatchEngineUser` task and
  `%ProgramFiles%\Latch\engine`. `install-host.ps1` from the new zip also
  moves a BroLink install to `%LOCALAPPDATA%\Latch`.
- **Linux:** the first start registers `latch.service` and retires the old
  service. For a native desktop, see [deploy/native/README.md](deploy/native/README.md).

Use `LATCH_DATA_DIR` and `LATCH_GITHUB_TOKEN` for environment overrides.
The equivalent names from versions before 4.1.0 still work.

## Updates

Latch keeps itself current. Every few hours the Mac app asks GitHub for
the latest release. A newer app is downloaded, checked against the digest
GitHub publishes and its own code signature, swapped into `/Applications`
once no stream is running, and relaunched. A newer Latch Host is sent
from the Mac to every PC whose host reports an older version, over the same
Tailscale-authenticated control API that can put the PC to sleep; the host
verifies the digest, replaces its executable and restarts. That path updates `latch-host.exe`, which contains the entire streaming engine. A PC that is asleep gets the host
update the next time the Mac sees it. Nothing is downloaded on the PC, and
no GitHub login is needed there.

Public releases need no GitHub login. If the repository is private, the Mac
uses the GitHub token git has stored for github.com,
`LATCH_GITHUB_TOKEN`, or `github_token` in `client.toml`. Settings has the
switch and a **Check now** button. Hosts installed before 3.1 do not have the
update route: install that release on the PC once (through the stream works),
after which updates are automatic.

## Staying reachable

A PC nobody can get to in person stays reachable when three things hold.
Latch starts when someone signs in to Windows, by default, and turns this
back on at every start unless the owner switches it off on the Sharing tab.
It runs (and streams) in the signed-in session, so after a restart the PC
answers once someone has signed in: a PC nobody sits at needs Windows'
automatic sign-in. And the PC's Tailscale node key must not expire:
Tailscale keys expire after 180 days unless key expiry is disabled for
that machine in the [admin console](https://login.tailscale.com/admin/machines),
and an expired key needs a sign-in at the PC. Latch shows the expiry of
every machine it lists, this Mac included, until expiry is disabled. The
PC's addresses are saved on the Mac, so PCs stay listed even while
Tailscale on the Mac is off.

## Waking the PC

A wake packet has to reach the PC's network card on the PC's own network.
Tailscale cannot deliver it, because the sleeping PC's Tailscale is asleep
too. Latch sends the packet to the LAN broadcast, to the PC's LAN address
and to the PC's public address.

| Where the Mac is | Asleep | Shut down |
|------------------|--------|-----------|
| Same network as the PC | works | works if the board's firmware allows wake from power off |
| Elsewhere | needs the PC's router to forward UDP 9 to the PC, or a Tailscale subnet router on that network | same, and the firmware condition |

**Test waking it from this network** in a machine's **…** menu settles it for the network you are
on: it sends the packet while the PC is awake and asks Latch there whether
it arrived. Setup on the PC takes care of Windows' side (Fast Startup off,
the adapter's wake keywords, wake allowed in power management). Wired
Ethernet is strongly preferred; most Wi-Fi adapters cannot wake a PC.

## Security

- The host's control service listens on TCP 47850 and answers only
  loopback and Tailscale addresses that `tailscale whois` attributes to the
  account the PC is signed in as. A tagged server (the relay VPS) belongs
  to no account and answers the people of its own tailnet, not other
  tagged machines. Everyone else,
  including machines shared in from other tailnets, gets a 403. A process
  on the machine itself (any user signed in to it) can read the status but
  cannot start a stream, push an update or use the clipboard. The firewall
  rules setup adds are scoped to Tailscale's `100.64.0.0/10` and
  `fd7a:115c:a1e0::/48`.
- A stream starts with an HTTP upgrade on TCP 47850 that the same check
  admits, then runs on UDP 47850 inside Tailscale's WireGuard, which is its
  encryption; there is no pairing and no second layer. One viewer at a
  time: a new one takes over.
- No Latch account or password exists.
- On Windows the administrator step never runs from the user's folder: it
  runs a copy in `%ProgramFiles%\Latch\Setup` whose SHA-256 matches the
  file the window was started from.
- Remote power actions can be turned off on the Sharing tab.

## Building

Needs Rust and the platform's linker (Visual Studio Build Tools on
Windows, the Xcode command line tools on a Mac); on Linux, the `-dev`
packages the CI's `native deps` step installs.

```powershell
cargo build --release -p latch-host          # Windows
```

```bash
./scripts/install-macos.sh                     # macOS: builds, bundles, installs
```

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the UI snapshots and the tests
that need real hardware.

## Layout

```
crates/core     control API types, small HTTP, Tailscale CLI, wake packets, config
crates/proto    wire format, FEC, packetizer and reassembler, input reliability
crates/codec    H.264 Constrained Baseline encoder and decoder, lpcm-rice audio
crates/engine   sharing side: capture, encode, audio capture, input, session server
crates/stream   viewing side: session, decoders, audio playback
crates/host     control service, setup and uninstall, unified window (view + share)
crates/client   viewer UI: machine list, wake, stream window and toolbar
crates/ui       theme and widgets shared by both windows
deploy/node/    Docker kit: virtual desktop + Latch on a VPS
deploy/native/  systemd user units for the same desktop without Docker
deploy/relay/   Docker kit: Tailscale peer relay
docs/           design, protocol and platform notes
scripts/        installers, the macOS bundle and the native-code audit
```

## License

GPL-3.0-or-later; see [LICENSE](LICENSE). Parts of the H.264 decoder are
adapted from pith-h264 (MIT). Third-party notices, fonts included, are in
[NOTICE](NOTICE).
