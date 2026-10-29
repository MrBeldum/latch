#!/usr/bin/env bash
# Move a native Linux node from the names used before 4.1.0 to the current
# ones. Run it as the user that owns the desktop,
# from this directory, after putting the new latch-host at /usr/local/bin:
#
#   sudo install -m 0755 latch-host /usr/local/bin/latch-host
#   bash migrate-legacy.sh
#
# It stops the old stack, swaps the user units, the display cookie name and
# the wait script, and starts the new one. The data folder (settings,
# preferences) are moved by latch-host itself the
# first time it runs, so nothing is copied here. Safe to run twice.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${LATCH_BIN:-/usr/local/bin/latch-host}"
UNITS="$HOME/.config/systemd/user"
OLD_UNITS=(brolink brolink-engine brolink-desktop brolink-display latch-engine)

[[ -x "$BIN" ]] || { echo "install the new latch-host at $BIN first" >&2; exit 1; }
mkdir -p "$UNITS"

echo "==> stopping the old stack"
for u in "${OLD_UNITS[@]}"; do
	systemctl --user stop "$u.service" 2>/dev/null || true
	systemctl --user disable "$u.service" 2>/dev/null || true
done
for u in "${OLD_UNITS[@]}"; do rm -f "$UNITS/$u.service"; done
rm -rf "$UNITS/brolink.service.d"

echo "==> installing the new units"
for u in latch-display latch-desktop; do
	install -m 0644 "$HERE/$u.service" "$UNITS/$u.service"
done
mkdir -p "$UNITS/latch.service.d"
install -m 0644 "$HERE/latch.service.d/desktop.conf" "$UNITS/latch.service.d/desktop.conf"
# The base unit is the one latch-host writes itself; the same text, so it
# finds nothing to change when it starts.
printf '[Unit]\nDescription=Latch\nAfter=network.target\n\n[Service]\nExecStart="%s" --background\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n' "$BIN" >"$UNITS/latch.service"

echo "==> display cookie"
env_file="$HOME/.config/environment.d/10-display.conf"
if [[ -f "$env_file" ]]; then sed -i 's/brolink\.Xauthority/latch.Xauthority/g' "$env_file"; fi
echo "==> starting Latch"
systemctl --user daemon-reload
systemctl --user enable latch-display.service latch-desktop.service
# Imports the owner's prep commands before removing the previous streaming
# unit and ~/.config/sunshine. Existing host preferences move with the data.
"$BIN" --setup
systemctl --user enable --now latch.service

for _ in $(seq 1 30); do
	if status="$(curl -fsS --max-time 2 http://127.0.0.1:47850/v1/status 2>/dev/null)"; then
		echo "$status" | python3 -c 'import json,sys; s=json.load(sys.stdin); print("app=%s version=%s name=%s streamer=%s running=%s" % (s["app"], s["version"], s["name"], s["streamer"]["kind"], s["streamer"]["running"]))'
		echo "done. The previous host binary can be removed once this looks right."
		exit 0
	fi
	sleep 2
done
echo "latch.service did not answer on 47850; see: journalctl --user -u latch -n 50" >&2
exit 1
