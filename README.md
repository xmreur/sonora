# Sonora

Tauri + custom web UI + native in-process Apple Music playback. No `music.apple.com` UI is ever loaded.

Sonora runs on **Linux**, **Windows**, and **macOS**. Full-track playback is **native**: the app calls Apple's `webPlayback` API, licenses via a Widevine CDM (downloaded once at runtime, or borrowed from an installed browser), decrypts CENC in-process, and plays through the local audio output. No browser engine needed.

## Layout

- `crates/core/` — pure Rust: tokens, `ApiClient`, auth URL, queue models. Builds/tested anywhere.
- `crates/am-playback/` — native playback: bearer scrape, `webPlayback` resolve, Widevine CDM (locate/download/shim), CENC decrypt, cache, `rodio`+`symphonia` audio engine.
- `src-tauri/` — Tauri shell (IPC only). Linux builds need WebKitGTK dev libs; Windows/macOS need the usual Tauri prerequisites.
- `ui/` — `index.html` (custom UI) + `app.js` + `styles.css`.
- `packaging/` — Arch PKGBUILD + Flatpak manifest (Linux).
- `docs/` — token setup + architecture.

## Quick start (core tests, no system deps)

```bash
cargo test
APPLE_MUSIC_DEVELOPER_TOKEN=xxx cargo test -- --nocapture
```

## Tauri dev

### Linux

```bash
# Arch:
sudo pacman -S webkit2gtk-4.1 gtk3 libappindicator-gtk3 alsa-lib
cargo install tauri-cli --locked
cargo tauri dev
```

### Windows

- Install [Rust](https://rustup.rs/), [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/) (usually already present on Windows 11), and Visual Studio Build Tools with the C++ workload.
- `cargo install tauri-cli --locked` then `cargo tauri dev`.

### macOS

- Xcode command-line tools (`xcode-select --install`).
- `cargo install tauri-cli --locked` then `cargo tauri dev`.

Set token via env `APPLE_MUSIC_DEVELOPER_TOKEN` or config file (see `docs/TOKEN_SETUP.md`).
Config and cache live in the OS app config directory (e.g. `~/.config/sonora` on Linux, `%APPDATA%\sonora` on Windows, `~/Library/Application Support/sonora` on macOS).

## CI

Pull requests and pushes to `main` run GitHub Actions ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)):

- Linux: `cargo fmt`, `clippy`, `test`, `node tools/uitest.js`
- Windows and macOS: `fmt`, `clippy`, and core tests (compile-check the Tauri shell on each OS)

Owners can also comment `build:test` on a PR ([`.github/workflows/build-pr.yml`](.github/workflows/build-pr.yml)) to produce Linux `.deb`/`.AppImage` artifacts. Requires a `PR_BOT_TOKEN` secret (repo scope) and a `CODEOWNERS` entry.

## Releases

Push a version tag that matches [`src-tauri/tauri.conf.json`](src-tauri/tauri.conf.json) (e.g. `v0.1.0`) to build **Linux**, **Windows**, and **macOS** bundles and publish a [GitHub Release](.github/workflows/release.yml). You can also trigger a draft release manually via **Actions → Release → Run workflow**.

Install from the release assets:

- **Linux `.deb`** — `sudo dpkg -i sonora_*.deb`
- **Linux `.AppImage`** — `chmod +x Sonora_*.AppImage && ./Sonora_*.AppImage`
- **Windows** — `.msi` or setup `.exe` from the NSIS bundle
- **macOS** — `.dmg` or `.app` from the release

Installers are unsigned; you may need to allow the app in your OS security settings.

## Packaging (Linux)

- Arch: `makepkg -si` in `packaging/` (uses PKGBUILD).
- Flatpak: `flatpak-builder --user --install build packaging/com.example.applemusiclinux.yml`.
