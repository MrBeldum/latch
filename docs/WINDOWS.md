# Windows

`latch-host.exe` is Latch on Windows: a background service other
machines talk to over Tailscale, a window that lists every machine on
your account (Connect opens their desktop) and shares this PC, and the
built-in streaming engine that captures and streams the desktop.

## Requirements

- Windows 10 1809+ or Windows 11, 64-bit
- A GPU the streaming engine can encode on (AMD, NVIDIA, Intel)
- [Tailscale](https://tailscale.com/download/windows), signed in with the
  same account as the Mac
- Wired Ethernet if you want to wake the PC from the Mac

## Install

1. Unzip the release and run `latch-host.exe`, or run `install-host.ps1`
   to copy it to `%LOCALAPPDATA%\Latch`, add shortcuts and an entry in
   Settings > Apps, and register the background service to start at logon.
   The script stops a running Latch to replace it and starts the service
   again; `-NoStart` only keeps the window closed.
2. Open the **Sharing** tab and click **Set up as administrator**. Windows
   shows one UAC prompt, for Windows PowerShell: that is Latch's setup. It
   copies `latch-host.exe` into `%ProgramFiles%\Latch\Setup` (a folder
   only administrators can change), checks that the copy's SHA-256 is that
   of the file the window was started from, and runs the copy as
   administrator, so nothing elevated loads or runs from a folder a user
   can write to. The copy runs a script that:
   - retires Latch 4 and BroLink: the engine services (`LatchStream`,
     `BroLinkStream`), the tasks (`LatchEngineUser`, `BroLinkEngineUser`,
     `LatchCursorUser`, `BroLinkCursorUser`), the engine folders
     (`%ProgramFiles%\Latch\engine`, `%ProgramFiles%\BroLink\engine`), a
     `SunshineService` installed in one of them, and the old firewall
     rules;
   - copies the checked `latch-host.exe` into `%ProgramFiles%\Latch\Input`
     and registers the `LatchInput` task, which runs that copy with
     `--input-helper` as the user who ran setup, elevated (see
     [Windows that run as administrator](#windows-that-run-as-administrator));
   - adds firewall rules: TCP 47850 inbound and UDP 47850 inbound for
     `latch-host.exe` from Tailscale's `100.64.0.0/10` and
     `fd7a:115c:a1e0::/48` only, and UDP 9 for the wake-packet listener;
   - on a PC with the [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver),
     lists every screen size a Mac can ask for in its `vdd_settings.xml`
     (a minimal file when there is none; everything else in it is kept)
     and restarts the display, so the engine can switch the PC to the
     Mac's size;
   - records Fast Startup and the AC sleep and hibernate timeouts as they
     were (once, under `HKLM\SOFTWARE\Latch\Setup`, for `--uninstall`),
     then turns Fast Startup off (`HiberbootEnabled = 0`) and sets both
     timeouts to never, so a PC on mains power stays reachable;
   - on the adapter that has the default route, enables wake on magic
     packet and writes the driver keywords `*WakeOnMagicPacket`,
     `*ModernStandbyWoLMagicPacket`, `S5WakeOnLan`, `*PMARPOffload` and
     `*PMNSOffload`, restarts the adapter if any changed, and lets the
     device wake the PC (`powercfg /deviceenablewake`).

   The script's transcript is in `%ProgramFiles%\Latch\Setup\setup.log`
   and is shown on the Sharing tab if something fails. Every step is
   idempotent; run setup again after fixing whatever it complained about.
   If the prompt is declined, the tab says an administrator's approval is
   needed and offers **Try again**.

   Run setup at the PC, not through a stream. Windows shows the
   administrator prompt on its secure desktop, which a stream can neither
   show nor click, so a viewer would see a frozen picture until someone at
   the PC answered it; setup may also restart the network card and the
   virtual display, which ends the stream. While a viewer is connected the
   first click only says so, and a second click within two minutes runs
   setup anyway.
3. The Sharing tab says **Shared** with a green dot. Leave the window
   closed; the service keeps running.

### Over SSH

An SSH (or WinRM) session runs in session 0, which has no desktop: a
Latch started there could not see the screen. Both steps account for it
when run from an administrator's SSH session:

- `install-host.ps1` starts the background service in the signed-in
  user's session through a one-off scheduled task (`LatchHostStart`, no
  trigger; its registration is removed after the startup attempt without
  stopping the running host). If nobody is signed in it says so, and the Run
  key starts the service at the next sign-in.
- `latch-host.exe --setup-elevated`, run from the installed copy, does
  what the button does without a prompt (the session is already
  elevated): it stages itself in `%ProgramFiles%\Latch\Setup` and runs
  setup from there, with the firewall rules naming the file you ran.
  `--local-app-data`, which 5.0.0 needed, is accepted and ignored.

The streaming engine is part of the background service, so it streams the
desktop of the session the service runs in.

## Windows that run as administrator

Windows ignores input that a program sends while a window of a higher
integrity level is in front (User Interface Privilege Isolation), and tells
the program it was delivered. Latch runs unelevated in the signed-in
session, so on its own it cannot move the pointer or type while an
installer, Task Manager, an administrator's terminal or a game launcher that
runs as administrator (HoYoPlay, for one) has focus: the viewer's pointer
freezes until that window is closed at the PC.

Each stream therefore starts the `LatchInput` task that setup registers. It
runs the copy of Latch in `%ProgramFiles%\Latch\Input`, a folder only
administrators can change, elevated, as the user who ran setup, with no
prompt. The stream sends its input to that helper through a named pipe
(`\\.\pipe\LatchInput-<session>`) that only that user's processes in the
same session can open, and only after checking that the pipe's server is
that very copy; until the helper answers, and if it ever goes away, Latch
injects input itself. The helper lets go of every key and button its stream
held when the stream ends, and exits a minute after the last stream.

Updates replace only the copy in `%LOCALAPPDATA%`. The helper keeps
serving newer versions until one changes how the two talk; then
`service.log` says the helper did not answer, and running setup again
brings it up to date. The Sharing tab asks for setup while the helper is
missing, and `service.log` names any window that ignores the viewer's
input because it runs as administrator.

Any program running as that user can start the task, so it lets that user's
unelevated programs send keyboard and mouse input to their own elevated
windows, which Windows would otherwise refuse. Windows' secure desktop (UAC
prompts, the sign-in and lock screens) still cannot be reached: it needs a
SYSTEM service, which Latch 5 does not install.

## Uninstalling

**Uninstall** on Latch's entry in Settings > Apps (written by
`install-host.ps1`), or `latch-host.exe --uninstall`, stops Latch and
removes, for the signed-in user, the Run values, the `LatchHostStart` task,
the shortcuts and the Apps entry; then, through one administrator prompt
(none in an elevated session), the firewall rules, the start task, the
`LatchInput` task and `%ProgramFiles%\Latch\Input`, every
Latch 4 or BroLink registration setup retires, and `%ProgramFiles%\Latch\Setup`,
and it puts back Fast Startup and the AC sleep and hibernate timeouts
recorded by the first setup. The network card's wake settings and the
virtual display's sizes stay, since they change nothing by themselves.
The program, `host.toml` and the logs stay in `%LOCALAPPDATA%\Latch`;
delete that folder to remove them. A message box says what was done.
Running it again is harmless; if the prompt is declined, run it again to
finish.

