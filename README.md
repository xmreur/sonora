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

## CI

Pull requests and pushes to `main` run GitHub Actions ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)):

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `node tools/uitest.js`

Owners can also comment `build:test` on a PR ([`.github/workflows/build-pr.yml`](.github/workflows/build-pr.yml)) to produce `.deb`/`.AppImage` artifacts via Actions → Artifacts and get a results comment. Requires a `PR_BOT_TOKEN` secret (repo scope) and a `CODEOWNERS` entry.

## Releases

Push a version tag that matches [`src-tauri/tauri.conf.json`](src-tauri/tauri.conf.json) (e.g. `v0.1.0`) to build Linux `.deb` and AppImage artifacts and publish a [GitHub Release](.github/workflows/release.yml). You can also trigger a draft release manually via **Actions → Release → Run workflow**.

Install from the release assets:

- **`.deb`** — `sudo dpkg -i sonora_*.deb` (or your package manager)
- **`.AppImage`** — `chmod +x Sonora_*.AppImage && ./Sonora_*.AppImage`

## Packaging

- Arch: `makepkg -si` in `packaging/` (uses PKGBUILD).
- Flatpak: `flatpak-builder --user --install build packaging/com.example.applemusiclinux.yml`.
