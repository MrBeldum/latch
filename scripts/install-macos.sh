#!/usr/bin/env bash
# Build and install the Latch Mac app into /Applications. A locally built
# app never carries the quarantine flag, so Gatekeeper leaves it alone.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
rustup target add aarch64-apple-darwin >/dev/null
cargo build --release -p latch-host --target aarch64-apple-darwin
bash "$ROOT/scripts/bundle-macos.sh"
APP="$ROOT/dist.noindex/Latch.app"
if [[ ! -d "$APP" ]]; then
  echo "bundle failed" >&2
  exit 1
fi
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
rm -rf /Applications/Latch.app
cp -R "$APP" /Applications/Latch.app
# Unregister the staging copy; the installed app is in /Applications.
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
"$LSREGISTER" -u "$APP" 2>/dev/null || true
"$LSREGISTER" -f /Applications/Latch.app 2>/dev/null || true
rm -rf "$APP"
# An install from before 4.1 (the app was called BroLink): stop its
# background job, drop its login item and remove its app, so only Latch is
# left. The new app registers its own login item when it first runs.
OLD_AGENT="gui/$(id -u)/dev.brolink.node"
launchctl bootout "$OLD_AGENT" >/dev/null 2>&1 || true
rm -f "$HOME/Library/LaunchAgents/dev.brolink.node.plist"
rm -rf /Applications/BroLink.app
# The background service keeps running the old binary until it restarts:
# replacing the bundle does not stop it, and launchd restarts it only after
# a failure. Restart it through launchd so it runs the new one now.
AGENT="gui/$(id -u)/com.bardbro.latch.node"
if launchctl print "$AGENT" >/dev/null 2>&1; then
  launchctl kickstart -k "$AGENT" && echo "restarted the Latch background service"
fi
echo "installed /Applications/Latch.app"
echo "open it; it lists every machine on your Tailscale account, and can share this Mac."
