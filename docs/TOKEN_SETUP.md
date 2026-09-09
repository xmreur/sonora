# Token setup (subscription-only — no paid developer account needed)

You only need your normal **Apple Music subscription**. The app handles the
developer token automatically:

1. **Developer token (auto)** — on first authorize/search, the app scrapes
   Apple's own web-player token from the `index-legacy~*.js` bundle on
   `beta.music.apple.com` (same token for everyone,
   `core::token::fetch_web_player_token`), caches it at
   `~/.config/sonora/web_token_cache`, and reuses it. Catalog
   calls go to `amp-api.music.apple.com` with `Origin: https://music.apple.com`
   (required for web tokens; verified working).
   - Fragile by nature: Apple rotates it every few months / may change the
     bundle layout. If auto-fetch fails, the error tells you and you can
     supply your own token (see below).
   - `token_status` IPC reports `official (env)` vs `shared web-player (cached)`.
2. **Music User Token (MUT)** — identifies YOU. Obtained interactively:
   - App builds `https://authorize.music.apple.com/woa?...` (see `core::auth::build_authorize_url`).
   - Approve in your system browser with your Apple ID (2FA as usual).
   - Popup `postMessage({method:'authorize', params:[mut]})` → paste MUT into
     the app → stored in memory (keyring persistence in Phase 3).

## Optional: supply your own official token (preferred long-term)

Apple Developer Program ($99/yr) → Media ID + MusicKit `.p8` → sign ES256 JWT.
Then:

```bash
export APPLE_MUSIC_DEVELOPER_TOKEN="eyJ..."
cargo tauri dev
```

This takes precedence over the auto-fetched shared token.
