#!/usr/bin/env bash
# Assemble Sonora.app (aarch64) with the Homebrew GStreamer runtime baked
# in, then wrap it in a DMG and a zip.
#
# CAUTION: this path has only been exercised on the CI runner — it has not
# been validated on real hardware. Homebrew links against /opt/homebrew,
# which doesn't exist on user machines, so every dylib reachable from the
# binary *or from any GStreamer plugin* is copied into
# Contents/Frameworks and its load commands rewritten to
# @executable_path/../Frameworks (plugins resolve that against the main
# executable's dir, i.e. Contents/MacOS → ../Frameworks). dylibbundler
# only walks the binary's own closure, hence the plugin loop below.
#
#   packaging/macos/build-app.sh <version> <path-to-release-binary>
set -euo pipefail

VER="${1:?usage: build-app.sh <version> <binary>}"
BIN="${2:?usage: build-app.sh <version> <binary>}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

APP="dist/Sonora.app"
FW="$APP/Contents/Frameworks"
PLUGINS_DIR="$APP/Contents/Resources/gstreamer/plugins"
rm -rf dist
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$FW" "$PLUGINS_DIR"

cp "$BIN" "$APP/Contents/MacOS/sonora"
install -m644 "$ROOT/crates/sonora/icons/icon.icns" \
  "$APP/Contents/Resources/sonora.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Sonora</string>
  <key>CFBundleDisplayName</key><string>Sonora</string>
  <key>CFBundleIdentifier</key><string>com.xmreur.sonora</string>
  <key>CFBundleExecutable</key><string>sonora</string>
  <key>CFBundleIconFile</key><string>sonora</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VER}</string>
  <key>CFBundleVersion</key><string>${VER}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
EOF

# The main binary's closure first (gstreamer, glib, gst's own deps …).
dylibbundler -b \
  -x "$APP/Contents/MacOS/sonora" \
  -d "$FW" \
  -p "@executable_path/../Frameworks"

# GStreamer plugins are dlopened objects, invisible to dylibbundler.
# Copy them and rewrite their Homebrew references, pulling in plugin-only
# deps (libavcodec & co) until the graph is closed. Homebrew builds the
# plugins via meson `shared_library`, which is .dylib on macOS; the .so
# glob is a fallback for a GStreamer.framework / Linux-style layout.
shopt -s nullglob
PLUGIN_OBJS=( "$(brew --prefix)"/lib/gstreamer-1.0/*.dylib
              "$(brew --prefix)"/lib/gstreamer-1.0/*.so )
shopt -u nullglob
if [ "${#PLUGIN_OBJS[@]}" -eq 0 ]; then
  echo "error: no GStreamer plugins found in $(brew --prefix)/lib/gstreamer-1.0" >&2
  exit 1
fi
cp "${PLUGIN_OBJS[@]}" "$PLUGINS_DIR/"

FW_LIB="$FW"
for _pass in $(seq 1 12); do
  added=0
  for obj in "$FW_LIB"/*.dylib "$PLUGINS_DIR"/*.dylib "$PLUGINS_DIR"/*.so; do
    [ -e "$obj" ] || continue
    while IFS= read -r dep; do
      case "$dep" in
        /opt/homebrew/*|/usr/local/*) src="$dep" ;;
        @rpath/*) src="$(brew --prefix)/lib/$(basename "$dep")" ;;
        *) continue ;; # system frameworks stay as they are
      esac
      base="$(basename "$dep")"
      dst="$FW_LIB/$base"
      if [ ! -e "$dst" ]; then
        if [ ! -e "$src" ]; then
          echo "warn: missing dylib for $obj: $src" >&2
          continue
        fi
        cp -L "$src" "$dst"
        chmod u+w "$dst"
        install_name_tool -id "@executable_path/../Frameworks/$base" "$dst"
        added=1
      fi
      # -change is a no-op failure when the reference is already fixed.
      install_name_tool -change "$dep" \
        "@executable_path/../Frameworks/$base" "$obj" 2>/dev/null || true
    done < <(otool -L "$obj" | tail -n +2 | awk '{print $1}')
  done
  [ "$added" = 1 ] || break
done

# Binaries were rewritten, and arm64 macOS requires a signature for those
# to even start; ad-hoc is enough for an unsigned release.
codesign --force --deep --sign - "$APP"

# DMG with an Applications symlink, plus a zip of the .app.
DMGROOT="$(mktemp -d)"
cp -R "$APP" "$DMGROOT/"
ln -s /Applications "$DMGROOT/Applications"
hdiutil create -volname "Sonora" -srcfolder "$DMGROOT" -ov -format UDZO \
  "Sonora-${VER}-aarch64-apple-darwin.dmg"
rm -rf "$DMGROOT"
ditto -c -k --keepParent "$APP" "Sonora-${VER}-aarch64-apple-darwin-app.zip"

ls -la "Sonora-${VER}-aarch64-apple-darwin.dmg" \
       "Sonora-${VER}-aarch64-apple-darwin-app.zip"
