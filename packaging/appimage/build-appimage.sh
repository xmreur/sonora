#!/usr/bin/env bash
# Build a self-contained Sonora AppImage.
#
# AppDir layout produced:
#   AppRun                      launcher (written here)
#   Sonora.desktop, sonora.png  files appimagetool requires at the root
#   usr/bin/sonora              stripped release binary
#   usr/lib/*.so*               ldd closure of the binary + the plugins
#   usr/lib/gstreamer-1.0/*.so  GStreamer plugins, pruned to what loads
#
# Runs on the Ubuntu release runner with the dev + runtime GStreamer
# packages installed (see .github/workflows/release.yml). appimagetool is
# fetched into target/ unless $APPIMAGETOOL points at an existing binary.
#
#   packaging/appimage/build-appimage.sh <version>
set -euo pipefail

VER="${1:?usage: build-appimage.sh <version>}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

APPDIR="$ROOT/Sonora.AppDir"
OUT="Sonora-${VER}-x86_64.AppImage"
# Debian/Ubuntu multiarch first (what the release runner uses); plain
# /usr/lib covers Arch and friends, which makes the script smoke-testable
# on a dev machine.
LIBDIR="/usr/lib/x86_64-linux-gnu"
[ -d "$LIBDIR/gstreamer-1.0" ] || LIBDIR="/usr/lib"
[ -d "$LIBDIR/gstreamer-1.0" ] || {
  echo "no GStreamer plugin dir under $LIBDIR — install the runtime plugins" >&2
  exit 1
}

rm -rf "$APPDIR" "$OUT"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/lib/gstreamer-1.0" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/32x32/apps" \
         "$APPDIR/usr/share/icons/hicolor/64x64/apps" \
         "$APPDIR/usr/share/icons/hicolor/128x128/apps" \
         "$APPDIR/usr/share/icons/hicolor/512x512/apps"

strip "$ROOT/target/release/sonora"
install -m755 "$ROOT/target/release/sonora" "$APPDIR/usr/bin/sonora"
install -m644 "$ROOT/packaging/sonora.desktop" \
  "$APPDIR/usr/share/applications/sonora.desktop"
install -m644 "$ROOT/crates/sonora/icons/32x32.png" \
  "$APPDIR/usr/share/icons/hicolor/32x32/apps/sonora.png"
install -m644 "$ROOT/crates/sonora/icons/64x64.png" \
  "$APPDIR/usr/share/icons/hicolor/64x64/apps/sonora.png"
install -m644 "$ROOT/crates/sonora/icons/128x128.png" \
  "$APPDIR/usr/share/icons/hicolor/128x128/apps/sonora.png"
install -m644 "$ROOT/crates/sonora/icons/icon.png" \
  "$APPDIR/usr/share/icons/hicolor/512x512/apps/sonora.png"

# The root files appimagetool insists on.
cp "$ROOT/packaging/sonora.desktop" "$APPDIR/Sonora.desktop"
install -m644 "$ROOT/crates/sonora/icons/128x128.png" "$APPDIR/sonora.png"
ln -sfn sonora.png "$APPDIR/.DirIcon"

# --- GStreamer plugins -----------------------------------------------------
# All of base/good/bad/libav are installed on the runner; copying them all
# keeps playbin3 free to auto-plug whatever a motion rendition needs
# (qtdemux/h264parse/avdec_*/…). Plugins with deps that don't resolve on
# the runner are pruned here instead of failing at dlopen for users.
GST_PLUGIN_SRC="$LIBDIR/gstreamer-1.0"
shopt -s nullglob
for plugin in "$GST_PLUGIN_SRC"/*.so; do
  if ldd "$plugin" 2>/dev/null | grep -q "not found"; then
    echo "pruning plugin (unresolved deps): $(basename "$plugin")"
    continue
  fi
  cp "$plugin" "$APPDIR/usr/lib/gstreamer-1.0/"
done
shopt -u nullglob
[ -n "$(ls -A "$APPDIR/usr/lib/gstreamer-1.0")" ] || {
  echo "no GStreamer plugins were bundled" >&2
  exit 1
}

# --- Shared-library closure ------------------------------------------------
# Copy the ldd closure of the binary and every bundled plugin (plugin-only
# deps such as libavcodec are only reachable through the plugins, so a
# plain binary ldd is not enough). The dynamic loader itself is excluded:
# it stays the host's.
copy_closure() {
  local obj lib dest added=1 pass
  for pass in $(seq 1 8); do
    [ "$added" = 1 ] || break
    added=0
    for obj in "$APPDIR/usr/bin/sonora" \
               "$APPDIR"/usr/lib/gstreamer-1.0/*.so \
               "$APPDIR"/usr/lib/*.so*; do
      while IFS= read -r lib; do
        case "$(basename "$lib")" in
          ld-linux-*) continue ;; # host loader stays host
        esac
        dest="$APPDIR/usr/lib/$(basename "$lib")"
        if [ ! -e "$dest" ]; then
          cp -L "$lib" "$dest"
          chmod u+w "$dest"
          added=1
        elif ! cmp -s "$lib" "$dest"; then
          echo "warn: basename collision, keeping first: $(basename "$lib")"
        fi
      # Every line is either "lib => /path (0x)" or "\t/path (0x)" (the
      # loader); print each /-token (the loader gets filtered below).
      # Note: no `exit` here — that would end awk after the first match.
      done < <(ldd "$obj" 2>/dev/null |
        awk '{for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i}')
    done
  done
}
copy_closure

# --- Rpaths ---------------------------------------------------------------
# Everything now lives under usr/, so make each bundled object resolve its
# siblings without LD_LIBRARY_PATH tricks (AppRun still exports it as a
# belt for dlopened plugins).
shopt -s nullglob
for f in "$APPDIR"/usr/lib/gstreamer-1.0/*.so; do
  patchelf --force-rpath --set-rpath '$ORIGIN/..' "$f"
done
for f in "$APPDIR"/usr/lib/*.so*; do
  patchelf --force-rpath --set-rpath '$ORIGIN' "$f"
done
patchelf --force-rpath --set-rpath '$ORIGIN/../lib' "$APPDIR/usr/bin/sonora"
shopt -u nullglob

# --- AppRun ---------------------------------------------------------------
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
# Sonora AppImage launcher: GStreamer lives inside the image, not on the
# host. gst_bundle.rs picks GST_PLUGIN_SYSTEM_PATH_1_0 up from the env
# when it is already set, so exporting here is enough.
HERE="$(dirname "$(readlink -f "$0")")"
export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export GST_PLUGIN_SYSTEM_PATH_1_0="$HERE/usr/lib/gstreamer-1.0"
export XDG_DATA_DIRS="$HERE/usr/share${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
exec "$HERE/usr/bin/sonora" "$@"
EOF
chmod +x "$APPDIR/AppRun"

# --- appimagetool ---------------------------------------------------------
TOOL="${APPIMAGETOOL:-}"
if [ -z "$TOOL" ]; then
  TOOL="$ROOT/target/appimagetool-x86_64.AppImage"
  if [ ! -x "$TOOL" ]; then
    curl -fsSL -o "$TOOL" \
      https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$TOOL"
  fi
fi
# --appimage-extract-and-run: CI runners have no FUSE.
ARCH=x86_64 VERSION="$VER" "$TOOL" --appimage-extract-and-run "$APPDIR" "$OUT"

ls -la "$OUT"
