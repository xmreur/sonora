use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Artwork {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Track {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub artwork: Option<Artwork>,
    #[serde(default)]
    pub isrc: Option<String>,
    /// `attributes.genreNames` (e.g. `["Ambient", "Electronic"]`) — used
    /// for same-genre affinity ranking in autoplay, never shown directly.
    #[serde(default)]
    pub genres: Vec<String>,
    /// DRM-free 30s preview (`attributes.previews[0].url`). Playable without
    /// Widevine — used until the full-track sidecar engine lands.
    #[serde(default)]
    pub preview_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Album {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub artwork: Option<Artwork>,
    /// `attributes.isSingle` — true for single drops (albums endpoint).
    #[serde(default)]
    pub is_single: bool,
    /// `attributes.releaseDate` (`YYYY-MM-DD`) — for newest-first sorting.
    #[serde(default)]
    pub release_date: Option<String>,
}

/// Release kind for artist-page grouping (Singles / EPs / Albums).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseKind {
    Single,
    Ep,
    #[default]
    Album,
}

/// Classify an album: `isSingle` (or a lone track) → Single; 2–6 tracks
/// (or a " - EP" suffixed title — Apple exposes no `isEp` flag) → EP;
/// everything else (incl. unknown shape) → Album.
pub fn release_kind(is_single: bool, track_count: Option<u32>, title: &str) -> ReleaseKind {
    if is_single || track_count == Some(1) {
        return ReleaseKind::Single;
    }
    let ep_suffix = title.trim_end().to_ascii_lowercase().ends_with(" - ep");
    if ep_suffix {
        return ReleaseKind::Ep;
    }
    match track_count {
        Some(n) if (2..=6).contains(&n) => ReleaseKind::Ep,
        _ => ReleaseKind::Album,
    }
}

/// Newest-first by `releaseDate` (`YYYY-MM-DD` sorts lexicographically);
/// undated releases sink to the bottom, order otherwise stable.
pub fn sort_albums_newest_first(albums: &mut [Album]) {
    albums.sort_by(|a, b| match (&a.release_date, &b.release_date) {
        (Some(x), Some(y)) => y.cmp(x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Playlist {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub artwork: Option<Artwork>,
    /// Curator / description blurb when the API provides one.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlbumDetail {
    pub album: Album,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlaylistDetail {
    pub playlist: Playlist,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResults {
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub albums: Vec<Album>,
    #[serde(default)]
    pub playlists: Vec<Playlist>,
    #[serde(default)]
    pub artists: Vec<Artist>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Artist {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub artwork: Option<Artwork>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArtistDetail {
    pub artist: Artist,
    #[serde(default)]
    pub albums: Vec<Album>,
}

fn artwork_from_api(a: &serde_json::Value) -> Option<Artwork> {
    Some(Artwork {
        url: a
            .get("url")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        width: a.get("width").and_then(|n| n.as_u64()).map(|n| n as u32),
        height: a.get("height").and_then(|n| n.as_u64()).map(|n| n as u32),
    })
}

/// Parse one song resource object (`{id, attributes: {...}}`) into [`Track`].
pub fn parse_track_item(item: &serde_json::Value) -> Track {
    let attrs = item.get("attributes");
    Track {
        id: item
            .get("id")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        title: attrs
            .and_then(|a| a.get("name"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        artist: attrs
            .and_then(|a| a.get("artistName"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        album: attrs
            .and_then(|a| a.get("albumName"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        duration_ms: attrs
            .and_then(|a| a.get("durationInMillis"))
            .and_then(|n| n.as_u64()),
        artwork: attrs
            .and_then(|a| a.get("artwork"))
            .and_then(artwork_from_api),
        isrc: attrs
            .and_then(|a| a.get("isrc"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        genres: attrs
            .and_then(|a| a.get("genreNames"))
            .and_then(|g| g.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        preview_url: attrs
            .and_then(|a| a.get("previews"))
            .and_then(|p| p.as_array())
            .and_then(|arr| arr.first())
            .and_then(|p| p.get("url"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
    }
}

pub fn parse_album_item(item: &serde_json::Value) -> Album {
    let attrs = item.get("attributes");
    Album {
        id: item
            .get("id")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        title: attrs
            .and_then(|a| a.get("name"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        artist: attrs
            .and_then(|a| a.get("artistName"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        track_count: attrs
            .and_then(|a| a.get("trackCount"))
            .and_then(|n| n.as_u64())
            .map(|n| n as u32),
        artwork: attrs
            .and_then(|a| a.get("artwork"))
            .and_then(artwork_from_api),
        is_single: attrs
            .and_then(|a| a.get("isSingle"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        release_date: attrs
            .and_then(|a| a.get("releaseDate"))
            .and_then(|s| s.as_str())
            .map(str::to_string),
    }
}

pub fn parse_artist_item(item: &serde_json::Value) -> Artist {
    let attrs = item.get("attributes");
    Artist {
        id: item
            .get("id")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        name: attrs
            .and_then(|a| a.get("name"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        genres: attrs
            .and_then(|a| a.get("genreNames"))
            .and_then(|g| g.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        artwork: attrs
            .and_then(|a| a.get("artwork"))
            .and_then(artwork_from_api),
    }
}

pub fn parse_playlist_item(item: &serde_json::Value) -> Playlist {
    let attrs = item.get("attributes");
    Playlist {
        id: item
            .get("id")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        name: attrs
            .and_then(|a| a.get("name").or_else(|| a.get("title")))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        track_count: attrs
            .and_then(|a| a.get("trackCount"))
            .and_then(|n| n.as_u64())
            .map(|n| n as u32),
        artwork: attrs
            .and_then(|a| a.get("artwork"))
            .and_then(artwork_from_api),
        description: attrs
            .and_then(|a| a.get("description"))
            .and_then(|d| d.get("standard").or_else(|| d.get("short")))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
    }
}

/// Item objects inside search/chart nodes, which look like either
/// `{"data": [...]}` or `[{"chart": ..., "data": [...]}, ...]`.
fn items_in(node: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(arr) = node.get("data").and_then(|d| d.as_array()) {
        return arr.iter().collect();
    }
    if let Some(arr) = node.as_array() {
        let mut out = Vec::new();
        for entry in arr {
            if let Some(data) = entry.get("data").and_then(|d| d.as_array()) {
                out.extend(data.iter());
            }
        }
        return out;
    }
    Vec::new()
}

/// Tracks inside `relationships.tracks.data` (album/playlist detail).
pub fn parse_relationship_tracks(item: &serde_json::Value) -> Vec<Track> {
    item.get("relationships")
        .and_then(|r| r.get("tracks"))
        .and_then(|t| t.get("data"))
        .and_then(|d| d.as_array())
        .map(|arr| arr.iter().map(parse_track_item).collect())
        .unwrap_or_default()
}

/// Parse `GET /v1/catalog/{storefront}/search` response into our models.
/// Only depends on documented Apple Music API shape, no network needed.
pub fn parse_search_response(json: &serde_json::Value) -> SearchResults {
    let mut out = SearchResults::default();
    let results = match json.get("results") {
        Some(r) => r,
        None => return out,
    };
    if let Some(songs) = results.get("songs") {
        out.tracks = items_in(songs)
            .iter()
            .map(|i| parse_track_item(i))
            .collect();
    }
    if let Some(albums) = results.get("albums") {
        out.albums = items_in(albums)
            .iter()
            .map(|i| parse_album_item(i))
            .collect();
    }
    if let Some(pls) = results.get("playlists") {
        out.playlists = items_in(pls)
            .iter()
            .map(|i| parse_playlist_item(i))
            .collect();
    }
    if let Some(artists) = results.get("artists") {
        out.artists = items_in(artists)
            .iter()
            .map(|i| parse_artist_item(i))
            .collect();
    }
    out
}

/// Parse `GET /v1/catalog/{storefront}/charts` (same item shapes as search,
/// nested under chart objects).
pub fn parse_charts_response(json: &serde_json::Value) -> SearchResults {
    parse_search_response(json)
}

/// Parse album detail (`?include=tracks`): first `data` entry + its tracks.
pub fn parse_album_detail(json: &serde_json::Value) -> Option<AlbumDetail> {
    let item = json.get("data")?.as_array()?.first()?;
    Some(AlbumDetail {
        album: parse_album_item(item),
        tracks: parse_relationship_tracks(item),
    })
}

/// Parse artist detail (`?include=albums`): first `data` entry + its albums.
/// Apple sometimes repeats an album inside the relationship, so ids are
/// deduped here (first occurrence wins); the paged fetch in `get_artist`
/// dedupes against this list as well.
pub fn parse_artist_detail(json: &serde_json::Value) -> Option<ArtistDetail> {
    let item = json.get("data")?.as_array()?.first()?;
    let albums: Vec<Album> = item
        .get("relationships")
        .and_then(|r| r.get("albums"))
        .and_then(|t| t.get("data"))
        .and_then(|d| d.as_array())
        .map(|arr| arr.iter().map(parse_album_item).collect())
        .unwrap_or_default();
    Some(ArtistDetail {
        artist: parse_artist_item(item),
        albums: dedupe_albums(albums),
    })
}

/// Drop repeat album entries by id, preserving order (first wins).
/// Items without an id are always kept.
pub fn dedupe_albums(albums: Vec<Album>) -> Vec<Album> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(albums.len());
    for a in albums {
        if !a.id.is_empty() && !seen.insert(a.id.clone()) {
            continue;
        }
        out.push(a);
    }
    out
}

/// One page of `GET /v1/catalog/{storefront}/artists/{id}/albums`:
/// album items plus the top-level `next` page URL, if any.
pub fn parse_artist_albums_page(json: &serde_json::Value) -> (Vec<Album>, Option<String>) {
    let albums = json
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| arr.iter().map(parse_album_item).collect())
        .unwrap_or_default();
    let next = json
        .get("next")
        .and_then(|n| n.as_str())
        .map(str::to_string);
    (albums, next)
}

/// Parse playlist detail (`?include=tracks`): first `data` entry + its tracks.
pub fn parse_playlist_detail(json: &serde_json::Value) -> Option<PlaylistDetail> {
    let item = json.get("data")?.as_array()?.first()?;
    Some(PlaylistDetail {
        playlist: parse_playlist_item(item),
        tracks: parse_relationship_tracks(item),
    })
}

/// Parse `GET /v1/me/library/playlists`.
pub fn parse_library_playlists(json: &serde_json::Value) -> Vec<Playlist> {
    json.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| arr.iter().map(parse_playlist_item).collect())
        .unwrap_or_default()
}

/// One timed lyric word (milliseconds from track start).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LyricWord {
    pub ms: u64,
    #[serde(default)]
    pub end_ms: Option<u64>,
    pub text: String,
}

/// One timed lyric line (milliseconds from track start).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LyricLine {
    pub ms: u64,
    #[serde(default)]
    pub end_ms: Option<u64>,
    pub text: String,
    #[serde(default)]
    pub words: Vec<LyricWord>,
    /// Background / parenthetical vocal (`ttm:role="x-bg"`).
    #[serde(default)]
    pub bg: bool,
    /// Singer agent id from TTML (`ttm:agent="v1"`).
    #[serde(default)]
    pub agent: Option<String>,
}

/// Lyrics payload: plain display text plus optional line timings
/// (line-level from LRCLIB; Apple TTML may include word-level spans).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Lyrics {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub lines: Vec<LyricLine>,
    #[serde(default)]
    pub synced: bool,
    /// Debug hint: `apple-syllable`, `apple-line`, `lrclib-graft`, `lrclib`, `plain`.
    #[serde(default)]
    pub source: String,
}

/// Total timed word/syllable spans across all lines.
pub fn lyrics_word_count(lyrics: &Lyrics) -> usize {
    lyrics.lines.iter().map(|l| l.words.len()).sum()
}

/// Lines that contain more than one timed span (karaoke-style).
pub fn lyrics_karaoke_lines(lyrics: &Lyrics) -> usize {
    lyrics.lines.iter().filter(|l| l.words.len() > 1).count()
}

/// Pick the best catalog search hit for lyrics / id resolution.
pub fn pick_best_track_match<'a>(
    tracks: &'a [Track],
    title: &str,
    artist: &str,
) -> Option<&'a Track> {
    let title_l = title.trim().to_lowercase();
    let artist_l = artist.trim().to_lowercase();
    for t in tracks {
        let tt = t.title.to_lowercase();
        let ta = t.artist.to_lowercase();
        let title_ok =
            title_l.is_empty() || tt == title_l || tt.contains(&title_l) || title_l.contains(&tt);
        let artist_ok = artist_l.is_empty()
            || ta.contains(&artist_l)
            || artist_l.contains(&ta)
            || artist_l
                .split(" feat")
                .next()
                .is_some_and(|a| ta.contains(a.trim()));
        if title_ok && artist_ok {
            return Some(t);
        }
    }
    tracks.first()
}

/// Parse `[mm:ss.xx]` timestamp tags. A line may carry several leading tags
/// (same text at each time); metadata tags (`[ar:…]`, `[ti:…]`) end parsing
/// for that line. Returns lines sorted by time.
pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for line in lrc.lines() {
        let mut rest = line.trim_start();
        let mut times = Vec::new();
        let mut meta = false;
        loop {
            if !rest.starts_with('[') {
                break;
            }
            let Some(close) = rest.find(']') else { break };
            let tag = &rest[1..close];
            match parse_lrc_tag(tag) {
                Some(ms) => times.push(ms),
                None => {
                    // Non-timestamp tag: metadata line, ignore whole line.
                    meta = true;
                    break;
                }
            }
            rest = rest[close + 1..].trim_start();
        }
        if meta || times.is_empty() {
            continue;
        }
        let text = rest.trim();
        if text.is_empty() {
            continue;
        }
        for ms in times {
            out.push(LyricLine {
                ms,
                end_ms: None,
                text: text.to_string(),
                words: Vec::new(),
                bg: false,
                agent: None,
            });
        }
    }
    out.sort_by_key(|l| l.ms);
    out
}

fn parse_lrc_tag(tag: &str) -> Option<u64> {
    // mm:ss[.xx[x]]
    let (mm, rest) = tag.split_once(':')?;
    if mm.len() != 2 || !mm.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (ss, frac) = match rest.split_once('.') {
        Some((s, f)) => (s, f),
        None => (rest, ""),
    };
    if ss.len() != 2 || !ss.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.is_empty() && (frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let minutes: u64 = mm.parse().ok()?;
    let seconds: u64 = ss.parse().ok()?;
    let millis: u64 = match frac.len() {
        0 => 0,
        1 => frac.parse::<u64>().ok()? * 100,
        2 => frac.parse::<u64>().ok()? * 10,
        3 => frac.parse::<u64>().ok()?,
        _ => return None,
    };
    Some((minutes * 60 + seconds) * 1000 + millis)
}

/// Strip `[mm:ss.xx]` (and `[mm:ss]`) timestamp tags from LRC text.
/// Non-timestamp tags like `[ar:Someone]` are kept.
pub fn strip_lrc_timestamps(lrc: &str) -> String {
    let mut out = String::new();
    for line in lrc.lines() {
        let mut s = line.to_string();
        while let Some(open) = s.find('[') {
            let close = match s[open..].find(']') {
                Some(r) => open + r,
                None => break,
            };
            // `[` and `]` are ASCII, so byte indices are char boundaries.
            let tag = &s[open + 1..close];
            let b = tag.as_bytes();
            let is_ts = b.len() >= 4
                && b[0].is_ascii_digit()
                && b[1].is_ascii_digit()
                && b.get(2) == Some(&b':');
            if is_ts {
                s.replace_range(open..=close, "");
            } else {
                break;
            }
        }
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(trimmed);
        }
    }
    out
}

/// Build a [`Lyrics`] value from a TTML document string.
pub fn lyrics_from_ttml_str(ttml: &str) -> Option<Lyrics> {
    let s = ttml.trim();
    if s.is_empty() || !looks_like_ttml(s) {
        return None;
    }
    let lines = parse_ttml(s);
    let text = if lines.is_empty() {
        strip_xml_tags(s)
    } else {
        lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    if text.is_empty() {
        return None;
    }
    Some(Lyrics {
        text,
        synced: !lines.is_empty(),
        lines,
        source: String::new(),
    })
}

/// Best-effort lyrics extraction from a song-lyrics response.
/// Prefers TTML (timed `<p begin>` lines) so the UI can highlight.
/// `None` means "no lyrics here".
fn lyrics_from_ttml_value(v: &serde_json::Value) -> Option<Lyrics> {
    if let Some(s) = v.as_str() {
        return lyrics_from_ttml_str(s);
    }
    if let Some(map) = v.as_object() {
        let mut best: Option<Lyrics> = None;
        for val in map.values() {
            if let Some(lyrics) = lyrics_from_ttml_value(val) {
                let score = lyrics_word_count(&lyrics);
                let best_score = best.as_ref().map(lyrics_word_count).unwrap_or(0);
                if score >= best_score {
                    best = Some(lyrics);
                }
            }
        }
        return best;
    }
    None
}

fn scan_attrs_for_ttml(attrs: &serde_json::Value) -> Option<Lyrics> {
    let mut best: Option<Lyrics> = None;
    if let Some(obj) = attrs.as_object() {
        for (key, val) in obj {
            if !key.to_ascii_lowercase().contains("ttml") {
                continue;
            }
            if let Some(lyrics) = lyrics_from_ttml_value(val) {
                let score = lyrics_word_count(&lyrics);
                let best_score = best.as_ref().map(lyrics_word_count).unwrap_or(0);
                if score >= best_score {
                    best = Some(lyrics);
                }
            }
        }
    }
    best
}

pub fn parse_lyrics(json: &serde_json::Value) -> Option<Lyrics> {
    let item = json.get("data")?.as_array()?.first()?;
    let attrs = item.get("attributes")?;
    if let Some(lyrics) = scan_attrs_for_ttml(attrs) {
        return Some(lyrics);
    }
    for key in ["text", "lyrics", "content"] {
        if let Some(s) = attrs.get(key).and_then(|v| v.as_str()) {
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            if let Some(lyrics) = lyrics_from_ttml_str(s) {
                return Some(lyrics);
            }
            return Some(Lyrics {
                text: s.to_string(),
                lines: Vec::new(),
                synced: false,
                source: String::new(),
            });
        }
    }
    None
}

fn looks_like_ttml(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with('<')
        && (find_bytes_ci(t, b"<p").is_some()
            || find_bytes_ci(t, b"<tt").is_some()
            || t.to_ascii_lowercase().contains("ttml"))
}

fn find_bytes_ci(hay: &str, pat: &[u8]) -> Option<usize> {
    if pat.is_empty() {
        return None;
    }
    let hay_b = hay.as_bytes();
    hay_b.windows(pat.len()).position(|w| {
        w.iter()
            .zip(pat.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Next opening `<span` (not `</span>`) from the start of `hay`.
fn find_next_open_span(hay: &str) -> Option<usize> {
    let bytes = hay.as_bytes();
    let mut pos = 0usize;
    while pos < hay.len() {
        let rel = find_bytes_ci(&hay[pos..], b"<span")?;
        let abs = pos + rel;
        if abs > 0 && bytes[abs - 1] == b'/' {
            pos = abs + 5;
            continue;
        }
        return Some(rel);
    }
    None
}

/// Line- and word-level timings from Apple TTML (`<p begin="…">` with optional `<span>` words).
pub fn parse_ttml(ttml: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ttml.len() {
        let Some(rel) = find_bytes_ci(&ttml[i..], b"<p") else {
            break;
        };
        let start = i + rel;
        let after_p = start + 2;
        let Some(gt) = ttml[after_p..].find('>') else {
            break;
        };
        let tag = &ttml[after_p..after_p + gt];
        let content_at = after_p + gt + 1;
        if tag.ends_with('/') {
            i = content_at;
            continue;
        }
        let Some(end_rel) = find_bytes_ci(&ttml[content_at..], b"</p>") else {
            break;
        };
        let inner = &ttml[content_at..content_at + end_rel];
        i = content_at + end_rel + 4;
        if let Some(ms) = ttml_attr(tag, "begin").and_then(|b| parse_ttml_time(&b)) {
            let end_ms = ttml_attr(tag, "end").and_then(|b| parse_ttml_time(&b));
            let bg = ttml_attr_ci(tag, "role").as_deref() == Some("x-bg");
            let agent = ttml_attr_ci(tag, "agent");
            let words = parse_ttml_words(inner, ms, end_ms);
            let text = if words.is_empty() {
                strip_xml_tags(inner)
            } else {
                join_word_texts(&words)
            };
            if text.is_empty() {
                continue;
            }
            out.push(LyricLine {
                ms,
                end_ms,
                text,
                words,
                bg,
                agent,
            });
        }
    }
    out.sort_by_key(|l| l.ms);
    out
}

/// Join timed word texts for display (Apple often omits inter-span whitespace).
fn join_word_texts(words: &[LyricWord]) -> String {
    let mut out = String::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            let prev = words[i - 1].text.as_str();
            let cur = w.text.as_str();
            if !prev.is_empty()
                && !cur.is_empty()
                && !prev.ends_with(' ')
                && !cur.starts_with(' ')
                && prev.trim() == prev
                && cur.trim() == cur
            {
                out.push(' ');
            }
        }
        out.push_str(&w.text);
    }
    out
}

/// Word spans inside a TTML `<p>` (plain text chunks + nested `<span begin/end>`).
fn parse_ttml_words(inner: &str, p_begin: u64, p_end: Option<u64>) -> Vec<LyricWord> {
    let words = collect_ttml_words(inner, p_begin, p_end);
    if words.is_empty() {
        let text = strip_xml_tags(inner);
        if text.is_empty() {
            return words;
        }
        return vec![LyricWord {
            ms: p_begin,
            end_ms: p_end,
            text,
        }];
    }
    words
}

fn collect_ttml_words(s: &str, default_ms: u64, default_end: Option<u64>) -> Vec<LyricWord> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut pending = String::new();
    let mut cursor = default_ms;

    while i < s.len() {
        let Some(rel) = find_next_open_span(&s[i..]) else {
            pending.push_str(&strip_xml_tags_preserve(&s[i..]));
            break;
        };
        let abs = i + rel;
        pending.push_str(&strip_xml_tags_preserve(&s[i..abs]));
        let after_span = abs + 5;
        let Some(gt) = s[after_span..].find('>') else {
            break;
        };
        let tag = &s[after_span..after_span + gt];
        let content_start = after_span + gt + 1;
        if tag.ends_with('/') {
            i = content_start;
            continue;
        }
        let Some(close_rel) = find_matching_close_span(&s[content_start..]) else {
            break;
        };
        let span_body = &s[content_start..content_start + close_rel];
        i = content_start + close_rel + 7;

        let nested = find_next_open_span(span_body).is_some();
        let span_begin = ttml_attr(tag, "begin").and_then(|b| parse_ttml_time(&b));
        let span_end = ttml_attr(tag, "end").and_then(|b| parse_ttml_time(&b));

        if nested && span_begin.is_none() {
            let mut inner_words = collect_ttml_words(span_body, cursor, default_end);
            if !pending.is_empty() {
                if let Some(first) = inner_words.first_mut() {
                    first.text = format!("{}{}", pending, first.text);
                }
                pending.clear();
            }
            out.extend(inner_words);
            if let Some(last) = out.last() {
                cursor = last.end_ms.unwrap_or(last.ms);
            }
            continue;
        }

        if !pending.is_empty() {
            let pre_begin = span_begin.or(Some(default_ms));
            out.push(LyricWord {
                ms: default_ms,
                end_ms: pre_begin,
                text: pending.clone(),
            });
            pending.clear();
        }

        let text = if nested {
            join_word_texts(&collect_ttml_words(
                span_body,
                span_begin.unwrap_or(cursor),
                span_end.or(default_end),
            ))
        } else {
            strip_xml_tags_preserve(span_body)
        };
        if text.is_empty() {
            continue;
        }
        let ms = span_begin.unwrap_or(cursor);
        let end = span_end;
        cursor = end.unwrap_or(ms);
        out.push(LyricWord {
            ms,
            end_ms: end,
            text,
        });
    }

    if !pending.is_empty() {
        out.push(LyricWord {
            ms: cursor,
            end_ms: default_end,
            text: pending,
        });
    }
    out
}

/// Index of the `</span>` that closes the span whose content begins at `s`.
fn find_matching_close_span(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = 0;
    while i < s.len() {
        let close_rel = find_bytes_ci(&s[i..], b"</span>");
        let open_rel = find_next_open_span(&s[i..]);
        let handle_close = match (close_rel, open_rel) {
            (Some(c), Some(o)) => c <= o,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return None,
        };
        if handle_close {
            let rel = close_rel.expect("close_rel");
            depth -= 1;
            if depth == 0 {
                return Some(i + rel);
            }
            i += rel + 7;
        } else {
            let rel = open_rel.expect("open_rel");
            depth += 1;
            i += rel + 5;
        }
    }
    None
}

fn ttml_attr_ci(tag: &str, name: &str) -> Option<String> {
    let needle = name.to_ascii_lowercase();
    let lower = tag.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(i) = lower[search_from..].find(&needle) {
        let abs = search_from + i;
        let after = tag.get(abs + needle.len()..)?;
        let rest = after.trim_start();
        if !rest.starts_with('=') {
            search_from = abs + 1;
            continue;
        }
        let rest = rest[1..].trim_start();
        let q = rest.chars().next()?;
        if q != '"' && q != '\'' {
            search_from = abs + 1;
            continue;
        }
        let rest = &rest[q.len_utf8()..];
        let end = rest.find(q)?;
        return Some(rest[..end].to_string());
    }
    None
}

fn ttml_attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let i = tag.find(&key)?;
    let rest = tag.get(i + key.len()..)?;
    let q = rest.chars().next()?;
    if q != '"' && q != '\'' {
        return None;
    }
    let rest = &rest[q.len_utf8()..];
    let end = rest.find(q)?;
    Some(rest[..end].to_string())
}

/// TTML `begin`: `12.5`, `12.5s`, `00:12.34`, `0:00:12.340`.
fn parse_ttml_time(raw: &str) -> Option<u64> {
    let s = raw.trim().trim_end_matches('s').trim();
    if s.is_empty() {
        return None;
    }
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        let (hours, minutes, sec) = match parts.as_slice() {
            [m, sec] => (0u64, m.parse::<u64>().ok()?, *sec),
            [h, m, sec] => (h.parse::<u64>().ok()?, m.parse::<u64>().ok()?, *sec),
            _ => return None,
        };
        let seconds: f64 = sec.parse().ok()?;
        Some(((hours * 3600 + minutes * 60) as f64 * 1000.0 + seconds * 1000.0).round() as u64)
    } else {
        let seconds: f64 = s.parse().ok()?;
        Some((seconds * 1000.0).round() as u64)
    }
}

fn strip_xml_tags(s: &str) -> String {
    decode_basic_entities(collapse_ws(&strip_xml_raw(s)))
}

/// Strip tags but keep interior spacing (for TTML word chunks).
fn strip_xml_tags_preserve(s: &str) -> String {
    decode_basic_entities(strip_xml_raw(s))
}

fn strip_xml_raw(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !space && !out.is_empty() {
                out.push(' ');
                space = true;
            }
        } else {
            space = false;
            out.push(c);
        }
    }
    out.trim().to_string()
}

fn decode_basic_entities(s: String) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_response() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":{"songs":{"data":[{"id":"1","attributes":{"name":"T","artistName":"A","albumName":"Al","durationInMillis":200000}}]},"albums":{"data":[]},"playlists":{"data":[]}}}"#,
        )
        .unwrap();
        let r = parse_search_response(&v);
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.tracks[0].title, "T");
        assert!(r.tracks[0].preview_url.is_none());
    }

    #[test]
    fn parses_track_genres() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":{"songs":{"data":[{"id":"1","attributes":{"name":"T","genreNames":["Ambient","Electronic"]}}]}}}"#,
        )
        .unwrap();
        let r = parse_search_response(&v);
        assert_eq!(
            r.tracks[0].genres,
            vec!["Ambient".to_string(), "Electronic".to_string()]
        );
        let v2: serde_json::Value = serde_json::from_str(
            r#"{"results":{"songs":{"data":[{"id":"2","attributes":{"name":"U"}}]}}}"#,
        )
        .unwrap();
        assert!(parse_search_response(&v2).tracks[0].genres.is_empty());
    }

    #[test]
    fn parses_preview_url() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":{"songs":{"data":[{"id":"9","attributes":{"name":"T","previews":[{"url":"https://example.invalid/p.m4a"}]}}]},"albums":{"data":[]},"playlists":{"data":[]}}}"#,
        )
        .unwrap();
        let r = parse_search_response(&v);
        assert_eq!(
            r.tracks[0].preview_url.as_deref(),
            Some("https://example.invalid/p.m4a")
        );
    }

    #[test]
    fn parses_charts_shape() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":{"songs":[{"chart":"most-played","data":[{"id":"1","attributes":{"name":"Hit"}}]}]}}"#,
        )
        .unwrap();
        let r = parse_charts_response(&v);
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.tracks[0].title, "Hit");
    }

    #[test]
    fn parses_album_detail() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"id":"a1","attributes":{"name":"Al","artistName":"Ar"},"relationships":{"tracks":{"data":[{"id":"t1","attributes":{"name":"Song"}}]}}}]}"#,
        )
        .unwrap();
        let d = parse_album_detail(&v).unwrap();
        assert_eq!(d.album.title, "Al");
        assert_eq!(d.tracks.len(), 1);
        assert_eq!(d.tracks[0].id, "t1");
    }

    #[test]
    fn parses_artists_and_detail() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":{"artists":{"data":[{"id":"a9","attributes":{"name":"Singer","genreNames":["Pop"]}}]}}}"#,
        )
        .unwrap();
        let r = parse_search_response(&v);
        assert_eq!(r.artists.len(), 1);
        assert_eq!(r.artists[0].genres, vec!["Pop".to_string()]);
        let d: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"id":"a9","attributes":{"name":"Singer"},"relationships":{"albums":{"data":[{"id":"al1","attributes":{"name":"Hits"}}]}}}]}"#,
        )
        .unwrap();
        let det = parse_artist_detail(&d).unwrap();
        assert_eq!(det.albums.len(), 1);
        assert_eq!(det.albums[0].title, "Hits");
    }

    #[test]
    fn artist_detail_dedupes_repeat_albums() {
        let d: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"id":"a9","attributes":{"name":"Singer"},"relationships":{"albums":{"data":[
                {"id":"al1","attributes":{"name":"Hits"}},
                {"id":"al1","attributes":{"name":"Hits"}},
                {"id":"al2","attributes":{"name":"More"}}
            ]}}}]}"#,
        )
        .unwrap();
        let det = parse_artist_detail(&d).unwrap();
        let ids: Vec<&str> = det.albums.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["al1", "al2"]);
    }

    #[test]
    fn parses_library_playlists() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"id":"p.1","type":"library-playlists","attributes":{"name":"Mine"}}]}"#,
        )
        .unwrap();
        let pls = parse_library_playlists(&v);
        assert_eq!(pls.len(), 1);
        assert_eq!(pls[0].name, "Mine");
    }

    #[test]
    fn strips_lrc_timestamps() {
        let lrc = "[00:12.34] hello\n[01:02] world\n[ar:Someone] kept\nplain";
        assert_eq!(
            strip_lrc_timestamps(lrc),
            "hello\nworld\n[ar:Someone] kept\nplain"
        );
    }

    #[test]
    fn parses_lrc_timestamps() {
        let lrc = "[00:12.34] hello\n[01:02] world\n[ar:x] skip\n[00:05.00][00:06.00] twice";
        let lines = parse_lrc(lrc);
        assert_eq!(
            lines,
            vec![
                LyricLine {
                    ms: 5000,
                    end_ms: None,
                    text: "twice".into(),
                    words: vec![],
                    bg: false,
                    agent: None
                },
                LyricLine {
                    ms: 6000,
                    end_ms: None,
                    text: "twice".into(),
                    words: vec![],
                    bg: false,
                    agent: None
                },
                LyricLine {
                    ms: 12340,
                    end_ms: None,
                    text: "hello".into(),
                    words: vec![],
                    bg: false,
                    agent: None
                },
                LyricLine {
                    ms: 62000,
                    end_ms: None,
                    text: "world".into(),
                    words: vec![],
                    bg: false,
                    agent: None
                },
            ]
        );
    }

    #[test]
    fn parses_lyrics_text() {
        let v = serde_json::json!({"data":[{"attributes":{"text":"line1\nline2"}}]});
        let lyrics = parse_lyrics(&v).expect("text lyrics");
        assert_eq!(lyrics.text, "line1\nline2");
        assert!(lyrics.lines.is_empty());
        let empty = serde_json::json!({"data":[]});
        assert!(parse_lyrics(&empty).is_none());
    }

    #[test]
    fn parses_apple_ttml_timings() {
        let ttml = r#"<?xml version="1.0"?><tt xmlns="http://www.w3.org/ns/ttml"><body>
            <p begin="0.064" end="4.314">I been <span begin="1.200" end="3.800">waiting</span></p>
            <p begin="00:04.314">(yeah, yeah)</p>
            <p begin="0:00:08.5">last</p>
        </body></tt>"#;
        let lines = parse_ttml(ttml);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].ms, 64);
        assert_eq!(lines[0].end_ms, Some(4314));
        assert_eq!(lines[0].text, "I been waiting");
        assert_eq!(lines[0].words.len(), 2);
        assert_eq!(lines[0].words[0].ms, 64);
        assert_eq!(lines[0].words[0].end_ms, Some(1200));
        assert_eq!(lines[0].words[0].text, "I been ");
        assert_eq!(lines[0].words[1].ms, 1200);
        assert_eq!(lines[0].words[1].end_ms, Some(3800));
        assert_eq!(lines[0].words[1].text, "waiting");
        assert_eq!(lines[1].ms, 4314);
        assert_eq!(lines[1].text, "(yeah, yeah)");
        assert!(lines[1].words.len() == 1);
        assert_eq!(lines[2].ms, 8500);
        let v = serde_json::json!({"data":[{"attributes":{"ttml": ttml}}]});
        let lyrics = parse_lyrics(&v).expect("ttml lyrics");
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines.len(), 3);
        assert_eq!(lyrics.lines[0].ms, 64);
        assert!(!lyrics.lines[0].words.is_empty());
    }

    #[test]
    fn nested_xbg_spans_keep_inner_timings() {
        let inner = r#"<span ttm:role="x-bg"><span begin="10.0" end="10.5">(I </span><span begin="10.5" end="11.0">might)</span></span>"#;
        let words = parse_ttml_words(inner, 0, Some(12000));
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].ms, 10000);
        assert_eq!(words[0].text, "(I ");
        assert_eq!(words[1].ms, 10500);
        assert_eq!(words[1].text, "might)");
        assert!(
            words[0].ms >= 10000,
            "bg word must not inherit parent begin"
        );
    }

    #[test]
    fn parses_ttml_agent_on_line() {
        let ttml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body>
            <p begin="1.0" ttm:agent="v1">left line</p>
            <p begin="2.0" ttm:agent="v2">right line</p>
        </body></tt>"#;
        let lines = parse_ttml(ttml);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].agent.as_deref(), Some("v1"));
        assert_eq!(lines[1].agent.as_deref(), Some("v2"));
    }

    #[test]
    fn joins_ttml_words_without_inter_span_whitespace() {
        let inner = r#"<span begin="0.0" end="0.2">All</span><span begin="0.2" end="0.4">my</span><span begin="0.4" end="0.6">brothers</span>"#;
        let words = parse_ttml_words(inner, 0, Some(600));
        assert_eq!(words.len(), 3);
        assert_eq!(join_word_texts(&words), "All my brothers");
    }

    #[test]
    fn parses_ttml_localizations_map() {
        let ttml =
            r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><p begin="1.0">hi</p></body></tt>"#;
        let v = serde_json::json!({
            "data": [{"attributes": {"ttmlLocalizations": {"en-US": ttml}}}]
        });
        let lyrics = parse_lyrics(&v).expect("localized ttml");
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].ms, 1000);
    }

    #[test]
    fn pick_best_track_match_prefers_title_and_artist() {
        let tracks = vec![
            Track {
                id: "1".into(),
                title: "Other".into(),
                artist: "X".into(),
                ..Default::default()
            },
            Track {
                id: "2".into(),
                title: "Secondhand".into(),
                artist: "Justine Skye".into(),
                ..Default::default()
            },
        ];
        let hit = pick_best_track_match(&tracks, "Secondhand", "Justine Skye feat. Rema");
        assert_eq!(hit.map(|t| t.id.as_str()), Some("2"));
    }

    #[test]
    fn empty_without_results() {
        let v = serde_json::json!({});
        assert!(parse_search_response(&v).tracks.is_empty());
    }

    #[test]
    fn parses_album_single_flag_and_release_date() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"id":"s1","attributes":{"name":"Fresh Drop","artistName":"Singer","isSingle":true,"releaseDate":"2026-09-04","trackCount":1}}"#,
        )
        .unwrap();
        let a = parse_album_item(&v);
        assert!(a.is_single);
        assert_eq!(a.release_date.as_deref(), Some("2026-09-04"));
        assert_eq!(
            release_kind(a.is_single, a.track_count, &a.title),
            ReleaseKind::Single
        );
    }

    #[test]
    fn release_kind_classifies_eps_and_albums() {
        use ReleaseKind::{Album as L, Ep, Single as S};
        assert_eq!(release_kind(false, Some(1), "Lone"), S);
        assert_eq!(release_kind(false, Some(4), "Short One"), Ep);
        assert_eq!(release_kind(false, None, "Something - EP"), Ep);
        assert_eq!(release_kind(false, Some(12), "Long Play"), L);
        assert_eq!(release_kind(false, None, "Mystery"), L);
        assert_eq!(release_kind(true, Some(3), "Multi-track single"), S);
    }

    #[test]
    fn sorts_albums_newest_first() {
        let mut albums = vec![
            Album {
                id: "old".into(),
                release_date: Some("2020-01-01".into()),
                ..Default::default()
            },
            Album {
                id: "undated".into(),
                ..Default::default()
            },
            Album {
                id: "new".into(),
                release_date: Some("2026-09-04".into()),
                ..Default::default()
            },
        ];
        sort_albums_newest_first(&mut albums);
        let ids: Vec<&str> = albums.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "old", "undated"]);
    }

    #[test]
    fn parses_artist_albums_page_with_next() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"id":"al1","attributes":{"name":"Hits","isSingle":false}}],"next":"/v1/catalog/us/artists/a9/albums?offset=100"}"#,
        )
        .unwrap();
        let (albums, next) = parse_artist_albums_page(&v);
        assert_eq!(albums.len(), 1);
        assert!(!albums[0].is_single);
        assert_eq!(
            next.as_deref(),
            Some("/v1/catalog/us/artists/a9/albums?offset=100")
        );
        let (empty, none) = parse_artist_albums_page(&serde_json::json!({}));
        assert!(empty.is_empty() && none.is_none());
    }
}
