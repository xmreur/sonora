# Token setup (subscription-only — no paid developer account needed)

You only need your normal **Apple Music subscription**. The app handles the
developer token automatically:

1. **Developer token (auto)** — on first authorize/search, the app scrapes
   Apple's own web-player token from the `index-legacy~*.js` bundle on
   `beta.music.apple.com` (same token for everyone,
   `core::token::fetch_web_player_token`), caches it at
   `~/.config/sonora/web_token_cache`, and reuses it **while it is still
   valid**. The cache is expiry-aware: once the JWT is within 5 minutes of
   its `exp` claim, the copy is dropped and re-scraped — so an expired token
   can no longer block the sign-in page (`MusicKit setup failed: Initialized
   with an expired token.`) or turn catalog calls into `401 Unauthorized`.
   Catalog calls go to `amp-api.music.apple.com` with
   `Origin: https://music.apple.com` (required for web tokens; verified
   working).
   - Fragile by nature: Apple rotates it every few months / may change the
     bundle layout. If auto-fetch fails, the error tells you and you can
     supply your own token (see below).
   - The open sign-in page recovers on its own too: it re-fetches a fresh
     JWT from the one-shot server (`GET /dev-token`) and retries setup.
   - `token_status` reports `official (env)` vs `shared web-player (cached)`,
     including the remaining lifetime of a cached token.
   - A 401 in the app is explained, not echoed: expired developer token →
     refreshed automatically (just retry); `APPLE_MUSIC_DEVELOPER_TOKEN`
     expired → renew the env key; otherwise the MUT was rejected and the
     app shows a *"session expired"* banner with a **Sign in again** button.
2. **Music User Token (MUT)** — identifies YOU.
   - **Automatic (recommended):** Settings → Account → *Sign in with Apple Music*.
     The app opens a one-shot localhost page in your browser; click Authorize
     there and approve Apple Music — the token lands in the app by itself
     (flow inspired by `matteing/am-keyman`, reimplemented locally with no
     extra dependencies). No copy-paste. Cancel from the app if you change
     your mind; *Log out* removes **every stored credential** (the MUT *and*
     the cached developer token, in memory and on disk) and stops playback —
     nothing has to be deleted by hand before the next login.
   - **Manual fallback:** *Get authorize URL* → approve in the browser →
     paste the MUT (or the full redirect URL) into the fields below.
     Stored in memory + `~/.config/sonora/music_user_token` (0600).
## Optional: supply your own official token (preferred long-term)

Apple Developer Program ($99/yr) → Media ID + MusicKit `.p8` → sign ES256 JWT.
Then:

```bash
export APPLE_MUSIC_DEVELOPER_TOKEN="eyJ..."
cargo run -p sonora
```

This takes precedence over the auto-fetched shared token.
