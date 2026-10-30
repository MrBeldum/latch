#!/usr/bin/env bash
# Install the prebuilt Mac app from the latest GitHub release without the
# Gatekeeper "damaged" dialog: files fetched from the command line carry no
# quarantine flag, unlike a browser download. Usage, from a checkout or a
# copy of this file:
#
#   bash scripts/install-macos-release.sh
#
set -euo pipefail
REPO="${LATCH_REPO:-MrBeldum/latch}"
URL="https://github.com/$REPO/releases/latest/download/latch-macos-arm64.tar.gz"
API="https://api.github.com/repos/$REPO/releases/latest"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "downloading $URL"
if ! curl -fsSL -o "$TMP/latch-macos-arm64.tar.gz" "$URL"; then
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    echo "anonymous download failed; downloading with gh"
    gh release download --repo "$REPO" --pattern latch-macos-arm64.tar.gz --dir "$TMP" --clobber
  else
    echo "download failed; a private repository requires gh auth login" >&2
    exit 1
  fi
fi

echo "checking SHA-256"
DIGEST=""
if JSON=$(curl -fsSL -H "Accept: application/vnd.github+json" "$API" 2>/dev/null); then
  DIGEST=$(printf '%s' "$JSON" | python3 -c '
import json, sys
rel = json.load(sys.stdin)
for a in rel.get("assets") or []:
    if a.get("name") == "latch-macos-arm64.tar.gz":
        d = a.get("digest") or ""
        print(d[7:] if d.lower().startswith("sha256:") else d)
        break
' 2>/dev/null || true)
fi
if [[ -z "$DIGEST" ]] && command -v gh >/dev/null 2>&1; then
  DIGEST=$(gh api "repos/$REPO/releases/latest" --jq '
    .assets[] | select(.name=="latch-macos-arm64.tar.gz") | .digest // ""
  ' 2>/dev/null | sed 's/^[Ss][Hh][Aa]256://' || true)
fi
if [[ ! "$DIGEST" =~ ^[0-9a-fA-F]{64}$ ]]; then
  echo "release published no SHA-256 for latch-macos-arm64.tar.gz; refusing to install" >&2
  exit 1
fi
GOT=$(shasum -a 256 "$TMP/latch-macos-arm64.tar.gz" | awk '{print $1}')
lc() { printf '%s' "$1" | tr 'A-Z' 'a-z'; }
if [[ "$(lc "$GOT")" != "$(lc "$DIGEST")" ]]; then
  echo "download does not match the published SHA-256" >&2
  echo "  expected $DIGEST" >&2
  echo "  got      $GOT" >&2
  exit 1
fi

tar xzf "$TMP/latch-macos-arm64.tar.gz" -C "$TMP"
APP="$TMP/Latch.app"
if [[ ! -x "$APP/Contents/MacOS/Latch" ]]; then
  echo "the download holds no Latch.app" >&2
  exit 1
fi
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
codesign --verify --deep --strict "$APP"

DEST="/Applications/Latch.app"
STAGE="/Applications/.Latch-install-$$.app"
rm -rf "$STAGE"
cp -R "$APP" "$STAGE"
# Swap last: a failed copy must not delete the live app.
if [[ -d "$DEST" ]]; then
  PREV="/Applications/.Latch-previous-$$.app"
  rm -rf "$PREV"
  mv "$DEST" "$PREV"
  if ! mv "$STAGE" "$DEST"; then
    mv "$PREV" "$DEST"
    echo "could not replace /Applications/Latch.app; previous copy restored" >&2
    exit 1
  fi
  rm -rf "$PREV"
else
  mv "$STAGE" "$DEST"
fi
# An install from before 4.1: stop its
# background job, drop its login item and remove its app, so only Latch is
# left. The new app registers its own login item when it first runs.
OLD_AGENT="gui/$(id -u)/dev.brolink.node"
launchctl bootout "$OLD_AGENT" >/dev/null 2>&1 || true
rm -f "$HOME/Library/LaunchAgents/dev.brolink.node.plist"
rm -rf /Applications/BroLink.app
# Retire the previous Latch login item; the app registers its current one.
launchctl bootout "gui/$(id -u)/com.bardbro.latch.node" >/dev/null 2>&1 || true
rm -f "$HOME/Library/LaunchAgents/com.bardbro.latch.node.plist"
# The background service keeps running the old binary until it restarts:
# replacing the bundle does not stop it, and launchd restarts it only after
# a failure. Restart it through launchd so it runs the new one now.
AGENT="gui/$(id -u)/io.github.MrBeldum.Latch.node"
if launchctl print "$AGENT" >/dev/null 2>&1; then
  launchctl kickstart -k "$AGENT" && echo "restarted the Latch background service"
fi
echo "installed $DEST"
open "$DEST"
