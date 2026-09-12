use crate::error::{CoreError, Result};
use crate::models::{
    parse_album_detail, parse_artist_albums_page, parse_artist_detail, parse_charts_response,
    parse_library_playlists, parse_lrc, parse_lyrics, parse_playlist_detail, parse_search_response,
    parse_track_item, pick_best_track_match, sort_albums_newest_first, strip_lrc_timestamps,
    AlbumDetail, ArtistDetail, Lyrics, Playlist, PlaylistDetail, SearchResults, Track,
};
use crate::token::TokenProvider;

/// Thin wrapper over the Apple Music catalog API.
/// Defaults to `amp-api.music.apple.com` (accepts the shared web-player token
/// when `Origin: https://music.apple.com` is sent); override with
/// [`ApiClient::new_with_base`] for the official `api.music.apple.com`.
/// Catalog calls need only the developer token; `/v1/me/...` also need MUT.
pub struct ApiClient<'a> {
    provider: &'a dyn TokenProvider,
    http: reqwest::Client,
    pub storefront: String,
    pub base: String,
}

/// Guess the catalog storefront from a POSIX locale string
/// (`it_IT.UTF-8` → `it`, `en-US` → `us`, bare `de` → `de`).
/// Returns `None` for `C`/`POSIX`/unparsable so callers fall back to `us`.
pub fn storefront_from_locale(locale: &str) -> Option<String> {
    let lang = locale.split(['.', '@']).next()?.trim();
    if lang.is_empty() {
        return None;
    }
    let parts: Vec<&str> = lang.split(['_', '-']).collect();
    let code = match parts.as_slice() {
        [single] => single,
        [_, region, ..] => region,
        [] => return None,
    };
    if code.len() == 2 && code.bytes().all(|b| b.is_ascii_alphabetic()) {
        Some(code.to_ascii_lowercase())
    } else {
        None
    }
}

/// Storefront from `LC_ALL`/`LANG` (device region hint for logged-out users).
pub fn system_locale_storefront() -> Option<String> {
    for key in ["LC_ALL", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            if let Some(sf) = storefront_from_locale(&v) {
                return Some(sf);
            }
        }
    }
    None
}

