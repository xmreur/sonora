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
2. **Music User Token (MUT)** — identifies YOU.
   - **Automatic (recommended):** Settings → Account → *Sign in with Apple Music*.
     The app opens a one-shot localhost page in your browser; click Authorize
     there and approve Apple Music — the token lands in the app by itself
     (flow inspired by `matteing/am-keyman`, reimplemented locally with no
     extra dependencies). No copy-paste. Cancel from the app if you change
     your mind; *Log out* removes the token from memory and disk and stops
     the sidecar.
   - **Manual fallback:** *Get authorize URL* → approve in the browser →
     paste the MUT (or the full redirect URL) into the fields below.
     Stored in memory + `~/.config/sonora/music_user_token` (0600).

   The sidebar shows the active account's region (e.g. `Account: IT`,
   from `/v1/me/storefront`, disk-cached for 24h in `~/.config/sonora/account_info`).
   Apple exposes no name/email on the Music API — the storefront is the
   only account-distinguishing fact available.
## Optional: supply your own official token (preferred long-term)

Apple Developer Program ($99/yr) → Media ID + MusicKit `.p8` → sign ES256 JWT.
Then:

```bash
export APPLE_MUSIC_DEVELOPER_TOKEN="eyJ..."
cargo tauri dev
```

This takes precedence over the auto-fetched shared token.
