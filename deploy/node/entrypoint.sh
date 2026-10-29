#!/bin/bash
# Start a virtual desktop, the streaming engine, Latch's control service,
# and (optionally) Tailscale so this container is a machine on the tailnet.
set -euo pipefail

log() { echo "[latch-node] $*"; }

install -d -m 700 /root/.local/share/latch /var/run/tailscale

if [[ -n "${TS_AUTHKEY:-}" ]]; then
  # Kernel networking only. In userspace mode tailscaled hands every peer's
  # connection to Latch Host from 127.0.0.1, so the
  # kernel TUN keeps both control and media on the tailnet.
  if [[ ! -c /dev/net/tun ]]; then
    log "Tailscale needs /dev/net/tun and NET_ADMIN (see docker-compose.yml); not starting without them"
    exit 1
  fi
  log "starting Tailscale as ${TS_HOSTNAME:-latch-node}"
  tailscaled --state=/var/lib/tailscale/tailscaled.state \
    --socket=/var/run/tailscale/tailscaled.sock >/var/log/tailscaled.log 2>&1 &
  for _ in $(seq 1 20); do
    tailscale status >/dev/null 2>&1 && break
    sleep 0.5
  done
  tailscale up --authkey="$TS_AUTHKEY" --hostname="${TS_HOSTNAME:-latch-node}" \
    --accept-dns=false
fi

log "starting Xvfb ${DISPLAY:-:1}"
rm -f /tmp/.X1-lock
Xvfb "${DISPLAY:-:1}" -screen 0 "${SCREEN_SIZE:-1920x1080x24}" -ac +extension RANDR >/var/log/xvfb.log 2>&1 &
sleep 0.4

pulseaudio --start --exit-idle-time=-1 >/var/log/pulse.log 2>&1 || true

log "starting xfce"
startxfce4 >/var/log/xfce.log 2>&1 &
sleep 1

python3 /opt/latch/bootstrap.py

log "starting Latch"
exec /usr/local/bin/latch-host --background