## Slow streams: the network, or the encoder

The **This PC** section of the Sharing tab has a **Network** line from
`tailscale netcheck`. "Hard NAT without UPnP" means a machine on another
network can only reach this PC
through a Tailscale relay, which adds a detour. What the Mac asks for is
the same either way, so a relayed stream is laggier, not blurrier; the Mac
shows the same thing as **Relayed via …** next to the PC. Turn UPnP (or NAT-PMP) on in the router, or forward a UDP port to
this PC, and Tailscale connects directly; IPv6 on both ends works too. The
service logs the finding each time it changes.

The **Streaming** line names the encoder the engine settled on. "software"
means no GPU encoder worked (a missing or broken driver): frames are slow
to make whatever the network does. Fix the GPU driver, then restart Latch
(sign out and in, or **Stop service** and then **Start it now** on the
Sharing tab). If it also says **no sound**, the engine found no audio device to
capture (a PC with no speakers, or a sink that was unplugged). Install a
virtual one (Steam's Streaming Speakers, or VB-CABLE), pick it as the PC's
audio sink, and restart Latch the same way.

## What the service does

`latch-host.exe --background` listens on TCP 47850 and answers these
requests, all JSON:

| Request | Effect |
|---------|--------|
| `GET /v1/status` | Name, Tailscale login and IP, LAN IP and MAC, wake state, Fast Startup state, seconds since the last wake packet arrived, the engine's state and the encoder it uses, this PC's NAT report |
| `POST /v1/stream` | Initiates streaming session via HTTP Upgrade to `latch-stream/1` |
| `POST /v1/power {"action"}` | `sleep`, `restart`, or `shutdown` (closes the running app first) |
| `GET /v1/clipboard` | The clipboard as text, with Windows' clipboard sequence number |
| `POST /v1/clipboard {"text"}` | Replaces the clipboard, so a ⌘V on the Mac pastes the Mac's text |
| `POST /v1/update` | A new `latch-host.exe`; see Updates |
| `GET /v1/display` | Why a capture may be black: monitors, the desktop's sampled brightness, the session, HDR state |
| `POST /v1/display {"advanced_color"}` | Turns the HDR (advanced colour) desktop off or on |

A request is answered only if it comes from loopback or from a Tailscale
address that `tailscale whois` attributes to the account this PC is signed
in as; a machine shared in from another tailnet is refused too. Anything
else gets a 403 before any action. Loopback, and this PC's own Tailscale
address, can be a process of any Windows user signed in here, so they may
not start a stream, push an update or use the clipboard.

The service also listens on UDP 9. A magic packet for this PC's MAC that
arrives while it is awake is logged and reported in the status, which is
what **Test wake** on the Mac reads.

Logs: `%LOCALAPPDATA%\Latch\service.log` and `panel.log`; setup's own
transcript is `%ProgramFiles%\Latch\Setup\setup.log`.

## Waking it from the Mac, and turning it off

The **This PC** card shows the adapter, its MAC, whether Windows will wake
on a magic packet, whether Fast Startup is on, and when a wake packet last
arrived. Setup arms all of it. The Mac learns the MAC and LAN address the
first time it sees this PC awake, and from then on **Connect** on the Mac
wakes it.

| PC state | Wake from the same network | Wake from elsewhere |
|----------|----------------------------|---------------------|
| Sleep (S3 or modern standby, plugged in) | yes | if the router forwards UDP 9 to the PC, or a Tailscale subnet router is on the PC's network |
| Shut down, Fast Startup off | if the firmware allows wake from power off (often called "Power on by PCI-E" or "Wake on LAN from S5"; ErP must be off) | same, plus the router condition |
| Shut down with Fast Startup on | no; setup turns it off | no |

Leave the PC **on** if you want Tailscale from another network. Asleep,
Tailscale is off; a Mac that is not on this LAN cannot wake it unless the
router forwards UDP 9. **Keep this PC awake while plugged in** (on by
default) stops idle sleep; Sleep from the Mac or the Start menu still
works. Untick **Let a paired Mac sleep, restart, or shut down this PC**
if you would rather the Mac could not. Remote power actions force-close
programs, because nobody is there to answer a save prompt.

After a **restart**, Latch starts when someone signs in: it runs in the
signed-in session, so the Mac cannot stream the sign-in screen. On a PC
nobody sits at, turn on Windows' automatic sign-in.

## Streaming with the monitor off or disconnected

Windows needs an active display for capture. If the connection works but the picture
is black, attach a monitor or an HDMI/DisplayPort dummy plug, or install the signed
[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver/releases).
It supplies a display even when the physical monitor is off. Setup lists
every screen size a Mac can ask for in the driver's settings file. If the
driver was installed after setup, choose **Run setup again** on the
Sharing tab; its setup list says when sizes are missing.

Latch captures the primary display (the one at the top-left of the
desktop). When that is the Virtual Display Driver's, the engine switches
it to the size the Mac asks for at each connect and back when the stream
ends, so there is nothing to choose in Windows Display settings. A physical
monitor is captured at its own size and the picture scaled. To stream at
the Mac's own size on a PC that also has a monitor, make the virtual display
the main display. Standard 1080p, 1440p and 4K streams are 16:9 and leave
bars on a taller Mac screen; **Match screen** uses the Mac's aspect ratio.

The pointer is drawn into the picture where Windows draws it. Latch 4 ran
a separate cursor guard (`LatchCursorUser`) that kept a game's custom
cursor from leaving the screen; Latch 5 does not have one, and setup
removes the old task. A game cursor whose visible pixels sit to the right
of or below its hotspot can therefore be cut off at the right or bottom
edge.

## Troubleshooting

- **The picture is an enlarged, blurry desktop**: the PC's display could not
  switch to the size the Mac asked for. On a PC with the Virtual Display
  Driver, run setup again so that size is listed; a monitor only offers its
  own sizes, and the stream is scaled from the nearest.
- **Amber "Needs setup" that will not clear**: open **Setup log** on the
  Sharing tab, or read `%ProgramFiles%\Latch\Setup\setup.log`. The
  usual causes are the UAC prompt being dismissed, or a driver that refuses
  the wake keywords.
- **Other machines list the PC as online, but "Latch isn't installed"**:
  the control service is not running or the firewall rule is missing. Open
  Latch on the PC (it restarts the service) and choose **Run setup again**
  on the Sharing tab.
- **The Mac says the PC is asleep but it is on**: Tailscale's online flag
  lags by up to half a minute; Connect probes the PC directly and will work.
- **Test waking it from this network says the packet did not arrive**: the packet is
  being sent but not delivered. From another network the PC's router has to
  forward UDP 9 to the PC's LAN address (give the PC a DHCP reservation so
  the address is stable). On the same network, check that Windows classes
  it as Private; a Public network drops the broadcast.
