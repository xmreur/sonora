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

## Quick start (core tests, no system deps)

```bash
cargo test
APPLE_MUSIC_DEVELOPER_TOKEN=xxx cargo test -- --nocapture
```

## Run the app

Build dependencies are the C libraries the UI and the audio/GStreamer
pipelines link against.

Arch:

```bash
sudo pacman -S gst-plugins-good gst-plugins-bad gst-libav gstreamer \
  alsa-lib libxkbcommon wayland libx11 pkgconf
cargo run -p sonora --release
```

Debian/Ubuntu:

```bash
sudo apt install pkg-config libasound2-dev libxkbcommon-dev libxcb1-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libegl1-mesa-dev libgles2-mesa-dev libgl1-mesa-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav
cargo run -p sonora --release
```

Set the token via env `APPLE_MUSIC_DEVELOPER_TOKEN` or the config file (see
[`docs/TOKEN_SETUP.md`](docs/TOKEN_SETUP.md)). Settings and the play-session
snapshot live in `~/.config/sonora/` on Linux (`%APPDATA%\com.xmreur.sonora`
on Windows, `~/Library/Application Support/com.xmreur.sonora` on macOS).

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
(e.g. `v0.1.7`) to build a release binary on **Linux**, **Windows**, and
**macOS** and publish a [GitHub Release](.github/workflows/release.yml). You can
also trigger a draft release manually via **Actions → Release → Run workflow**.

Install from the release assets (each archive holds the `sonora` binary, a
`.desktop` file, and the icon):

- **Linux** — `tar -xzf sonora-*-x86_64-unknown-linux-gnu.tar.gz` then put `sonora` on your `PATH` (a distro package is better: it carries the GStreamer/ALSA dependencies).
- **Windows** — unzip `sonora-*-x86_64-pc-windows-msvc.zip` and run `sonora.exe` (needs the GStreamer runtime on the machine).
- **macOS** — `tar -xzf sonora-*-aarch64-apple-darwin.tar.gz` and run `./sonora`.

Installers are unsigned; you may need to allow the app in your OS security settings.

## Packaging (Linux)

- **Arch**: `makepkg -si` in `packaging/` (PKGBUILD builds `-p sonora` and installs the binary, `.desktop`, and icons).
- **Flatpak**: regenerate the vendored crate list whenever `Cargo.lock` changes, then build:

  ```bash
  flatpak-cargo-generator.py Cargo.lock -o packaging/cargo-sources.json
  flatpak-builder --user --install build packaging/com.xmreur.sonora.yml
  ```
