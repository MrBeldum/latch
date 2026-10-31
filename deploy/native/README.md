# Native desktop on the relay VPS

The deployed `relay` desktop runs directly on Ubuntu as user `ubuntu`, not in
Docker. The four remaining containers provide the Tailscale peer relay and the
three website services. Tailscale identity and streaming pairing were preserved
when retiring `latch-node` on 2026-09-21 UTC.

## Runtime

- Xvfb provides a headless 1920x1080 display on `:1`; XFCE runs on the VPS host.
- `latch-host` runs the streaming engine in-process, capturing X11 and encoding
  H.264 in software via `latch-codec`. No external engine package is required.
- `/usr/local/bin/latch-host` runs the Latch control service.
- `/usr/local/bin/tailscale` is the CLI only. It uses the existing relay
  container's socket at `/var/run/tailscale/tailscaled.sock`; no second daemon
  or Tailscale identity is installed on the host.
- `ubuntu` has systemd lingering enabled so the desktop starts without SSH login
  and keeps running after one ends; `migrate-legacy.sh` turns it on.
- The host name is `relay`; cloud-init is configured to preserve it.

## Files and services

The `.service` files in this directory are installed in
`~/.config/systemd/user/`. Latch maintains its own `latch.service` base unit;
`latch.service.d/desktop.conf` adds the display environment and engine startup
ordering without conflicting with Latch's automatic registration.
`wait-engine.py` is installed as `/usr/local/libexec/latch-wait-engine.py`.

`latch-display.service` carries `CPUWeight=1000` so the X server gets prioritized
CPU share on a small VPS. `latch.service` runs the streaming engine in-process
with display environment from `latch.service.d/desktop.conf`, pulling in
`latch-desktop.service` and `latch-display.service`. PulseAudio runs as its standard
user service. Check them as `ubuntu`:

```sh
systemctl --user status latch latch-desktop latch-display
journalctl --user -u latch -n 50
```

Latch's host configuration lives in `~/.local/share/latch/host.toml`.
Legacy Sunshine configurations and `latch-engine.service` are removed by
`migrate-legacy.sh` and `latch-host --setup`.

The X server disables TCP and uses an authentication cookie in
`/run/user/1001/latch.Xauthority`. Mouse and keyboard input are injected
directly via XTest.

## Installing and updating latch-host

Releases publish the Linux host as `latch-host-linux-arm64.tar.gz` and
`latch-host-linux-x64.tar.gz`, each with a `.sha256` beside it. They are built
and tested on Ubuntu 24.04 by the release workflow, so they need glibc 2.39
or later. The Mac pushes updates to Windows hosts only; a Linux host is
updated by installing the next release the same way:

```sh
tag=vX.Y.Z    # the release to install
arch=arm64    # or x64
base=https://github.com/MrBeldum/latch/releases/download/$tag
curl -fLO "$base/latch-host-linux-$arch.tar.gz"
curl -fLO "$base/latch-host-linux-$arch.tar.gz.sha256"
sha256sum -c "latch-host-linux-$arch.tar.gz.sha256"
tar xzf "latch-host-linux-$arch.tar.gz"
sudo install -m 0755 "latch-host-linux-$arch/latch-host" /usr/local/bin/latch-host
systemctl --user restart latch.service
```

The archive also carries this directory as `native/`. Re-running
`migrate-legacy.sh` from it (safe twice) reinstalls the units and
reloads systemd, which is how a change to them (such as the desktop's
restart policy) reaches a running node without restarting anything.

## A desktop that takes the viewer's size

Xvfb cannot change size, so the relay desktop is streamed at 1920x1080 (a
smaller size the Mac asks for is that desktop scaled down). An X server that
can resize (Xorg with the dummy driver) can instead switch to each stream's
size: give Sunshine a `global_prep_cmd` that runs `xrandr` with
`SUNSHINE_CLIENT_WIDTH` and `SUNSHINE_CLIENT_HEIGHT`, and set
`LATCH_DISPLAY_FOLLOWS_VIEWER=1` in `latch.service.d/desktop.conf` so Latch
Host stops reporting a fixed capture size. Use a mode at 60 Hz for every size
(`xrandr --newmode` with a pixel clock of 60 x htotal x vtotal): the
compositor and browsers draw at the mode's rate.

In Latch 5, prep commands are specified in `host.toml` (`prep_do` / `prep_undo`)
and executed per session with `LATCH_CLIENT_WIDTH`, `LATCH_CLIENT_HEIGHT`, and
`LATCH_CLIENT_FPS` in the environment. Any existing Sunshine `global_prep_cmd`
is imported automatically during setup.

## Upgrading from versions before 4.1.0

Nodes set up before 4.1.0 use earlier names for their services, host binary,
display cookie and data folder. `migrate-legacy.sh` in this directory swaps
the units, cookie name and wait script for the current ones and starts
Latch. On its first run, `latch-host` moves the data folder and the engine's
login file, keeping the pairing and Tailscale identity.

```sh
sudo install -m 0755 latch-host /usr/local/bin/latch-host
bash migrate-legacy.sh
```

## Migration and recovery

The old container's complete `.config` and `.local` directories are saved in
`/home/ubuntu/latch-native-migration/container-backup/` with private permissions.
They contain pairing credentials; do not commit or publish them.
The original Docker recipe remains available in `deploy/node/` for other uses,
but it must not be started on this VPS alongside the native services because
both would claim the same streaming and control ports.

Verified through the installed Mac Latch app: existing pairing, 1080p/60 H.264
video, mouse clicks, terminal keyboard input, host identity (`relay`, `ubuntu`,
`kvm`, `/home/ubuntu`), and clean disconnect. User-manager restart is checked to
verify automatic startup without rebooting the websites or packet relay.
