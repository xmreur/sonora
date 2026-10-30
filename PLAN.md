# PLAN — Auto-update system (`feat/auto-update`)

Progress tracker: tick boxes as items land so work can be resumed later.

## Behavior matrix

| Install method | Detection | What happens on "Update" |
|---|---|---|
| AppImage | `$APPIMAGE` env | Download → sha256 verify → atomic rename → restart |
| Plain Linux/macOS binary | exe not under `/usr`, not AppImage/Flatpak/.app | Download tar.gz → extract → stage → rename → restart |
| deb / AUR | exe under `/usr` | Notify only + "Open release page" |
| Flatpak | `/.flatpak-info` exists | Notify only + "Open release page" |
| Windows NSIS | `HKLM\Software\Sonora\InstallDir` == exe dir | Download `Sonora-Setup-<v>.exe` → cmd wrapper waits for exit → `/S` → relaunch |
| Windows portable | otherwise on Windows | Download portable zip → extract → cmd wrapper copies after exit → relaunch |
| macOS .app | exe inside `Sonora.app` | Download app.zip → `ditto -x -k` → replace if writable → restart, else notify only |
| Unknown / read-only | — | Notify only + "Open release page" |

## Tasks

### shell crate (core updater)

- [x] `crates/shell/src/updater.rs`: `check_for_update()` — GitHub `/releases/latest` via `LazyLock<reqwest::Client>`, serde `UpdateInfo { tag, notes, assets }`
- [x] `updater.rs`: hand-rolled `parse_version` / `cmp` (tags strictly `vX.Y.Z`, no new dep)
- [x] `updater.rs`: `detect_install() -> InstallKind` — pure fn of injected env/paths (matrix above), unit-testable
- [x] `updater.rs`: `download()` — streaming `Response::chunk()`, progress `Arc<AtomicU64>`, cancel `Arc<AtomicBool>`, sha256 verify, `.part` → rename
- [x] `updater.rs`: `install_and_restart()` — cfg-gated:
  - [x] Unix: rename over target, `sh -c 'sleep 1; exec …'` relaunch (no single-instance lock exists), `exit(0)`
  - [x] Windows: `.cmd` wrapper (tasklist wait → installer `/S` or robocopy → start → self-delete), `CREATE_NO_WINDOW`, exit
  - [x] macOS: `ditto -x -k` extraction, `.app` replacement
- [x] New deps in `crates/shell/Cargo.toml`: `sha2`, `tar`, `flate2` (unix), `zip` (windows), `winreg` (windows)
- [x] `crates/shell/src/commands.rs`: `pub async fn check_for_update() -> Result<Option<UpdateInfo>, String>`
- [x] Register module in `crates/shell/src/lib.rs`

### UI (crates/sonora)

- [x] `app.rs`: `Settings` fields `check_updates: bool` (default true), `skipped_version: Option<String>` (persisted free by `store.rs`)
- [x] `app.rs`: update state — `Pending<UpdateInfo>` slot, `last_check` timestamp, download progress/cancel Arcs, update banner state
- [x] Scheduling: first check a few seconds after launch (non-blocking `backend.call`), re-check every 24 h; silent on failure
- [x] `app.rs`: `render_update_banner` cloned from `render_auth_banner` (`app.rs:836`) — **[Update] [Release notes] [Not now] [Skip this version]**; package-managed installs → **[Open release page]** only
- [x] Confirm via existing `Dialog` enum when playback is active ("restarting stops playback")
- [x] Download progress in banner: `Downloading… 42%` + **[Cancel]**, then `Installing…` → restart
- [x] `views.rs`: Settings → new "Updates" card (`settings_card`): auto-check checkbox, current version, **Check now** + inline status

### Release pipeline

- [x] `.github/workflows/release.yml` `publish` job: generate + attach `SHA256SUMS` (`sha256sum` over staged assets)
- [x] Audit `packaging/windows/sonora.nsi` for silent-mode safety (expected: no/minimal edits — wrapper exits before installer runs)

### Docs & tests

- [x] README: short "Auto-updates" note under Releases
- [x] Unit tests (no network): version compare (v-prefix, equal, downgrade), `detect_install()` matrix, GitHub JSON fixture parse, `SHA256SUMS` parse
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo test --workspace`

## Out of scope

Delta/patch downloads, beta channels, auto-restart while playing without confirmation.

## Status

- [x] Plan agreed, branch `feat/auto-update` created
- [x] Implementation complete
- [ ] All CI checks green