/// Search terms for the artist fallback: the full credit first, then the
/// individual collaborators (`&`, `,`, `+`, feat/ft/with/x). A solo search
/// opens neighborhoods the joint credit never returns (e.g. a featured
/// artist's own catalog). Pure helper, unit-tested.
pub fn artist_search_terms(artist: &str) -> Vec<String> {
    let full = artist.trim().to_string();
    if full.is_empty() {
        return Vec::new();
    }
    let mut terms = vec![full.clone()];
    let mut rest = full.clone();
    for sep in [
        " feat. ", " feat ", " ft. ", " ft ", " with ", " x ", "&", ",", "+",
    ] {
        rest = rest
            .split(sep)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    for part in rest.split('\n').map(str::trim) {
        if !part.is_empty() && part != full && !terms.contains(&part.to_string()) {
            terms.push(part.to_string());
        }
    }
    terms
}

/// Merge tracks into `out` (deduped by id) up to `lim`.
fn push_new_tracks(
    out: &mut Vec<Track>,
    seen: &mut std::collections::HashSet<String>,
    tracks: Vec<Track>,
    lim: usize,
) {
    for t in tracks {
        if out.len() >= lim {
            break;
        }
        if seen.insert(t.id.clone()) {
            out.push(t);
        }
    }
}

/// Featured artists parsed from a track title (`… (feat. X & Y)`,
/// `[ft. X]`, `(con X)`). The artist *field* often omits them, yet their
/// solo catalogs are prime in-vibe autoplay territory. Pure, tested.
pub fn featured_artists_from_title(title: &str) -> Vec<String> {
    // Longest prefixes first; all ASCII so slicing stays on char boundaries.
    const PREFIXES: &[&str] = &[
        "featuring.",
        "featuring",
        "feat.",
        "feat",
        "ft.",
        "ft",
        "with",
        "con ",
    ];
    let mut out = Vec::new();
    let mut rest = title.to_string();
    // Bracketed feature tags: "(feat. A & B)", "[ft. A]", "(con A)".
    for (open, close) in [('(', ')'), ('[', ']')] {
        while let (Some(s), Some(e)) = (rest.find(open), rest.find(close)) {
            if e <= s {
                break;
            }
            let inner: String = rest[s + 1..e].chars().collect();
            let low = inner.to_ascii_lowercase();
            for pre in PREFIXES {
                if let Some(names) = low
                    .strip_prefix(pre)
                    .map(|_| inner[pre.len()..].to_string())
                {
                    for part in names.split(['&', ',', '+']) {
                        let p = part.trim().to_string();
                        if !p.is_empty() && !out.contains(&p) {
                            out.push(p);
                        }
                    }
                    break;
                }
            }
            rest.replace_range(s..=e, " ");
        }
    }
    out
}

/// Normalize a genre name for matching (`Hip-Hop/Rap` ≡ `hiphoprap`).
fn genre_name_key(name: &str) -> String {
    name.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// First resource id of a `{"data": [...]}` id-mapping response
/// (`…/library` ↔ `…/catalog` lookups). Pure helper, unit-tested.
pub fn parse_single_resource_id(json: &serde_json::Value) -> Option<String> {
    json.get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|i| i.get("id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

impl<'a> ApiClient<'a> {
    pub fn new(provider: &'a dyn TokenProvider, storefront: &str) -> Result<Self> {
        Self::new_with_base(provider, storefront, "https://amp-api.music.apple.com")
    }

    pub fn new_with_base(
        provider: &'a dyn TokenProvider,
        storefront: &str,
        base: &str,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
            .build()
            .map_err(|e| CoreError::Http(e.to_string()))?;
        Ok(Self {
            provider,
            http,
            storefront: storefront.to_string(),
            base: base.to_string(),
        })
    }

    fn catalog_url(&self, path: &str) -> String {
        format!("{}/v1/catalog/{}{}", self.base, self.storefront, path)
    }

    pub fn auth_headers(&self, needs_user: bool) -> Result<reqwest::header::HeaderMap> {
        let dev = self.provider.developer_token()?;
        let mut h = reqwest::header::HeaderMap::new();
        let bearer = format!("Bearer {dev}");
        h.insert(
            reqwest::header::AUTHORIZATION,
            bearer
                .parse()
                .map_err(|e| CoreError::Http(format!("bad token header: {e}")))?,
        );
        // Required by amp-api for web-player tokens; harmless on the official endpoint.
        h.insert(
            reqwest::header::ORIGIN,
            "https://music.apple.com"
                .parse()
                .map_err(|e| CoreError::Http(format!("bad origin header: {e}")))?,
        );
        h.insert(
            reqwest::header::REFERER,
            "https://music.apple.com/"
                .parse()
                .map_err(|e| CoreError::Http(format!("bad referer header: {e}")))?,
        );
        h.insert(
            reqwest::header::ACCEPT,
            "application/json"
                .parse()
                .map_err(|e| CoreError::Http(format!("bad accept header: {e}")))?,
        );
        if needs_user {
            let mut_ = self
                .provider
                .music_user_token()
                .ok_or(CoreError::MissingUserToken)?;
            let parsed: reqwest::header::HeaderValue = mut_
                .parse()
                .map_err(|e| CoreError::Http(format!("bad MUT header: {e}")))?;
            h.insert("Music-User-Token", parsed.clone());
            h.insert("Media-User-Token", parsed);
        }
        Ok(h)
    }

    pub async fn search(&self, term: &str, limit: u8) -> Result<SearchResults> {
        self.search_paged(term, limit, 0).await
    }

    /// `search` with a result-page offset. Starved autoplay passes deeper
    /// pages so fixed windows (station top-N, artist top-25, charts) keep
    /// yielding fresh tracks instead of the same exhausted page.
    pub async fn search_paged(&self, term: &str, limit: u8, offset: u32) -> Result<SearchResults> {
        let headers = self.auth_headers(false)?;
        let url = self.catalog_url("/search");
        let res = self
            .http
            .get(url)
            .headers(headers)
            .query(&[
                ("term", term.to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
                ("types", "songs,albums,playlists,artists".to_string()),
            ])
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        if !res.status().is_success() {
            return Err(CoreError::Http(format!("search: http {}", res.status())));
        }
        let v: serde_json::Value = res
            .json()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        Ok(parse_search_response(&v))
    }

    async fn get_json(&self, url: String, needs_user: bool) -> Result<serde_json::Value> {
        let (status, v) = self.fetch_json(url, needs_user).await?;
        if !status.is_success() {
            return Err(CoreError::Http(format!("api: http {}", status)));
        }
        Ok(v)
    }

    async fn fetch_json(
        &self,
        url: String,
        needs_user: bool,
    ) -> Result<(reqwest::StatusCode, serde_json::Value)> {
        let headers = self.auth_headers(needs_user)?;
        let res = self
            .http
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            let v = res
                .json()
                .await
                .map_err(|e| CoreError::Http(e.to_string()))?;
            Ok((status, v))
        } else {
            Ok((status, serde_json::Value::Null))
        }
    }

    /// Top charts (songs/albums/playlists). Catalog only, no MUT needed.
    pub async fn charts(&self, limit: u8) -> Result<SearchResults> {
        let headers = self.auth_headers(false)?;
        let res = self
            .http
            .get(self.catalog_url("/charts"))
            .headers(headers)
            .query(&[
                ("types", "songs,albums,playlists".to_string()),
                ("limit", limit.to_string()),
            ])
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        if !res.status().is_success() {
            return Err(CoreError::Http(format!("charts: http {}", res.status())));
        }
        let v: serde_json::Value = res
            .json()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        Ok(parse_charts_response(&v))
    }

    /// Artist + their albums. `?include=albums` alone truncates the
    /// to-many relationship, which can hide a brand-new single behind a
    /// large back catalog — so the relationship endpoint is paged as well,
    /// merged (deduped), and sorted newest-first.
    pub async fn get_artist(&self, id: &str) -> Result<ArtistDetail> {
        let v = self
            .get_json(
                self.catalog_url(&format!("/artists/{id}?include=albums")),
                false,
            )
            .await?;
        let mut detail = parse_artist_detail(&v)
            .ok_or_else(|| CoreError::Http("artist: empty response".into()))?;
        let href = v
            .pointer("/data/0/relationships/albums/href")
            .and_then(|h| h.as_str())
            .map(str::to_string);
        let mut next = Some(
            href.unwrap_or_else(|| self.catalog_url(&format!("/artists/{id}/albums?limit=100"))),
        );
        let mut seen: std::collections::HashSet<String> =
            detail.albums.iter().map(|a| a.id.clone()).collect();
        // Cap pages so a huge catalog can't loop forever (5 × 100).
        for _ in 0..5 {
            let url = match next.take() {
                Some(u) => u,
                None => break,
            };
            let url = if url.starts_with("http") {
                url
            } else {
                format!("{}{}", self.base.trim_end_matches('/'), url)
            };
            let page = match self.fetch_json(url, false).await {
                Ok((status, v)) if status.is_success() => v,
                _ => break,
            };
            let (albums, more) = parse_artist_albums_page(&page);
            if albums.is_empty() && more.is_none() {
                break;
            }
            for a in albums {
                if seen.insert(a.id.clone()) {
                    detail.albums.push(a);
                }
            }
            next = more;
        }
        sort_albums_newest_first(&mut detail.albums);
        Ok(detail)
    }

    /// Album + its tracks (`?include=tracks`).
    pub async fn get_album(&self, id: &str) -> Result<AlbumDetail> {
        let v = self
            .get_json(
                self.catalog_url(&format!("/albums/{id}?include=tracks")),
                false,
            )
            .await?;
        parse_album_detail(&v).ok_or_else(|| CoreError::Http("album: empty response".into()))
    }

    /// Playlist + its tracks. Library ids (`p.…`) hit `/v1/me/...` (needs MUT),
    /// numeric ids hit the catalog.
    pub async fn get_playlist(&self, id: &str) -> Result<PlaylistDetail> {
        let v = if id.starts_with("p.") {
            let url = format!("{}/v1/me/library/playlists/{id}?include=tracks", self.base);
            self.get_json(url, true).await?
        } else {
            self.get_json(
                self.catalog_url(&format!("/playlists/{id}?include=tracks")),
                false,
            )
            .await?
        };
        parse_playlist_detail(&v).ok_or_else(|| CoreError::Http("playlist: empty response".into()))
    }

    /// The user's library playlists (needs MUT).
    pub async fn library_playlists(&self) -> Result<Vec<Playlist>> {
        let url = format!("{}/v1/me/library/playlists?limit=100", self.base);
        let v = self.get_json(url, true).await?;
        Ok(parse_library_playlists(&v))
    }

    /// True when `song_id` exists in this account's catalog storefront.
    pub async fn catalog_song_exists(&self, song_id: &str) -> Result<bool> {
        if song_id.trim().is_empty() {
            return Ok(false);
        }
        let url = reqwest::Url::parse(&self.catalog_url(&format!("/songs/{song_id}")))
            .map_err(|e| CoreError::Http(format!("catalog song url: {e}")))?;
        let (status, _) = self.fetch_url(url, false).await?;
        Ok(status.is_success())
    }

    /// Re-resolve a catalog song id for the user's storefront (search fallback).
    pub async fn resolve_catalog_song_id(
        &self,
        song_id: &str,
        artist: &str,
        title: &str,
    ) -> Result<String> {
        if !song_id.trim().is_empty() && self.catalog_song_exists(song_id).await? {
            return Ok(song_id.to_string());
        }
        let term = format!("{title} {artist}").trim().to_string();
        if term.is_empty() {
            return Ok(song_id.to_string());
        }
        let results = self.search(&term, 10).await?;
        Ok(pick_best_track_match(&results.tracks, title, artist)
            .map(|t| t.id.clone())
            .unwrap_or_else(|| song_id.to_string()))
    }

    /// Line-level lyrics (`/songs/{id}/lyrics`). Often plain text only.
    /// Subscriber-gated: needs the MUT.
    pub async fn get_lyrics(&self, song_id: &str) -> Result<Lyrics> {
        let v = self
            .get_json(self.catalog_url(&format!("/songs/{song_id}/lyrics")), true)
            .await?;
        parse_lyrics(&v)
            .ok_or_else(|| CoreError::Http("lyrics: none published for this song".into()))
    }

    /// Account storefront (e.g. `us`, `it`). Needs MUT.
    pub async fn user_storefront(&self) -> Result<String> {
        let url = format!("{}/v1/me/storefront", self.base.trim_end_matches('/'));
        let v = self.get_json(url, true).await?;
        v.get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .and_then(|i| i.get("id"))
            .and_then(|id| id.as_str())
            .map(str::to_string)
            .ok_or_else(|| CoreError::Http("storefront: missing id".into()))
    }

    /// BCP-47 tag for `l[lyrics]` (Apple expects `en-US`, not `en-us`).
    fn lyrics_locale_for_storefront(storefront: &str) -> String {
        match storefront.to_ascii_lowercase().as_str() {
            "us" => "en-US".into(),
            "gb" => "en-GB".into(),
            "au" => "en-AU".into(),
            "ca" => "en-CA".into(),
            "it" => "it-IT".into(),
            "de" => "de-DE".into(),
            "fr" => "fr-FR".into(),
            "es" => "es-ES".into(),
            "jp" => "ja-JP".into(),
            "kr" => "ko-KR".into(),
            "cn" => "zh-Hans-CN".into(),
            "tw" => "zh-Hant-TW".into(),
            other => {
                if other.contains('-') {
                    other.to_string()
                } else {
                    format!("{}-{}", other, other.to_uppercase())
                }
            }
        }
    }

    /// Script tag for `l[script]` (`en-Latn`, not bare `Latn`).
    fn lyrics_script_for_storefront(storefront: &str) -> String {
        match storefront.to_ascii_lowercase().as_str() {
            "cn" => "zh-Hans".into(),
            "tw" => "zh-Hant".into(),
            "jp" => "ja-Jpan".into(),
            "kr" => "ko-Hang".into(),
            other => {
                let locale = Self::lyrics_locale_for_storefront(other);
                let lang = locale.split('-').next().unwrap_or("en");
                format!("{}-Latn", lang)
            }
        }
    }

    fn syllable_lyrics_url(&self, song_id: &str, query: &[(&str, &str)]) -> Result<reqwest::Url> {
        let mut url =
            reqwest::Url::parse(&self.catalog_url(&format!("/songs/{song_id}/syllable-lyrics")))
                .map_err(|e| CoreError::Http(format!("syllable url: {e}")))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        Ok(url)
    }

    async fn fetch_url(
        &self,
        url: reqwest::Url,
        needs_user: bool,
    ) -> Result<(reqwest::StatusCode, serde_json::Value)> {
        let headers = self.auth_headers(needs_user)?;
        let res = self
            .http
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            let v = res
                .json()
                .await
                .map_err(|e| CoreError::Http(e.to_string()))?;
            Ok((status, v))
        } else {
            Ok((status, serde_json::Value::Null))
        }
    }

    /// Word/syllable-timed TTML (`/songs/{id}/syllable-lyrics`). One amp-api
    /// request with storefront locale; on 404 retries plain `extend=ttmlLocalizations`.
    /// Retries once on 429.
    pub async fn get_syllable_lyrics(&self, song_id: &str) -> Result<Lyrics> {
        let locale = Self::lyrics_locale_for_storefront(&self.storefront);
        let script = Self::lyrics_script_for_storefront(&self.storefront);
        let localized = [
            ("l[lyrics]", locale.as_str()),
            ("l[script]", script.as_str()),
            ("extend", "ttmlLocalizations"),
        ];
        let extend_only = [("extend", "ttmlLocalizations")];

        let parse_syllable = |v: &serde_json::Value| -> Option<Lyrics> {
            parse_lyrics(v).map(|mut lyrics| {
                lyrics.source = "apple-syllable".into();
                lyrics
            })
        };

        let localized_url = self.syllable_lyrics_url(song_id, &localized)?;
        let (status, v) = self.fetch_url(localized_url, true).await?;
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let retry_url = self.syllable_lyrics_url(song_id, &localized)?;
            let (status2, v2) = self.fetch_url(retry_url, true).await?;
            if status2.is_success() {
                return parse_syllable(&v2)
                    .ok_or_else(|| CoreError::Http("syllable-lyrics: unparsable".into()));
            }
            return Err(CoreError::Http(format!(
                "syllable-lyrics: http {}",
                status2
            )));
        }
        if status.is_success() {
            return parse_syllable(&v)
                .ok_or_else(|| CoreError::Http("syllable-lyrics: unparsable".into()));
        }
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::BAD_REQUEST {
            let ext_url = self.syllable_lyrics_url(song_id, &extend_only)?;
            let (status2, v2) = self.fetch_url(ext_url, true).await?;
            if status2.is_success() {
                return parse_syllable(&v2)
                    .ok_or_else(|| CoreError::Http("syllable-lyrics: unparsable (extend)".into()));
            }
            return Err(CoreError::Http(format!(
                "syllable-lyrics: http {}",
                status2
            )));
        }
        Err(CoreError::Http(format!("syllable-lyrics: http {}", status)))
    }

    /// Free fallback lyrics (LRCLIB, no key) with line timings preserved.
    pub async fn get_lyrics_lrclib(&self, artist: &str, title: &str) -> Result<Lyrics> {
        let res = self
            .http
            .get("https://lrclib.net/api/get")
            .query(&[("artist_name", artist), ("track_name", title)])
            .header("User-Agent", "apple-music-linux-client/0.1")
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        if !res.status().is_success() {
            return Err(CoreError::Http(format!("lrclib: http {}", res.status())));
        }
        let v: serde_json::Value = res
            .json()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        let synced = v.get("syncedLyrics").and_then(|s| s.as_str()).unwrap_or("");
        let lines = parse_lrc(synced);
        let plain = v
            .get("plainLyrics")
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let text = match (plain, lines.is_empty()) {
            (Some(p), _) => p,
            (None, false) => strip_lrc_timestamps(synced),
            (None, true) => String::new(),
        };
        if text.trim().is_empty() {
            return Err(CoreError::Http("lrclib: no lyrics for this song".into()));
        }
        Ok(Lyrics {
            text,
            synced: !lines.is_empty(),
            lines,
            source: String::new(),
        })
    }

    /// True when `id` is a library-song id (`i.…`), not a catalog id.
    pub fn is_library_song_id(id: &str) -> bool {
        id.starts_with("i.")
    }

    /// Playlist track `type` for an id (catalog ids must be resolved before POST).
    pub fn playlist_track_type_for_id(id: &str) -> &'static str {
        if Self::is_library_song_id(id) {
            "library-songs"
        } else {
            "songs"
        }
    }

    /// Add catalog songs to the user's library / favorites (needs MUT).
    /// Explicit user action — never called implicitly by playlist flows.
    pub async fn add_to_library(&self, song_ids: &[String]) -> Result<usize> {
        let catalog_ids: Vec<String> = song_ids
            .iter()
            .filter(|id| !Self::is_library_song_id(id))
            .cloned()
            .collect();
        // Library-song ids are already in the library.
        if catalog_ids.is_empty() {
            return Ok(song_ids.len());
        }
        self.add_catalog_songs_to_library(&catalog_ids).await?;
        Ok(song_ids.len())
    }

    /// Low-level library add used by [`ApiClient::add_to_library`] and by the
    /// playlist fallback path (only after the catalog-only attempt fails).
    async fn add_catalog_songs_to_library(&self, catalog_ids: &[String]) -> Result<()> {
        if catalog_ids.is_empty() {
            return Ok(());
        }
        let headers = self.auth_headers(true)?;
        let url = format!("{}/v1/me/library", self.base.trim_end_matches('/'));
        let ids_param = catalog_ids.join(",");
        let res = self
            .http
            .post(url)
            .headers(headers)
            .query(&[("ids[songs]", ids_param)])
            .body("")
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let detail = Self::http_error_detail(res).await;
        Err(CoreError::Http(format!(
            "add-to-library: http {}{}",
            status, detail
        )))
    }

    /// Map a library-song id (`i.…`) back to its catalog id (needs MUT).
    /// Catalog-only endpoints (stations, song views) 404 on library ids.
    async fn catalog_id_for_library_song(&self, library_id: &str) -> Result<String> {
        let url = format!(
            "{}/v1/me/library/songs/{library_id}/catalog",
            self.base.trim_end_matches('/')
        );
        let v = self.get_json(url, true).await?;
        parse_single_resource_id(&v)
            .ok_or_else(|| CoreError::Http(format!("catalog-id: no mapping for {library_id}")))
    }

    /// Map a catalog song id to its library-song id (needs MUT + song in library).
    async fn library_song_id_for_catalog(&self, catalog_id: &str) -> Result<String> {
        let v = self
            .get_json(
                self.catalog_url(&format!("/songs/{catalog_id}/library")),
                true,
            )
            .await?;
        parse_single_resource_id(&v)
            .ok_or_else(|| CoreError::Http(format!("library-id: no mapping for {catalog_id}")))
    }

    /// Resolve catalog ids to library-song ids for playlist mutation.
    async fn resolve_library_song_ids(&self, song_ids: &[String]) -> Result<Vec<String>> {
        let catalog_ids: Vec<String> = song_ids
            .iter()
            .filter(|id| !Self::is_library_song_id(id))
            .cloned()
            .collect();
        if !catalog_ids.is_empty() {
            self.add_catalog_songs_to_library(&catalog_ids).await?;
            // Apple notes a delay before new library resources are queryable.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        let mut resolved = Vec::with_capacity(song_ids.len());
        for id in song_ids {
            if Self::is_library_song_id(id) {
                resolved.push(id.clone());
            } else {
                resolved.push(self.library_song_id_for_catalog(id).await?);
            }
        }
        Ok(resolved)
    }

    async fn post_playlist_tracks(
        &self,
        base: &str,
        playlist_id: &str,
        body: &serde_json::Value,
    ) -> Result<()> {
        let headers = self.auth_headers(true)?;
        let url = format!(
            "{}/v1/me/library/playlists/{playlist_id}/tracks",
            base.trim_end_matches('/')
        );
        let res = self
            .http
            .post(url)
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let detail = Self::http_error_detail(res).await;
        Err(CoreError::Http(format!(
            "add-to-playlist: http {status}{detail}"
        )))
    }

    /// DELETE variant with an `ids[library-songs]` query and no body —
    /// byte-for-byte what Apple's own web client sends (`&mode=all` is
    /// mandatory; without it amp-api 400s "No mode supplied"). MusicKit
    /// likewise never puts a body on DELETE (params go in the query).
    async fn delete_playlist_tracks_query(
        &self,
        base: &str,
        playlist_id: &str,
        library_ids: &[String],
    ) -> Result<()> {
        let headers = self.auth_headers(true)?;
        let url = format!(
            "{}/v1/me/library/playlists/{playlist_id}/tracks",
            base.trim_end_matches('/')
        );
        let res = self
            .http
            .delete(url)
            .headers(headers)
            .query(&[
                ("ids[library-songs]", library_ids.join(",")),
                ("mode", "all".to_string()),
            ])
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let detail = Self::http_error_detail(res).await;
        Err(CoreError::Http(format!("http {status}{detail}")))
    }

    /// Map ids to library-song ids via read-only lookup (no library mutation,
    /// unlike [`ApiClient::resolve_library_song_ids`]). Unmappable ids are
    /// skipped.
    async fn map_to_library_ids_readonly(&self, song_ids: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(song_ids.len());
        for id in song_ids {
            if Self::is_library_song_id(id) {
                out.push(id.clone());
            } else if let Ok(mapped) = self.library_song_id_for_catalog(id).await {
                out.push(mapped);
            }
        }
        out
    }

    /// Remove songs from a library playlist (needs MUT). Only library
    /// playlists (`p.…`) are mutable — catalog playlists are read-only.
    /// Sends exactly what Apple's web client sends (`DELETE …/tracks`
    /// with `?ids[library-songs]=…&mode=all`, no body), trying reported
    /// library ids plus read-only-mapped catalog ids across both API
    /// bases. Failures carry a per-attempt trail (`query@base: status`).
    pub async fn remove_from_playlist(
        &self,
        playlist_id: &str,
        song_ids: &[String],
    ) -> Result<usize> {
        if song_ids.is_empty() {
            return Ok(0);
        }
        let mut bases = vec![self.base.trim_end_matches('/').to_string()];
        let official = "https://api.music.apple.com";
        if !bases.iter().any(|b| b == official) {
            bases.push(official.into());
        }
        fn base_tag(base: &str) -> &'static str {
            if base.contains("amp-api") {
                "amp"
            } else {
                "official"
            }
        }
        let mut trail: Vec<String> = Vec::new();
        let library_ids = self.map_to_library_ids_readonly(song_ids).await;
        if library_ids.is_empty() {
            return Err(CoreError::Http(
                "remove-from-playlist: no library ids (songs not in library?)".into(),
            ));
        }
        for base in &bases {
            match self
                .delete_playlist_tracks_query(base, playlist_id, &library_ids)
                .await
            {
                Ok(()) => return Ok(song_ids.len()),
                Err(e) => trail.push(format!("query@{}: {e}", base_tag(base))),
            }
        }
        Err(CoreError::Http(format!(
            "remove-from-playlist: failed [{}]",
            trail.join("; ")
        )))
    }

    async fn http_error_detail(res: reqwest::Response) -> String {
        let text = res.text().await.unwrap_or_default();
        if text.is_empty() {
            return String::new();
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(errors) = v.get("errors").and_then(|e| e.as_array()) {
                let parts: Vec<String> = errors
                    .iter()
                    .filter_map(|e| {
                        let title = e.get("title").and_then(|t| t.as_str()).unwrap_or("");
                        let detail = e.get("detail").and_then(|d| d.as_str()).unwrap_or("");
                        if !detail.is_empty() {
                            Some(format!("{title}: {detail}"))
                        } else if !title.is_empty() {
                            Some(title.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();
                if !parts.is_empty() {
                    return format!(" ({})", parts.join("; "));
                }
            }
        }
        let trimmed: String = text.chars().take(200).collect();
        if trimmed.is_empty() {
            String::new()
        } else {
            format!(" ({trimmed})")
        }
    }

    /// Add songs to a library playlist (needs MUT).
    /// Catalog-only first so the call never pollutes the user's library /
    /// favorites as a side effect. Only if every catalog attempt fails do we
    /// fall back to resolving library-song ids (which does add to library).
    /// Use [`ApiClient::add_to_library`] for the explicit "Add to favorites" action.
    pub async fn add_to_playlist(&self, playlist_id: &str, song_ids: &[String]) -> Result<usize> {
        if song_ids.is_empty() {
            return Ok(0);
        }
        let catalog_ids: Vec<String> = song_ids
            .iter()
            .filter(|id| !Self::is_library_song_id(id))
            .cloned()
            .collect();

        let mut bases = vec![self.base.trim_end_matches('/').to_string()];
        let official = "https://api.music.apple.com";
        if !bases.iter().any(|b| b == official) {
            bases.push(official.into());
        }

        let mut last_err: Option<CoreError> = None;

        // Pass 1: catalog ids only — no library side effects.
        let mut catalog_bodies: Vec<serde_json::Value> = Vec::new();
        if !catalog_ids.is_empty() {
            catalog_bodies.push(Self::add_tracks_body_with_type(&catalog_ids, "songs"));
        } else {
            catalog_bodies.push(Self::add_tracks_body(song_ids));
        }
        for base in &bases {
            for body in &catalog_bodies {
                match self.post_playlist_tracks(base, playlist_id, body).await {
                    Ok(()) => return Ok(song_ids.len()),
                    Err(e) => last_err = Some(e),
                }
            }
        }

        // Pass 2 (fallback): resolve library-song ids, then retry. This path
        // adds the songs to the library — only reached when catalog POSTs fail.
        let library_ids = self.resolve_library_song_ids(song_ids).await?;
        let library_bodies = vec![
            Self::add_tracks_body(&library_ids),
            Self::add_tracks_body_with_type(&library_ids, "songs"),
        ];
        for base in &bases {
            for body in &library_bodies {
                match self.post_playlist_tracks(base, playlist_id, body).await {
                    Ok(()) => return Ok(song_ids.len()),
                    Err(e) => last_err = Some(e),
                }
            }
        }
        Err(last_err.unwrap_or_else(|| CoreError::Http("add-to-playlist: failed".into())))
    }

    /// Create a library playlist (needs MUT). Returns the new playlist id.
    pub async fn create_playlist(&self, name: &str) -> Result<String> {
        let headers = self.auth_headers(true)?;
        let body = serde_json::json!({ "attributes": { "name": name } });
        let url = format!("{}/v1/me/library/playlists", self.base);
        let res = self
            .http
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        if !res.status().is_success() {
            return Err(CoreError::Http(format!(
                "create-playlist: http {}",
                res.status()
            )));
        }
        let v: serde_json::Value = res
            .json()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        v.get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .and_then(|item| item.get("id"))
            .and_then(|id| id.as_str())
            .map(str::to_string)
            .ok_or_else(|| CoreError::Http("create-playlist: no id returned".into()))
    }

    /// Request body for [`ApiClient::add_to_playlist`] (unit-tested shape).
    pub fn add_tracks_body(song_ids: &[String]) -> serde_json::Value {
        serde_json::json!({
            "data": song_ids
                .iter()
                .map(|id| serde_json::json!({
                    "id": id,
                    "type": Self::playlist_track_type_for_id(id)
                }))
                .collect::<Vec<_>>()
        })
    }

    pub fn add_tracks_body_with_type(song_ids: &[String], track_type: &str) -> serde_json::Value {
        serde_json::json!({
            "data": song_ids
                .iter()
                .map(|id| serde_json::json!({ "id": id, "type": track_type }))
                .collect::<Vec<_>>()
        })
    }

    /// Similar / radio tracks for a song (best-effort). Library-song ids
    /// (`i.…`) are mapped to catalog ids first — catalog-only endpoints
    /// 404 on them. Sources merge (station, song views, artist search
    /// incl. collaborators and title features) up to `limit`, skipping
    /// `exclude` (already-queued) ids. `page` offsets the pageable windows
    /// (station/search) so starved callers dig past exhausted pages instead
    /// of re-fetching them. Deliberately no genre charts here: regional
    /// tops drift off-vibe — see `genre_filler_for_song`, which
    /// callers quarantine from seeding.
    pub async fn similar_songs(
        &self,
        song_id: &str,
        limit: u8,
        exclude: &std::collections::HashSet<String>,
        page: u32,
    ) -> Result<Vec<Track>> {
        let lim = limit.clamp(1, 25);
        let catalog_id = if Self::is_library_song_id(song_id) {
            self.catalog_id_for_library_song(song_id)
                .await
                .unwrap_or_else(|_| song_id.to_string())
        } else {
            song_id.to_string()
        };
        let mut out: Vec<Track> = Vec::new();
        let mut seen = std::collections::HashSet::from([catalog_id.clone()]);
        seen.extend(exclude.iter().cloned());
        let off = page.saturating_mul(25);
        // Personal radio station seeded by this song.
        let station_url = self.catalog_url(&format!("/stations?filter[identity]=s.{catalog_id}"));
        if let Ok(v) = self.get_json(station_url, false).await {
            if let Some(station_id) = v
                .get("data")
                .and_then(|d| d.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.get("id"))
                .and_then(|id| id.as_str())
            {
                let tracks_url = self.catalog_url(&format!(
                    "/stations/{station_id}/tracks?limit={lim}&offset={off}"
                ));
                if let Ok(tv) = self.get_json(tracks_url, false).await {
                    let tracks: Vec<Track> = tv
                        .get("data")
                        .and_then(|d| d.as_array())
                        .map(|arr| arr.iter().map(parse_track_item).collect())
                        .unwrap_or_default();
                    push_new_tracks(&mut out, &mut seen, tracks, lim as usize);
                }
            }
        }
        // More from the same artist via song views.
        if out.len() < lim as usize {
            let views_url =
                self.catalog_url(&format!("/songs/{catalog_id}?views=more-by-artist,similar"));
            if let Ok(v) = self.get_json(views_url, false).await {
                if let Some(item) = v
                    .get("data")
                    .and_then(|d| d.as_array())
                    .and_then(|a| a.first())
                {
                    for key in ["more-by-artist", "similar"] {
                        if out.len() >= lim as usize {
                            break;
                        }
                        let tracks: Vec<Track> = item
                            .get("views")
                            .and_then(|views| views.get(key))
                            .and_then(|view| view.get("data"))
                            .and_then(|d| d.as_array())
                            .map(|arr| arr.iter().map(parse_track_item).collect())
                            .unwrap_or_default();
                        push_new_tracks(&mut out, &mut seen, tracks, lim as usize);
                    }
                }
            }
        }
        // Artist catalog via search: full credit, each collaborator solo,
        // then featured artists parsed from the seed title itself (the
        // artist field often omits them, yet their catalogs are prime
        // in-vibe territory).
        if out.len() < lim as usize {
            if let Ok(meta) = self.get_song(&catalog_id).await {
                let attrs = meta
                    .get("data")
                    .and_then(|d| d.as_array())
                    .and_then(|a| a.first())
                    .and_then(|i| i.get("attributes"));
                let artist = attrs
                    .and_then(|a| a.get("artistName"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                let title = attrs
                    .and_then(|a| a.get("name"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                let mut terms = artist_search_terms(artist);
                for feat in featured_artists_from_title(title) {
                    if !terms.contains(&feat) {
                        terms.push(feat);
                    }
                }
                for term in terms {
                    if out.len() >= lim as usize {
                        break;
                    }
                    if let Ok(res) = self.search_paged(&term, 25, off).await {
                        push_new_tracks(&mut out, &mut seen, res.tracks, lim as usize);
                    }
                }
            }
        }
        if out.is_empty() {
            return Err(CoreError::Http("similar: none found for this song".into()));
        }
        Ok(out)
    }

    /// Same-genre chart tracks for a seed song (last-resort autoplay filler).
    /// Returned separately from [`ApiClient::similar_songs`] so callers can
    /// quarantine them: regional top charts drift off-vibe and must never
    /// become seeds for further expansion.
    pub async fn genre_filler_for_song(
        &self,
        song_id: &str,
        limit: u8,
        exclude: &std::collections::HashSet<String>,
        page: u32,
    ) -> Result<Vec<Track>> {
        let lim = limit.clamp(1, 25);
        let catalog_id = if Self::is_library_song_id(song_id) {
            self.catalog_id_for_library_song(song_id)
                .await
                .unwrap_or_else(|_| song_id.to_string())
        } else {
            song_id.to_string()
        };
        let meta = self.get_song(&catalog_id).await?;
        let genres: Vec<String> = meta
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .and_then(|i| i.get("attributes"))
            .and_then(|a| a.get("genreNames"))
            .and_then(|g| g.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let gid = self
            .genre_id_for_names(&genres)
            .await
            .ok_or_else(|| CoreError::Http("similar: no genre for this song".into()))?;
        let off = page.saturating_mul(25);
        let url = self.catalog_url(&format!(
            "/charts?types=songs&genre={gid}&limit=25&offset={off}"
        ));
        let cv = self.get_json(url, false).await?;
        let mut out: Vec<Track> = Vec::new();
        let mut seen = std::collections::HashSet::from([catalog_id]);
        seen.extend(exclude.iter().cloned());
        push_new_tracks(
            &mut out,
            &mut seen,
            parse_charts_response(&cv).tracks,
            lim as usize,
        );
        if out.is_empty() {
            return Err(CoreError::Http(
                "similar: genre charts yielded nothing fresh".into(),
            ));
        }
        Ok(out)
    }

    /// Catalog genre id for the first matching genre name
    /// (`Hip-Hop/Rap` → `18`). Used for same-genre autoplay charts.
    async fn genre_id_for_names(&self, names: &[String]) -> Option<String> {
        if names.is_empty() {
            return None;
        }
        let v = self
            .get_json(self.catalog_url("/genres?limit=100"), false)
            .await
            .ok()?;
        let all: Vec<(String, String)> = v
            .get("data")?
            .as_array()?
            .iter()
            .filter_map(|g| {
                Some((
                    g.get("id")?.as_str()?.to_string(),
                    g.get("attributes")?.get("name")?.as_str()?.to_string(),
                ))
            })
            .collect();
        for want in names {
            let key = genre_name_key(want);
            if key.is_empty() {
                continue;
            }
            if let Some((id, _)) = all.iter().find(|(_, n)| genre_name_key(n) == key) {
                return Some(id.clone());
            }
        }
        None
    }

    pub async fn get_song(&self, id: &str) -> Result<serde_json::Value> {
        let headers = self.auth_headers(false)?;
        let res = self
            .http
            .get(self.catalog_url(&format!("/songs/{id}")))
            .headers(headers)
            .send()
            .await
            .map_err(|e| CoreError::Http(e.to_string()))?;
        if !res.status().is_success() {
            return Err(CoreError::Http(format!("song: http {}", res.status())));
        }
        res.json().await.map_err(|e| CoreError::Http(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::EnvTokenProvider;

    #[test]
    fn storefront_from_locale_parses() {
        assert_eq!(storefront_from_locale("it_IT.UTF-8"), Some("it".into()));
        assert_eq!(storefront_from_locale("en_US"), Some("us".into()));
        assert_eq!(storefront_from_locale("en-US"), Some("us".into()));
        assert_eq!(storefront_from_locale("fr_FR@euro"), Some("fr".into()));
        assert_eq!(storefront_from_locale("de"), Some("de".into()));
        assert_eq!(storefront_from_locale("C.UTF-8"), None);
        assert_eq!(storefront_from_locale("C"), None);
        assert_eq!(storefront_from_locale("POSIX"), None);
        assert_eq!(storefront_from_locale(""), None);
    }

    #[test]
    fn parses_single_resource_id() {
        let v = serde_json::json!({"data": [{"id": "123", "type": "songs"}]});
        assert_eq!(parse_single_resource_id(&v).as_deref(), Some("123"));
        assert_eq!(parse_single_resource_id(&serde_json::json!({})), None);
        assert_eq!(
            parse_single_resource_id(&serde_json::json!({"data": []})),
            None
        );
    }

    #[test]
    fn artist_terms_split_collaborators() {
        assert_eq!(
            artist_search_terms("Rafilù, Hosawa & Silent Bob"),
            vec![
                "Rafilù, Hosawa & Silent Bob",
                "Rafilù",
                "Hosawa",
                "Silent Bob"
            ]
        );
        assert_eq!(
            artist_search_terms("Il Ghost feat. Silent Bob"),
            vec!["Il Ghost feat. Silent Bob", "Il Ghost", "Silent Bob"]
        );
        assert_eq!(artist_search_terms("Uzi Lvke"), vec!["Uzi Lvke"]);
        assert_eq!(artist_search_terms(""), Vec::<String>::new());
        // "and" is not a separator (Simon and Garfunkel stay whole).
        assert_eq!(
            artist_search_terms("Simon and Garfunkel"),
            vec!["Simon and Garfunkel"]
        );
    }

    #[test]
    fn genre_name_keys_match() {
        assert_eq!(genre_name_key("Hip-Hop/Rap"), "hiphoprap");
        assert_eq!(genre_name_key("Hip-Hop/Rap"), genre_name_key("hiphoprap"));
        assert_eq!(genre_name_key(""), "");
    }

    #[test]
    fn title_features_parse() {
        assert_eq!(
            featured_artists_from_title("Potevamo (feat. Emis Killa)"),
            vec!["Emis Killa"]
        );
        assert_eq!(
            featured_artists_from_title("Autostrada Del Sole (feat. Massimo Pericolo & Crookers)"),
            vec!["Massimo Pericolo", "Crookers"]
        );
        assert_eq!(
            featured_artists_from_title("DOMANI [ft. Crookers]"),
            vec!["Crookers"]
        );
        assert_eq!(
            featured_artists_from_title("Plain Title (Remastered)"),
            Vec::<String>::new()
        );
        assert_eq!(
            featured_artists_from_title("No Brackets"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn headers_require_dev_token() {
        let p = EnvTokenProvider::new("DEFINITELY_NOT_SET_XYZ");
        let c = ApiClient::new(&p, "us").unwrap();
        assert!(matches!(
            c.auth_headers(false),
            Err(CoreError::MissingDeveloperToken)
        ));
    }

    #[test]
    fn catalog_url_shape() {
        let p = EnvTokenProvider::new("X");
        let c = ApiClient::new(&p, "us").unwrap();
        assert_eq!(
            c.catalog_url("/search"),
            "https://amp-api.music.apple.com/v1/catalog/us/search"
        );
        let c2 = ApiClient::new_with_base(&p, "us", "https://api.music.apple.com").unwrap();
        assert_eq!(
            c2.catalog_url("/search"),
            "https://api.music.apple.com/v1/catalog/us/search"
        );
    }

    #[test]
    fn add_tracks_body_shape() {
        let ids = vec!["i.abc".to_string(), "i.def".to_string()];
        let b = ApiClient::add_tracks_body(&ids);
        assert_eq!(b["data"][0]["type"], "library-songs");
        assert_eq!(b["data"][1]["id"], "i.def");
    }

    #[test]
    fn playlist_track_type_classifies_ids() {
        assert_eq!(
            ApiClient::playlist_track_type_for_id("i.x"),
            "library-songs"
        );
        assert_eq!(ApiClient::playlist_track_type_for_id("123456"), "songs");
        assert!(ApiClient::is_library_song_id("i.abc"));
        assert!(!ApiClient::is_library_song_id("123456"));
        let catalog = ApiClient::add_tracks_body_with_type(&["9".into()], "songs");
        assert_eq!(catalog["data"][0]["type"], "songs");
        assert_eq!(catalog["data"][0]["id"], "9");
    }

    #[test]
    fn lyrics_locale_tags_are_bcp47() {
        assert_eq!(ApiClient::lyrics_locale_for_storefront("us"), "en-US");
        assert_eq!(ApiClient::lyrics_script_for_storefront("us"), "en-Latn");
        assert_eq!(ApiClient::lyrics_locale_for_storefront("it"), "it-IT");
        assert_eq!(ApiClient::lyrics_script_for_storefront("it"), "it-Latn");
    }

    #[tokio::test]
    async fn remove_empty_is_noop() {
        std::env::set_var("TEST_DEV_TOKEN_RM", "dummy");
        let p = EnvTokenProvider::new("TEST_DEV_TOKEN_RM");
        let c = ApiClient::new(&p, "us").unwrap();
        assert_eq!(c.remove_from_playlist("p.x", &[]).await.unwrap(), 0);
        std::env::remove_var("TEST_DEV_TOKEN_RM");
    }

    #[test]
    fn headers_include_origin_for_amp_api() {
        std::env::set_var("TEST_DEV_TOKEN_XYZ", "dummy");
        let p = EnvTokenProvider::new("TEST_DEV_TOKEN_XYZ");
        let c = ApiClient::new(&p, "us").unwrap();
        let h = c.auth_headers(false).unwrap();
        assert_eq!(
            h.get(reqwest::header::ORIGIN).unwrap(),
            "https://music.apple.com"
        );
        std::env::remove_var("TEST_DEV_TOKEN_XYZ");
    }
}
