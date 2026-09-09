# Architecture

- UI (Tauri webview, `ui/index.html`): custom minimal DOM. Search box, results, queue, settings. No Apple CSS/JS.
- Hidden player (`ui/player.html`): ~40 lines + `musickit.js` v3. `MusicKit.configure({developerToken})`, `setQueue/play/pause/skip`. Audio only, EME/Widevine handled by engine.
- Rust core (`crates/core`): `TokenProvider` (env/file), `ApiClient` (reqwest → api.music.apple.com), `auth` (woa URL + MUT extraction), `playback` (`EngineKind::Gecko|Chromium|WebKit`, `PlaybackCommand`, `SidecarConfig::check_supported`, `PlaybackEngine` trait).
- Tauri shell (`src-tauri`): exposes `search_catalog`, `authorize_url`, `submit_user_token`, `set_engine`, `playback_command`.
- Engines: default Gecko sidecar (`firefox --kiosk player.html`, controlled via WebSocket — Phase 3). Chromium fallback same page with `--app`. WebKit rejected for playback (no Widevine) but fine for metadata browsing.
- MPRIS: `mpris-server` integration planned (Phase 3) for `playerctl`, media keys, GNOME/KDE applets.
