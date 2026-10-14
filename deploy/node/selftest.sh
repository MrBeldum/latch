#!/usr/bin/env bash
# Static checks for the node kit: compose file, entrypoint, Dockerfile pins.
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
need "$ROOT/Dockerfile" "v2026.906.222525"
need "$ROOT/entrypoint.sh" "latch-host --background"
need "$ROOT/entrypoint.sh" "userspace-networking"
need "$ROOT/entrypoint.sh" "packetsize = 1184"
need "$ROOT/entrypoint.sh" "sw_preset = superfast"
need "$ROOT/entrypoint.sh" "minimum_fps_target = 0"
need "$ROOT/entrypoint.sh" "latch-engine"
need "$ROOT/entrypoint.sh" '"name": "Desktop"'
need "$ROOT/docker-compose.yml" "latch-node"
need "$ROOT/docker-compose.yml" "node-tailscale"
# Sunshine rejects the entire catalog when its required `env` object is
# absent, even though its HTTP health endpoints still return success.
"$PYTHON" - "$ROOT/entrypoint.sh" <<'PY'
import json
from pathlib import Path
import sys

script = Path(sys.argv[1]).read_text()
catalog = script.split("cat >/root/.config/sunshine/apps.json <<'EOF'\n", 1)[1].split("\nEOF", 1)[0]
data = json.loads(catalog)
assert isinstance(data["env"], dict), "Sunshine requires an environment object"
assert any(app.get("name") == "Desktop" for app in data["apps"]), "No desktop to stream"
PY
"$PYTHON" -m unittest discover -s "$ROOT" -p 'test_*.py'
if grep -q '47989:47989' "$ROOT/docker-compose.yml"; then
  echo "GameStream must not be published on the public internet" >&2
  fail=1
fi
test -x "$ROOT/entrypoint.sh" || chmod +x "$ROOT/entrypoint.sh"
exit "$fail"
