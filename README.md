# Sonora

Native Apple Music client with an **egui/eframe** UI and **in-process full-track
playback**. No `music.apple.com` UI is ever loaded, and no webview either: the
whole app is one Rust process that draws its own interface.

Full-track playback is native — the app calls Apple's `webPlayback` API,
licenses via a Widevine CDM (downloaded once at runtime, or borrowed from an
installed browser), decrypts CENC in-process, and plays through the local audio
output. Previews are DRM-free and play straight from the stream.

Sonora builds on **Linux**, **Windows**, and **macOS** (Linux is the primary
target: MPRIS, Wayland/X11, AUR/Flatpak packaging).

## Layout

- `crates/core/` — pure Rust: tokens, `ApiClient`, auth URL, queue models. Builds/tested anywhere.
- `crates/am-playback/` — native playback: bearer scrape, `webPlayback` resolve, Widevine CDM (locate/download/shim), CENC decrypt, cache, `rodio`+`symphonia` audio engine.
- `crates/shell/` — service layer shared by the UI: app state, the command layer, the native player, the one-shot sign-in server, Discord/notifications, and the OS media-session bridge.
- `crates/sonora/` — the egui/eframe UI: browse/search/detail, playlists, queue + radio, synced karaoke lyrics, classic and immersive fullscreen with animated covers, settings, session restore.
- `packaging/` — Arch PKGBUILD, Flatpak manifest, `.desktop` file.
- `docs/` — token setup, architecture, playback internals.
- `tools/` — `mem.sh` (resident-memory report for a running instance).

## How to build

You need a stable Rust toolchain (`rustup default stable`) and the system
libraries the UI and the playback pipeline link against: ALSA (or the
platform audio stack), a GL/EGL loader, and GStreamer with the good, bad,
and libav plugin sets (animated covers).

Arch:

```bash
sudo pacman -S gst-plugins-good gst-plugins-bad gst-libav gstreamer \
  alsa-lib libxkbcommon wayland libx11 pkgconf
```

Debian/Ubuntu:

```bash
sudo apt install pkg-config libasound2-dev libxkbcommon-dev libxcb1-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libegl1-mesa-dev libgles2-mesa-dev libgl1-mesa-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav
```

macOS (Homebrew): `brew install gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav pkg-config`.

Windows: install the GStreamer MSVC runtime and development packages and put `bin` and `lib\pkgconfig` on `PATH` / `PKG_CONFIG_PATH`. The CI job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml) is the exact recipe.

Then, from the repo root:

```bash
cargo build -p sonora --release
```

The binary is `target/release/sonora` (`sonora.exe` on Windows). A debug build is `cargo build -p sonora`. Run the test suite with `cargo test --workspace`.

Set the token via env `APPLE_MUSIC_DEVELOPER_TOKEN` or the config file (see [`docs/TOKEN_SETUP.md`](docs/TOKEN_SETUP.md)) before the first play.

## Quick start (core tests, no system deps)

```bash
cargo test
APPLE_MUSIC_DEVELOPER_TOKEN=xxx cargo test -- --nocapture
```

## Run the app

Install the libraries in [How to build](#how-to-build), then:

```bash
cargo run -p sonora --release
```

Settings and the play-session snapshot live in `~/.config/sonora/` on Linux
(`%APPDATA%\com.xmreur.sonora` on Windows,
`~/Library/Application Support/com.xmreur.sonora` on macOS).

`tools/mem.sh` prints the resident memory of a running instance (plus any
helper processes), which is how the migration off the webview was measured.

## Dev loop

```bash
cargo run -p sonora            # debug build, logs to stderr with RUST_LOG=debug
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## CI

Pull requests and pushes to `main` run GitHub Actions
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)):

- **Linux**: `cargo fmt`, `clippy --workspace --all-targets -D warnings`, `cargo build -p sonora`, `cargo test --workspace`
- **Windows / macOS**: `cargo fmt`, `cargo build --workspace`, `cargo test --workspace` (GStreamer installed on the runner first)

Owners can also comment `build:test` on a PR
([`.github/workflows/build-pr.yml`](.github/workflows/build-pr.yml)) to get a
Linux release-binary tarball artifact. Requires a `PR_BOT_TOKEN` secret
(repo scope) and a `CODEOWNERS` entry.

## Releases

Push a version tag that matches [`crates/sonora/Cargo.toml`](crates/sonora/Cargo.toml)
(e.g. `v0.1.8`) to build the release bundles on **Linux**, **Windows**, and
**macOS** and publish a [GitHub Release](.github/workflows/release.yml). You can
also trigger a draft release manually via **Actions → Release → Run workflow**.

Install from the release assets:

- **Linux**
  - `Sonora-*-x86_64.AppImage` — GStreamer bundled, audio via the host's ALSA (alsa-lib): `chmod +x` and run. Works on any x86_64 desktop distro with glibc ≥ 2.35 (Ubuntu 22.04+, Debian 12+, Fedora 36+ …).
  - `Sonora-*-x86_64-linux.deb` — Debian/Ubuntu; depends on the distro GStreamer packages instead of bundling them.
  - `sonora-*-x86_64-unknown-linux-gnu.tar.gz` — plain binary + `.desktop` + icon, for packagers.
- **Windows**
  - `Sonora-Setup-*.exe` — NSIS installer (Start Menu + desktop shortcut, uninstaller). Bundles the full GStreamer runtime, no manual install needed.
  - `Sonora-*-windows-x86_64-portable.zip` — the same payload (exe + GStreamer runtime + plugins), no installer.
- **macOS** (bundles built by CI, not yet verified on real hardware)
  - `Sonora-*-aarch64-apple-darwin.dmg` — drag `Sonora.app` to Applications. GStreamer dylibs are bundled inside the app.
  - `Sonora-*-aarch64-apple-darwin-app.zip` — the same `.app`, zipped.
  - `sonora-*-aarch64-apple-darwin.tar.gz` — plain binary (needs the Homebrew GStreamer from [How to build](#how-to-build)).

Installers are unsigned; you may need to allow the app in your OS security settings
(macOS: right-click → Open, or `xattr -dr com.apple.quarantine Sonora.app`).

## Packaging (Linux)

- **Arch**: `makepkg -si` in `packaging/` (PKGBUILD builds `-p sonora` and installs the binary, `.desktop`, and icons).
- **Flatpak**: regenerate the vendored crate list whenever `Cargo.lock` changes, then build:

  ```bash
  flatpak-cargo-generator.py Cargo.lock -o packaging/cargo-sources.json
  flatpak-builder --user --install build packaging/com.xmreur.sonora.yml
  ```

The release bundles are built by the scripts in `packaging/`, all runnable
locally after `cargo build --release -p sonora`:

- **deb**: `cargo deb -p sonora --no-build` (payload declared in `[package.metadata.deb]`)
- **AppImage**: `packaging/appimage/build-appimage.sh <version>` (bundles the GStreamer runtime + plugins)
- **macOS .app/DMG**: `packaging/macos/build-app.sh <version> target/release/sonora`
- **Windows installer**: `packaging/windows/sonora.nsi`, compiled by `makensis` against a staged payload dir (see the Release workflow)
