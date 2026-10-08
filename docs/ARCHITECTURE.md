# Architecture

- UI (Tauri webview, `ui/index.html`): custom minimal DOM. Search box, results, queue, settings. No Apple CSS/JS.
- Native player (`crates/am-playback`, default): bearer scrape → `webPlayback` resolve → Widevine license (CDM downloaded via Mozilla GMP or borrowed from a browser, `$SONORA_WIDEVINE_CDM` overrides) → CENC decrypt → `rodio`/`symphonia` local audio. Single-track engine; queue/Next/Previous live in `src-tauri/src/native_player.rs`, which publishes `PlayerReport`s into the shared status hub. Album/playlist queue items expand via the catalog API; library (`i.*`) ids resolve to catalog ids first.
- Hidden player (`ui/player.html`, legacy `SONORA_PLAYER=firefox` only): ~40 lines + `musickit.js` v3. `MusicKit.configure({developerToken})`, `setQueue/play/pause/skip`. Audio only, EME/Widevine handled by the Firefox sidecar (not the Tauri webview).
- Rust core (`crates/core`): `TokenProvider` (env/file), `ApiClient` (reqwest → api.music.apple.com), `auth` (woa URL + MUT extraction), `playback` (`EngineKind::Gecko|Chromium|WebKit`, `PlaybackCommand`, `SidecarConfig::check_supported`, `PlaybackEngine` trait).
- Tauri shell (`src-tauri`): exposes catalog/search, auth, and sidecar IPC commands.
- Engines: default **Gecko sidecar** (Firefox + dedicated profile). Chromium/WebKit labels exist for compatibility; embedded WebKit/WebView2/WKWebView cannot play Widevine DRM.
- **Media session** (`src-tauri/src/media_session/`): publishes transport metadata and accepts OS media keys. Fed by `PlayerReport` (`POST /state` → `SidecarManager::status()`). OS Next/Previous bump counters the UI consumes as queue jumps (same on every platform).

| Platform | Backend | Notes |
|----------|---------|--------|
| Linux | MPRIS (`org.mpris.MediaPlayer2.sonora`) via `mpris-server` | Session bus optional; missing bus disables media keys only |
| Windows | System Media Transport Controls via **souvlaki** | Needs main window HWND |
| macOS | Now Playing / remote commands via **souvlaki** | `MPNowPlayingInfoCenter` |

Opt-in track-change desktop notifications (`Settings → Notifications`) use `notify-rust` on all platforms.
