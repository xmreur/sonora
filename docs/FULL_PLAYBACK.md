# Full-track playback (Firefox sidecar)

Previews are DRM-free; **full tracks are Widevine-encrypted** and only a real
browser engine with the CDM can decrypt them. The app therefore drives a
hidden-from-Apple-UI Firefox window:

- Rust serves `ui/player.html` (audio-only MusicKit page, embedded in the
  binary) on `http://127.0.0.1:<random-port>/`, plus `/config` (dev token +
  MUT), `/cmd` (command queue, player polls), `/state` (player reports),
  and `/apiproxy/*` — a same-origin forwarder to `amp-api.music.apple.com`
  that adds the `Origin: https://music.apple.com` header Apple requires for
  web-player tokens (a localhost page can't send that Origin itself; the
  player's `fetch` wrapper rewrites `api.music.apple.com` calls to it).
- On first Play it launches
  `firefox --no-remote --profile ~/.config/sonora/firefox-profile --new-window <url>`,
  headless by default (`MOZ_HEADLESS=1`; toggle in the sidebar if your build
  stays silent — some builds need a real window for the CDM). Changing the
  toggle applies on sidecar relaunch (sidebar button or stop + Play).
- The Apple approval popup appears **in that window once**; the dedicated
  profile remembers it afterwards. Keep the window open (minimize it).

## Requirements

```bash
sudo pacman -S firefox
```

In Firefox: Settings → General → *Digital Rights Management (DRM) Content* →
check **Play DRM-controlled content**. Widevine downloads itself on first
playback. Verify at `about:addons` → Plugins → Widevine Content Decryption
Module.

## Use

- Track **Play** = full track via sidecar. **Shift+click** = 30s preview via
  `ffplay` (no browser needed).
- Transport buttons drive the sidecar; Now Playing refreshes every 3s from
  `sidecar_status`. `sidecar_stop` kills the Firefox window.
