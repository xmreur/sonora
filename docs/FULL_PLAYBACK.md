# Full-track playback (native, default)

Previews are DRM-free; **full tracks are Widevine-encrypted**. The native
engine decrypts them in-process — no browser engine involved:

- `crates/am-playback` scrapes the shared web-player bearer token from
  `music.apple.com`, calls `play.music.apple.com/.../webPlayback` with it
  plus your MUT, and picks the `28:ctrp256` fMP4 asset.
- The KID is licensed through a system Widevine CDM: first `$SONORA_WIDEVINE_CDM`,
  then the runtime download (`~/.config/sonora/widevine/<version>/` via Mozilla's
  GMP service, sha512-verified), then a browser copy (Firefox GMP or Chromium
  `WidevineCdm`). The content key never leaves the CDM.
- Samples decrypt per `senc` IV/subsample tables, output is relabelled
  `enca → mp4a`, decrypted bytes cache under the Sonora cache dir, and
  `symphonia` decodes to the `rodio` output. The `tenc` box is read
  version-aware for the real IV size (constant-IV tracks supported); `senc`
  parsing is strict — a wrong IV size fails loudly instead of producing
  noise. A failed play stops the engine so no stale song keeps running
  under the new UI state.
- First play downloads the CDM (~20MB) if missing; `sidecar_warmup` (app boot)
  prefetches bearer + CDM in the background. While a track plays, the next
  two queued tracks prefetch (decrypt → decode) in the background, and
  decoded PCM stays in a small memory LRU — replay/Next/Previous skip
  network, license, and decode. The dev console logs per-phase timings
  (`webplayback` / `download+cdm` / `license` / `decrypt`) plus
  `cached`/`fresh` decode notes.

Set `SONORA_PLAYER=firefox` to use the legacy Firefox sidecar instead
(hidden minimal MusicKit page; needs Firefox with **Play DRM-controlled
content** enabled). All `sidecar_*` IPC names and the status/media-session
bridges behave the same either way.

# Full-track playback (Firefox sidecar, legacy)

Previews are DRM-free; **full tracks are Widevine-encrypted** and only a real
browser engine with the CDM can decrypt them. The app therefore drives a
hidden-from-Apple-UI Firefox window:

- Rust serves `ui/player.html` (audio-only MusicKit page, embedded in the
  binary) on `http://127.0.0.1:<port>/`, plus `/config` (dev token +
  MUT), `/cmd` (command queue, player polls), `/state` (player reports),
  and `/apiproxy/*` — a same-origin forwarder to `amp-api.music.apple.com`
  that adds the `Origin: https://music.apple.com` header Apple requires for
  web-player tokens (a localhost page can't send that Origin itself; the
  player's `fetch` wrapper rewrites `api.music.apple.com` calls to it).
- On first Play it launches Firefox with a dedicated profile under the Sonora
  config directory, headless by default (`MOZ_HEADLESS=1`; toggle in the
  sidebar if your build stays silent — some builds need a real window for
  the CDM). Changing the toggle applies on sidecar relaunch (sidebar button
  or stop + Play).
- The Apple approval popup appears **in that window once**; the dedicated
  profile remembers it afterwards.

### Profile location

| OS      | Config root (Firefox profile is `<root>/firefox-profile`) |
|---------|-------------------------------------------------------------|
| Linux   | `~/.config/sonora` (migrated from `~/.config/apple-music-linux` once) |
| Windows | `%APPDATA%\sonora` |
| macOS   | `~/Library/Application Support/sonora` |

## Requirements

Install **Firefox** on your system. Sonora looks for `firefox` on `PATH`, then
common install locations (e.g. `Program Files\Mozilla Firefox\firefox.exe` on
Windows, `/Applications/Firefox.app` on macOS).

In Firefox: Settings → General → *Digital Rights Management (DRM) Content* →
check **Play DRM-controlled content**. Widevine downloads itself on first
playback. Verify at `about:addons` → Plugins → Widevine Content Decryption
Module.

## Use

- Track **Play** = full track via sidecar.
- Transport buttons drive the sidecar; Now Playing refreshes from
  `sidecar_status`. `sidecar_stop` kills the Firefox window.
