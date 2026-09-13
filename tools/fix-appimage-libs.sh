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

# Build find name predicates into the array named by $1 (nameref).
# Result: (-name a -o -name b ...)
build_name_args() {
  local -n _out=$1
  _out=()
  local p
  for p in "${PATTERNS[@]}"; do
    if [ "${#_out[@]}" -eq 0 ]; then
      _out=(-name "$p")
    else
      _out+=(-o -name "$p")
    fi
  done
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

  # Resolve to an absolute path so extraction works regardless of cwd.
  local appimage_abs
  case "$appimage" in
    /*) appimage_abs="$appimage" ;;
    *) appimage_abs="$(pwd)/$appimage" ;;
  esac

  local workdir
  workdir="$(mktemp -d)"

  local root="$workdir/squashfs-root"
  # The runtime always extracts to ./squashfs-root in the cwd, so run it
  # from inside the temp dir.
  (cd "$workdir" && APPIMAGE_EXTRACT_AND_RUN=1 "$appimage_abs" --appimage-extract >/dev/null)
  [ -d "$root" ] || { echo "extract failed for $appimage" >&2; rm -rf "$workdir"; return 1; }

  local name_args=()
  build_name_args name_args

  echo "Bundled matches before strip:"
  find "$root" \( -type f -o -type l \) \( "${name_args[@]}" \) -print \
    | sed "s|^$root/||" || true

  find "$root" \( -type f -o -type l \) \( "${name_args[@]}" \) -delete

  local leftovers
  leftovers="$(find "$root" \( -type f -o -type l \) \( "${name_args[@]}" \) -print || true)"
  if [ -n "$leftovers" ]; then
    echo "ERROR: some excluded libs remain:" >&2
    echo "$leftovers" >&2
    rm -rf "$workdir"
    return 1
  fi
  echo "Strip OK: no excluded libs remain."

  local tool
  tool="$(ensure_appimagetool)"
  local out="$workdir/fixed.AppImage"
  APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$tool" "$root" "$out" >/dev/null
  chmod +x "$out"

  mv "$out" "$appimage_abs"
  rm -rf "$workdir"
  echo "Repacked: $appimage"
}

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
