# Full-track playback

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
- First play downloads the CDM (~20MB) if missing; `player_warmup` (app boot)
  prefetches bearer + CDM in the background. While a track plays, the next
  two queued tracks prefetch (decrypt → decode) in the background, and
  decoded PCM is cached as files, not RAM: `<cache>/sonora/pcm/<adam>.pcm`
  (validated header + raw s16; 16-bit is transparent for playback and halves
  both RAM and disk vs f32). Each decoded chunklet streams to a
  per-generation `.part` file as it arrives and is atomically renamed on
  completion; orphans are swept at startup. Replay/Next read the file back
  (~50–150ms), still skipping network, license, decrypt, and decode, while
  the playing track alone retains its chunklets in memory for gapless
  playback and seek. The next two queued tracks prefetch (decrypt → decode
  straight to file, no full-size transient); the file cap is 8 entries by
  oldest-mtime. The decrypt pipeline decodes incrementally (each packet
  once) and applies backpressure past 4 queued mixer sources. The dev
  console logs per-phase timings (`webplayback` / `download+cdm` /
  `license` / `decrypt`) plus pcm-file store/evict byte counts and a
  per-track retention snapshot (file entries, live chunks, mixer backlog).

