# Architecture

- **UI** (`crates/sonora`, egui/eframe): one native window, one process — no
  webview and no WebKitGTK helper processes. Browse/search/detail, playlists,
  queue, settings, karaoke lyrics, classic + immersive fullscreen. The frame
  loop draws every frame; a 250 ms tick drives all backend I/O.
  - `app.rs` — the `SonoraApp` shell: layout, player bar, settings, auth,
    boot + session restore.
  - `player.rs` — the sync engine: UI-side queue mirror, the smooth position
    clock (`PosClock`), report handling (`on_report`), track-end advance,
    radio fill, dead-id skipping, OS media-key jumps, load-confirm window.
  - `views.rs` / `widgets.rs` — browse, search, detail, playlist, queue and
    lyrics views plus the widget kit.
  - `immersive.rs` / `kawarp.rs` / `motion.rs` — fullscreen overlays, the
    GLSL warp background (egui_glow paint callback) and the animated covers
    (GStreamer `playbin3 → appsink`, one pipeline per surface).
  - `lyrics.rs` — LRCLIB fetch + the synced karaoke engine; `artwork.rs` —
    cover decode/resize into egui textures.
- **Service layer** (`crates/shell`): the command layer the UI calls
  directly (catalog, search, playlists, auth, `player_*`), shared app state
  (tokens, MUT, storefront, dead ids), the one-shot sign-in server, Discord
  presence, notifications, and the media-session bridge. Spawns no UI of its
  own: it publishes `PlayerReport`s into a shared hub the UI polls.
- **Native player** (`crates/am-playback`, default): bearer scrape →
  `webPlayback` resolve → Widevine license (CDM downloaded via Mozilla GMP or
  borrowed from a browser, `$SONORA_WIDEVINE_CDM` overrides) → CENC decrypt →
  `rodio`/`symphonia` local audio. Single-track engine; queue/Next/Previous
  live in `shell::native_player`, which publishes `PlayerReport`s. Album and
  playlist queue items expand via the catalog API; library (`i.*`) ids
  resolve to catalog ids first.
- **Rust core** (`crates/core`): `TokenProvider` (env/file), `ApiClient`
  (reqwest → api.music.apple.com), `auth` (woa URL + MUT extraction), queue
  models.

**Who decides what plays next:** the backend never auto-advances. The UI owns
the queue mirror and every jump — track-end detection, loop-one, dead-id
skipping, radio fill and OS media-key skips all happen in
`crates/sonora/src/player.rs`, driven by the reports it polls.

| Platform | Backend | Notes |
|----------|---------|--------|
| Linux | MPRIS (`org.mpris.MediaPlayer2.sonora`) via `mpris-server` | Session bus optional; missing bus disables media keys only |
| Windows | System Media Transport Controls via **souvlaki** | Needs main window HWND |
| macOS | Now Playing / remote commands via **souvlaki** | `MPNowPlayingInfoCenter` |

OS Next/Previous bump counters the UI consumes as queue jumps (same on every
platform). Opt-in track-change desktop notifications
(`Settings → Notifications`) use `notify-rust` on all platforms.

## On-disk state (`~/.config/sonora` on Linux)

| File | Owner | Contents |
|------|-------|----------|
| `ui-settings.json` | `crates/sonora/src/store.rs` | UI settings (volume, loop, background mode, integrations, …) |
| `ui-session.json` | `crates/sonora/src/store.rs` | Queue snapshot (capped at 300 entries) + index/position for restore |
| `motion-urls.json` | `crates/sonora/src/motion.rs` | Animated-cover manifest cache (30-day TTL) |
| `music_user_token`, `web_token_cache`, dead-id store | `crates/shell` | Auth + skipped-track memory |
| `widevine/`, thumb cache, PCM cache | `crates/am-playback` | CDM, artwork, decrypted audio |

Older releases kept settings and the queue in the webview's `localStorage`;
those do not migrate — the first start on the native UI begins fresh.
