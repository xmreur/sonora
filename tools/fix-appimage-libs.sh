#!/usr/bin/env bash
# Strip host-GPU/Wayland libs from Tauri AppImages.
#
# Why: Tauri's linuxdeploy bundles its own older libwayland* while EGL/Mesa
# (libEGL, libGL, libGLX, libGLESv2, libgbm, libdrm) resolve from the host.
# On Mesa 25+ / Wayland sessions that mix aborts with:
#   Could not create surfaceless egl display: EGL_BAD_ALLOC. Aborting...
#   Could not create default EGL display: EGL_BAD_PARAMETER. Aborting...
# Fix: remove the bundled copies so the AppImage falls back to the host
# stack (same approach as the pkg2appimage community excludelist).
#
# Usage:
#   ./tools/fix-appimage-libs.sh [AppImage ...]
#   (no args = all of target/release/bundle/appimage/*.AppImage)
set -euo pipefail

PATTERNS=(
  "libwayland-client.so*"
  "libwayland-cursor.so*"
  "libwayland-egl.so*"
  "libwayland-server.so*"
  "libEGL.so*"
  "libGL.so*"
  "libGLX.so*"
  "libGLESv2.so*"
  "libgbm.so*"
  "libdrm.so*"
)

APPIMAGETOOL_URL="${APPIMAGETOOL_URL:-https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage}"

find_args() {
  # Build: \( -name a -o -name b ... \) for find.
  local out="( "
  local first=1
  for p in "${PATTERNS[@]}"; do
    if [ "$first" -eq 1 ]; then
      out+="-name \"$p\""
      first=0
    else
      out+=" -o -name \"$p\""
    fi
  done
  out+=" )"
  printf '%s' "$out"
}

ensure_appimagetool() {
  if command -v appimagetool >/dev/null 2>&1; then
    echo "appimagetool"
    return
  fi
  local cached="/tmp/appimagetool-x86_64.AppImage"
  if [ ! -x "$cached" ]; then
    echo "Downloading appimagetool..." >&2
    curl -fL -o "$cached" "$APPIMAGETOOL_URL"
    chmod +x "$cached"
  fi
  echo "$cached"
}

fix_one() {
  local appimage="$1"
  echo "=== Fixing $appimage ==="
  [ -f "$appimage" ] || { echo "missing: $appimage" >&2; return 1; }
  chmod +x "$appimage"

  local workdir
  workdir="$(mktemp -d)"
  # Expand trap immediately (double quotes) so $workdir survives function exit.
  trap "rm -rf \"$workdir\"" RETURN

  pushd "$workdir" >/dev/null
  APPIMAGE_EXTRACT_AND_RUN=1 "$OLDPWD/$appimage" --appimage-extract >/dev/null
  local root="$workdir/squashfs-root"
  [ -d "$root" ] || { echo "extract failed for $appimage" >&2; return 1; }

  echo "Bundled matches before strip:"
  # shellcheck disable=SC2086
  eval "find \"$root\" \\( -type f -o -type l \\) $(find_args) -print" | sed "s|^$root/||" || true

  # shellcheck disable=SC2086
  eval "find \"$root\" \\( -type f -o -type l \\) $(find_args) -delete"

  local leftovers
  # shellcheck disable=SC2086
  leftovers="$(eval "find \"$root\" \\( -type f -o -type l \\) $(find_args) -print" || true)"
  if [ -n "$leftovers" ]; then
    echo "ERROR: some excluded libs remain:" >&2
    echo "$leftovers" >&2
    return 1
  fi
  echo "Strip OK: no excluded libs remain."

  local tool
  tool="$(ensure_appimagetool)"
  local out="$workdir/fixed.AppImage"
  APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$tool" "$root" "$out" >/dev/null
  chmod +x "$out"
  popd >/dev/null

  mv "$workdir/fixed.AppImage" "$appimage"
  echo "Repacked: $appimage"
}

OLDPWD="$(pwd)"
if [ "$#" -gt 0 ]; then
  for a in "$@"; do fix_one "$a"; done
else
  shopt -s nullglob
  imgs=(target/release/bundle/appimage/*.AppImage)
  if [ "${#imgs[@]}" -eq 0 ]; then
    echo "No AppImages found in target/release/bundle/appimage/" >&2
    exit 1
  fi
  for a in "${imgs[@]}"; do fix_one "$a"; done
fi