- **Test wake passes but the PC does not wake**: the network path is fine
  and Windows is the problem. Check **Wake-on-LAN** on the Sharing tab for
  "Off" or "Fast Startup is on" and run setup again; for wake from power off, look for the
  firmware setting named above.

## Updates

Latch Host does not download anything. The Mac fetches each release and
POSTs the new `latch-host.exe` to `/v1/update` on the control port with
its version and SHA-256, from a machine on the PC's own Tailscale account,
the same check that guards remote power actions. The service verifies the
digest, that the bytes are a Windows executable and a newer version, writes
`latch-host.exe.new` beside itself, renames the running file to
`latch-host.exe.old`, moves the new one in and starts it with
`--replaces <pid>`; the new service waits for the old one to release the
port, then removes the `.old` file. Both events appear in the host log and
the streaming session, if any, is not interrupted.

Hosts older than 3.1 have no update route, and the Mac does not POST the
executable at them (that used to show as a broken pipe). Instead the Mac
installs 3.1 through the stream: from the stream's **PC → Update Latch
Host…**, the Mac serves the new executable on its Tailscale address,
presses Win+R here, types `powershell -ep bypass -c "irm
http://<mac>:47851/u.ps1|iex"` and presses Enter. The script fetches the
executable from the Mac, checks its SHA-256, asks the running service to
quit, replaces the file where it is, registers it under the Run key and
starts it. The desktop has to be unlocked. After that, updates are
automatic.

## Staying reachable

The background service starts when you sign in to Windows by default and
sets that again at every start, so a PC nobody can reach in person comes
back after a restart once someone (or Windows' automatic sign-in) signs
in; **Start Latch when you log in** on the Sharing tab is the only thing
that turns it off. It also holds Windows awake while plugged in (**Keep
this PC awake while plugged in**, same place), because a sleeping PC's
Tailscale is asleep and a Mac on another network cannot wake it. The
streaming engine is part of that service and streams the signed-in
desktop; it cannot show the sign-in screen. Keep the PC's Tailscale key
from expiring by disabling key expiry for it in the
[admin console](https://login.tailscale.com/admin/machines); the Mac warns
about this for every PC it lists.
