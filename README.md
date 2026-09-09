# Sonora

Tauri + minimal web UI + hidden minimal MusicKit player. No `music.apple.com` UI is ever loaded (that is what froze Chromium tabs).

## Layout

- `crates/core/` — pure Rust: tokens, `ApiClient`, auth URL, `PlaybackEngine` trait, models. Builds/tested anywhere.
- `src-tauri/` — Tauri shell (IPC only). Needs WebKitGTK dev libs to build.
- `ui/` — `index.html` (custom UI) + `player.html` (hidden audio-only musickit.js page for Gecko/Chromium sidecar).
- `packaging/` — Arch PKGBUILD + Flatpak manifest.
- `docs/` — token setup + architecture.

## Quick start (core tests, no system deps)

```bash
cargo test
APPLE_MUSIC_DEVELOPER_TOKEN=xxx cargo test -- --nocapture
```

## Tauri dev (needs WebKitGTK + Node)

```bash
# Arch:
sudo pacman -S webkit2gtk-4.1 gtk3 libappindicator-gtk3
cargo install tauri-cli --locked
cargo tauri dev
```

Set token via env `APPLE_MUSIC_DEVELOPER_TOKEN` or config file (see `docs/TOKEN_SETUP.md`).
Default playback engine is `gecko` (Firefox + Widevine). Chromium is fallback; WebKit is metadata-only.

## Packaging

- Arch: `makepkg -si` in `packaging/` (uses PKGBUILD).
- Flatpak: `flatpak-builder --user --install build packaging/com.example.applemusiclinux.yml`.
