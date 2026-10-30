#!/usr/bin/env bash
# Build a self-contained Sonora AppImage.
#
# AppDir layout produced:
#   AppRun                      launcher (written here)
#   Sonora.desktop, sonora.png  files appimagetool requires at the root
#   usr/bin/sonora              stripped release binary
#   usr/lib/*.so*               ldd closure of the binary + the plugins,
#                               minus host-owned runtimes (exclude_lib)
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
# Libraries that must stay the host's. Groups:
#
#   1. The C runtime and compiler runtime — ld.so, glibc, libgcc_s,
#      libstdc++. The loader is the host's by definition (it loads the
#      AppImage's binary), so bundling the *runner's* libc and then
#      prepending usr/lib to LD_LIBRARY_PATH makes a newer-glibc host
#      (Arch, Fedora, …) load our older libc and segfault before main().
#      libstdc++ stays out too: the host's mesa DRI drivers are C++ and
#      must agree with the host's libstdc++.
#   2. The GL stack — mesa dlopens its DRI drivers, which are built
#      against the host's libdrm/mesa. A bundled 22.04 libGL driving
#      host radeonsi drivers is a second guaranteed crash. This
#      includes libGLdispatch: the host's libEGL (libglvnd) dlopens it
#      by soname and a bundled 1.4 next to a host 1.7 yields zero EGL
#      configs ("NoGlutinConfigs" from glutin).
#   3. The Wayland client stack — winit and glutin dlopen
#      libwayland-client.so.0 by soname and honour the rpath we set on
#      the binary, so a bundled copy (pulled in by gst's waylandsink)
#      shadows the host's and breaks windowing outright.
#   4. The ALSA client library — libasound hardcodes a distro-specific
#      plugin dir at build time (Ubuntu/Debian:
#      /usr/lib/x86_64-linux-gnu/alsa-lib, Arch: /usr/lib/alsa-lib,
#      Fedora/openSUSE: /usr/lib64/alsa-lib) and reads the host's
#      /usr/share/alsa + /etc/alsa config. Our bundled 22.04 copy looks
#      for plugins in the Debian multiarch path, so on any other layout
#      snd_pcm_open("default") fails (-ENXIO): cpal never opens an
#      output stream, playback is silent and no stream shows up in
#      pavucontrol (0.1.9). The host's libasound must drive the host's
#      plugins and routing (PipeWire/Pulse/dmix) — the same reasoning
#      as the GL stack above, and the same as the deb/tar.gz, which
#      link the system libasound.
#
# Everything else (glibc's own sonames are versioned and the ABI is
# backward-compatible) is safe to bundle.
exclude_lib() {
  case "$1" in
    ld-linux-*|libc.so.*|libm.so.*|libmvec.so.*|libpthread.so.*|libdl.so.*|\
    librt.so.*|libresolv.so.*|libutil.so.*|libnsl.so.*|libgcc_s.so.*|\
    libstdc++.so.*)
      return 0 ;;
    libGL.so.*|libOpenGL.so.*|libGLX*.so.*|libEGL.so.*|libglapi.so.*|\
    libGLdispatch.so.*|libdrm.so.*|libgbm.so.*)
      return 0 ;;
    libwayland-client.so.*|libwayland-cursor.so.*|libwayland-server.so.*|\
    libwayland-egl.so.*)
      return 0 ;;
    libasound.so.*)
      return 0 ;;
  esac
  return 1
}

copy_closure() {
  local obj lib dest added=1 pass
  for pass in $(seq 1 8); do
    [ "$added" = 1 ] || break
    added=0
    for obj in "$APPDIR/usr/bin/sonora" \
               "$APPDIR"/usr/lib/gstreamer-1.0/*.so \
               "$APPDIR"/usr/lib/*.so*; do
      while IFS= read -r lib; do
        exclude_lib "$(basename "$lib")" && continue
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

# Belt for the exclusion above: glutin and winit dlopen several of these
# by soname and honour the rpaths set below, so a bundled copy shadows
# the host's and breaks GL/windowing (0.1.9 shipped both failures);
# libasound's bundled copy likewise breaks audio on every non-Debian
# plugin layout (see exclude_lib). Fail the build if one ever slips
# back in.
for bad in libc.so libGLdispatch.so libEGL.so libGL.so \
           libwayland-client.so libwayland-cursor.so libwayland-server.so \
           libwayland-egl.so libasound.so; do
  if compgen -G "$APPDIR/usr/lib/$bad*" > /dev/null; then
    echo "excluded library was bundled: $bad*" >&2
    exit 1
  fi
done

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
