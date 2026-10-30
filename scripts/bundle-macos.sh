#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# The unified node binary (views and shares). When both a cross-compiled
# and a native build exist, the newer one is the one that was just built.
newest() {
  local pick=""
  for f in "$@"; do
    [[ -f "$f" ]] || continue
    if [[ -z "$pick" || "$f" -nt "$pick" ]]; then
      pick="$f"
    fi
  done
  printf '%s' "$pick"
}
BIN="$(newest "$ROOT/target/aarch64-apple-darwin/release/latch-host" "$ROOT/target/release/latch-host")"
if [[ -z "$BIN" ]]; then
  echo "build the app first: cargo build --release -p latch-host" >&2
  exit 1
fi
# Spotlight excludes .noindex folders, so this build copy stays out of app search.
APP="$ROOT/dist.noindex/Latch.app"
# The workspace version, so the bundle cannot drift from the binary inside it.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/Latch"
chmod +x "$APP/Contents/MacOS/Latch"
# The Finder/Dock icon: the ink tile with the logo knocked out, built by
# scripts/make-icons.py. macOS applies the rounded app-icon shape.
cp "$ROOT/crates/client/Latch.icns" "$APP/Contents/Resources/Latch.icns"
cp "$ROOT/LICENSE" "$ROOT/NOTICE" "$APP/Contents/Resources/"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Latch</string>
  <key>CFBundleDisplayName</key><string>Latch</string>
  <key>CFBundleIdentifier</key><string>io.github.MrBeldum.Latch</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>Latch</string>
  <key>CFBundleIconFile</key><string>Latch</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.utilities</string>
  <key>NSHumanReadableCopyright</key>
  <string>Copyright © 2026 Latch Contributors</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key>
  <string>Latch sends the wake-up packet to your PC over the local network.</string>
  <key>NSAppleEventsUsageDescription</key>
  <string>Latch asks System Events to restart or shut down this Mac when you choose that from another machine.</string>
</dict>
</plist>
PLIST
# Sign the bundle. Ad-hoc by default: Apple Silicon refuses to launch an
# unsigned binary at all, and the linker's signature does not cover the
# bundle. Set CODESIGN_IDENTITY to a "Developer ID Application" certificate
# for a build Gatekeeper accepts once notarized (see docs/MACOS.md).
IDENTITY="${CODESIGN_IDENTITY:--}"
if [[ "$IDENTITY" == "-" ]]; then
  codesign --force --sign "$IDENTITY" "$APP"
else
  # Notarization requires the hardened runtime and a secure timestamp. The
  # hardened runtime blocks Apple Events unless the app is entitled to send
  # them: a shared Mac restarts and shuts down through System Events.
  ENTITLEMENTS="$(mktemp -t latch-entitlements).plist"
  cat > "$ENTITLEMENTS" <<'ENT'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.automation.apple-events</key><true/>
</dict>
</plist>
ENT
  codesign --force --options runtime --timestamp --entitlements "$ENTITLEMENTS" --sign "$IDENTITY" "$APP"
  rm -f "$ENTITLEMENTS"
fi
codesign --verify --deep --strict "$APP"
if [[ "$IDENTITY" == "-" ]]; then
  echo "wrote $APP (ad-hoc signed; set CODESIGN_IDENTITY for a Developer ID build)"
else
  echo "wrote $APP (signed as $IDENTITY; notarize before distributing)"
fi
