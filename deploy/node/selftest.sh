#!/usr/bin/env bash
# Static checks and preference persistence for the node kit.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
PYTHON=${PYTHON:-python3}
fail=0
need() {
  if ! grep -q "$2" "$1"; then
    echo "missing in $1: $2" >&2
    fail=1
  fi
}
need "$ROOT/Dockerfile" "latch-host"
need "$ROOT/Dockerfile" "ubuntu:24.04"
need "$ROOT/Dockerfile" "EXPOSE 47850/tcp 47850/udp"
need "$ROOT/entrypoint.sh" "latch-host --background"
need "$ROOT/docker-compose.yml" "/dev/net/tun:/dev/net/tun"
need "$ROOT/docker-compose.yml" "NET_ADMIN"
need "$ROOT/entrypoint.sh" "/dev/net/tun"
# Userspace Tailscale makes every peer look like 127.0.0.1 to Latch Host,
# which trusts loopback as this machine.
if grep -v '^[[:space:]]*#' "$ROOT/entrypoint.sh" | grep -q 'userspace-networking'; then
  echo "entrypoint.sh must not run Tailscale in userspace mode" >&2
  fail=1
fi
# Every .env holds a Tailscale auth key and must stay out of the build
# context, at any depth (deploy/node/.env, deploy/relay/.env).
REPO="$(cd "$ROOT/../.." && pwd)"
need "$REPO/.dockerignore" '^\*\*/\.env$'
need "$REPO/.dockerignore" '^\*\*/\.env\.\*$'
need "$ROOT/Dockerfile" "cargo build --locked"
need "$ROOT/docker-compose.yml" "latch-node"
need "$ROOT/docker-compose.yml" "node-tailscale"
"$PYTHON" -m unittest discover -s "$ROOT" -p 'test_*.py'
test -x "$ROOT/entrypoint.sh" || chmod +x "$ROOT/entrypoint.sh"
exit "$fail"
