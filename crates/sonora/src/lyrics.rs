//! Timed lyrics engine — Apple-style line + word-level karaoke sync,
//! ported 1:1 from the old web UI's timed-lyrics section (`ui/app.js`).
//!
//! The engine is pure state: it owns the fetched payload for one track,
//! the continuation-aware backing-vocal flags, the active/primary line
//! bookkeeping (including the hold-through-gap rule), per-word karaoke
//! fill, focus (±2) classes, duet sides and the caption. Views render
//! [`WordView`] strips and ask for [`LineClass`]es; the position clock
//! lives in [`crate::player::PosClock`].
//!
//! DOM-parity notes: the old UI styled bracket runs ("(ooh)") smaller
//! and threaded unclosed parens across words *and* lines; both are
//! ported here as per-segment flags. Scrolling-to-active is a view
//! concern; this module only reports the primary line index.

use apple_music_core::models::{LyricLine, LyricWord, Lyrics};

/// Focused mode keeps lines within this distance of the center visible
/// as "near"; farther lines dim to "far" (old `.lyr-line.near/.far`).
pub const FOCUS_RADIUS: usize = 2;

/// How one line should render right now (old class toggles on the line
/// element: `active`, `near`, `far`, or none of them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineClass {
    /// In the active time window (or held through a gap).
    Active,
    /// Not active but within [`FOCUS_RADIUS`] of the focus center.
    Near,
    /// Focused mode, outside the near window.
    Far,
    /// Plain (unfocused list, or active-less full text).
    Plain,
}

/// Duet styling side: TTML agents alternate left/right (odd → left).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// One rendered text segment of a line: a timed karaoke word or a plain
/// whitespace token. `bracket` marks parenthetical content (smaller,
/// dimmer — old `.lyr-bracket`), `fill` is the karaoke fill percent
/// (only ever non-zero for timed words of the active line).
#[derive(Clone, Debug)]
pub struct WordView {
    pub text: String,
    pub bracket: bool,
    pub fill: f32,
}

/// The lyrics state for one track: payload + sync bookkeeping.
#[derive(Debug, Default)]
pub struct LyricEngine {
    /// Track these lyrics belong to (target even while the fetch is
    /// pending, like the old `lyric.trackId`).
    track_id: Option<String>,
    title: String,
    artist: String,
    lyrics: Lyrics,
    /// Track duration — the window for the last line/word when the
    /// provider sends no explicit end (old `current.duration_ms`).
    duration_ms: Option<u64>,
    /// Continuation-aware backing-vocal flags, cached per load (old
    /// `lyricBgFlags`): explicit `bg`, bracket-only lines, and lines
    /// that start inside an unclosed parenthetical.
    bg_flags: Vec<bool>,
    /// Active line indices for the current position (old `lyricActiveSet`).
    active: Vec<usize>,
    /// The line the view centers on (old `lyricPrimary`).
    primary: Option<usize>,
    /// Line kept active through gaps between main lines (old
    /// `lyricHoldMain`): the last main line we passed, never the final
    /// one (nothing to hold "toward" at the end of the song).
    hold_main: Option<usize>,
    /// Some line carries timed words (old `lyricHasKaraoke`).
    has_karaoke: bool,
    /// Last fetch error, shown when there is nothing else to display.
    error: Option<String>,
}

fn line_ms(l: &LyricLine) -> u64 {
    l.ms
}

/// A line that is only a parenthetical, e.g. "(ooh)", is backing-vocal
/// style even when the provider didn't flag it as `bg` (LRCLIB/plain).
fn is_bracket_only_line(text: &str) -> bool {
    let t = text.trim();
    t.len() > 2 && t.starts_with('(') && t.ends_with(')')
}

fn is_bg_line(l: &LyricLine) -> bool {
    l.bg || is_bracket_only_line(&l.text)
}

fn word_ms(w: &LyricWord) -> u64 {
    w.ms
}

/// End of one timed word: explicit end wins, else the next word's start,
/// else the line end, else a flat 400ms guess (old `wordEndMs`).
fn word_end_ms(w: &LyricWord, line: &LyricLine, wi: usize) -> u64 {
    let start = word_ms(w);
    if let Some(e) = w.end_ms {
        if e > start {
            return e;
        }
    }
    if let Some(next) = line.words.get(wi + 1) {
        return next.ms;
    }
    if let Some(e) = line.end_ms {
        if e > start {
            return e;
        }
    }
    start + 400
}

/// Walk one line's characters (timed words in order, else the plain
/// text) for paren depth. `f(segment_index, starts_inside_paren)` is
/// called once per segment before its characters update the depth;
/// returns the depth left open at end of line (carried into the next).
fn walk_paren(line: &LyricLine, in_paren: bool, mut f: impl FnMut(usize, bool)) -> bool {
    let mut paren = in_paren;
    if !line.words.is_empty() {
        for (i, w) in line.words.iter().enumerate() {
            f(i, paren);
            for ch in w.text.chars() {
                if ch == '(' {
                    paren = true;
                } else if ch == ')' {
                    paren = false;
                }
            }
        }
    } else {
        for (i, tok) in line.text.split_whitespace().enumerate() {
            f(i, paren);
            for ch in tok.chars() {
                if ch == '(' {
                    paren = true;
                } else if ch == ')' {
                    paren = false;
                }
            }
        }
    }
    paren
}

/// TTML agent id → duet side: odd numbers left, even right (old
/// `lyricAgentSide`); no/invalid agent gets no side.
fn agent_side(agent: &str) -> Option<Side> {
    let n: usize = agent
        .strip_prefix(['v', 'V'])
        .unwrap_or(agent)
        .parse()
        .ok()?;
    if n < 1 {
        return None;
    }
    Some(if n % 2 == 1 { Side::Left } else { Side::Right })
}

impl LyricEngine {
    /// Retarget at a new track: everything resets, nothing is kept (old
    /// `lyric = {trackId, title, artist, lines: [], ...}` on change).
    pub fn set_target(&mut self, id: &str, title: &str, artist: &str) {
        *self = LyricEngine {
            track_id: Some(id.to_owned()),
            title: title.to_owned(),
            artist: artist.to_owned(),
            ..Default::default()
        };
    }

    /// Land a fetched payload for the current target and recompute the
    /// derived caches (old `applyLyricsResult` + `renderLyrics` setup).
    pub fn set_lyrics(&mut self, lyrics: Lyrics) {
        self.has_karaoke = lyrics.lines.iter().any(|l| l.words.len() > 1);
        self.bg_flags = compute_bg_flags(&lyrics.lines);
        self.lyrics = lyrics;
        self.error = None;
        self.reset_highlight();
    }

    /// Land a fetch failure: no lines, message kept for the view (old
    /// autoFetch failure path reset the lyric and re-rendered empty).
    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
        self.lyrics = Lyrics::default();
        self.bg_flags.clear();
        self.has_karaoke = false;
        self.reset_highlight();
    }

    /// Track duration, for the open-ended last window (old
    /// `current.duration_ms` feeding `lineEndMs`/`bgEndMs`).
    pub fn set_duration(&mut self, ms: Option<u64>) {
        self.duration_ms = ms;
    }

    pub fn track_id(&self) -> Option<&str> {
        self.track_id.as_deref()
    }

    /// True when this engine already holds usable lyrics for `id` (old
    /// `lyric.trackId === t.id && (lines.length || text)`).
    pub fn holds(&self, id: &str) -> bool {
        self.track_id.as_deref() == Some(id)
            && (!self.lyrics.lines.is_empty() || !self.lyrics.text.is_empty())
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn artist(&self) -> &str {
        &self.artist
    }

    pub fn is_synced(&self) -> bool {
        !self.lyrics.lines.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.lyrics.text
    }

    pub fn source(&self) -> &str {
        &self.lyrics.source
    }

    pub fn lines(&self) -> &[LyricLine] {
        &self.lyrics.lines
    }

    pub fn has_karaoke(&self) -> bool {
        self.has_karaoke
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn line_count(&self) -> usize {
        self.lyrics.lines.len()
    }

    pub fn is_bg(&self, i: usize) -> bool {
        self.bg_flags
            .get(i)
            .copied()
            .unwrap_or_else(|| self.lyrics.lines.get(i).is_some_and(is_bg_line))
    }

    /// More than one distinct TTML agent → duet styling (old buildLyricList).
    pub fn duet(&self) -> bool {
        let mut agents: Vec<&str> = Vec::new();
        for l in &self.lyrics.lines {
            if let Some(a) = l.agent.as_deref() {
                if !a.is_empty() && !agents.contains(&a) {
                    agents.push(a);
                }
            }
        }
        agents.len() > 1
    }

    /// Side for a duet line, if it names an agent.
    pub fn agent_side(&self, i: usize) -> Option<Side> {
        self.lyrics
            .lines
            .get(i)?
            .agent
            .as_deref()
            .and_then(agent_side)
    }

    // -- timing windows ---------------------------------------------------

    /// End of a main line: explicit end wins, else the next line's start,
    /// else the track duration (old `lineEndMs`).
    fn line_end_ms(&self, i: usize) -> u64 {
        let Some(l) = self.lyrics.lines.get(i) else {
            return 0;
        };
        if let Some(e) = l.end_ms {
            if e > line_ms(l) {
                return e;
            }
        }
        if let Some(next) = self.lyrics.lines.get(i + 1) {
            return line_ms(next);
        }
        self.duration_ms.unwrap_or(u64::MAX)
    }

    fn line_in_range(&self, i: usize, pos: u64) -> bool {
        let Some(l) = self.lyrics.lines.get(i) else {
            return false;
        };
        pos >= line_ms(l) && pos < self.line_end_ms(i)
    }

    /// Backing-vocal window: explicit end_ms wins; otherwise run until
    /// the next line with a strictly later timestamp (same-ms main+bg
    /// pairs share a timestamp, so the immediate next line must not end
    /// the bg line) — old `bgEndMs`.
    fn bg_end_ms(&self, i: usize) -> u64 {
        let Some(l) = self.lyrics.lines.get(i) else {
            return 0;
        };
        let start = line_ms(l);
        if let Some(e) = l.end_ms {
            if e > start {
                return e;
            }
        }
        for j in (i + 1)..self.lyrics.lines.len() {
            let ms = line_ms(&self.lyrics.lines[j]);
            if ms > start {
                return ms;
            }
        }
        self.duration_ms.unwrap_or(u64::MAX)
    }

    /// Is line `i` inside its active window at `pos`? (old
    /// `lyricLineActiveAt`: bg lines use the bg window, others the main
    /// line range.)
    pub fn line_active_at(&self, i: usize, pos: u64) -> bool {
        let Some(l) = self.lyrics.lines.get(i) else {
            return false;
        };
        if self.is_bg(i) {
            pos >= line_ms(l) && pos < self.bg_end_ms(i)
        } else {
            self.line_in_range(i, pos)
        }
    }

    // -- active / primary bookkeeping -------------------------------------

    /// Last non-bg line (the song's final "main" line — old
    /// `lastMainLyricIndex`).
    fn last_main_index(&self) -> Option<usize> {
        self.lyrics
            .lines
            .iter()
            .enumerate()
            .rev()
            .find(|(i, _)| !self.is_bg(*i))
            .map(|(i, _)| i)
    }

    /// Last non-bg line that has already started at `pos` (old
    /// `mainLyricIndexBeforePos`).
    fn main_index_before(&self, pos: u64) -> Option<usize> {
        let mut idx = None;
        for (i, l) in self.lyrics.lines.iter().enumerate() {
            if line_ms(l) > pos {
                break;
            }
            if !self.is_bg(i) {
                idx = Some(i);
            }
        }
        idx
    }

    /// Active set + primary for a position, before the gap-hold pass
    /// (old `computeActiveLyricIndices`): every in-window line; primary
    /// is the highest non-bg active, falling back to any active.
    fn compute_active(&self, pos: u64) -> (Vec<usize>, Option<usize>) {
        let mut active: Vec<usize> = Vec::new();
        let mut primary: Option<usize> = None;
        for i in 0..self.lyrics.lines.len() {
            if self.line_active_at(i, pos) {
                active.push(i);
                // Primary: the last non-bg line; a backing line claims it
                // only when no main line has yet.
                if !self.is_bg(i) || primary.is_none() {
                    primary = Some(i);
                }
            }
        }
        (active, primary)
    }

    /// Keep the last main line highlighted through the gap before the
    /// next line starts — but never hold the final main line (nothing
    /// follows it) — old `applyLyricHoldThroughGap`.
    fn apply_hold(
        &mut self,
        mut active: Vec<usize>,
        primary: Option<usize>,
        pos: u64,
    ) -> (Vec<usize>, Option<usize>) {
        let main_in_active = active.iter().copied().filter(|i| !self.is_bg(*i)).max();
        let before = self.main_index_before(pos);
        if before < self.hold_main {
            self.hold_main = before;
        }
        if let Some(p) = main_in_active {
            self.hold_main = Some(p);
            return (active, Some(p));
        }
        let last_main = self.last_main_index();
        if let (Some(h), Some(b)) = (self.hold_main, before) {
            if Some(h) != last_main && b == h && pos >= self.lyrics.lines.get(h).map_or(0, line_ms)
            {
                active.push(h);
                return (active, Some(h));
            }
        }
        (active, primary)
    }

    /// Is line `i` held (fully lit) through a gap right now? Old
    /// `heldThroughGap` in `applyWordFill`.
    pub fn held_through_gap(&self, i: usize, pos: u64) -> bool {
        self.hold_main == Some(i)
            && self.active.contains(&i)
            && !self.is_bg(i)
            && !self.line_active_at(i, pos)
    }

    /// Recompute active/primary for `pos`. Returns true when the set or
    /// primary changed (the old UI used the change to scroll + caption).
    pub fn update(&mut self, pos: u64) -> bool {
        if self.lyrics.lines.is_empty() {
            let changed = !self.active.is_empty() || self.primary.is_some();
            self.reset_highlight();
            return changed;
        }
        let (active, primary) = self.compute_active(pos);
        let (active, primary) = self.apply_hold(active, primary, pos);
        let changed = self.active != active || self.primary != primary;
        self.active = active;
        self.primary = primary;
        changed
    }

    /// Drop the highlight (new track, seek to 0 — old resetLyricHighlight;
    /// `hold_main` survives, matching `lyricHoldMain`).
    pub fn reset_highlight(&mut self) {
        self.active.clear();
        self.primary = None;
    }

    pub fn active(&self) -> &[usize] {
        &self.active
    }

    pub fn primary(&self) -> Option<usize> {
        self.primary
    }

    /// Center line for focused mode: the primary when one is active,
    /// else the last main line passed, else line 0 (old lyricFocusCenter).
    pub fn focus_center(&self, pos: u64) -> usize {
        if let Some(p) = self.primary {
            return p;
        }
        self.main_index_before(pos).unwrap_or(0)
    }

    /// Render class for a line in the current highlight state.
    pub fn line_class(&self, i: usize, focused: bool, center: usize) -> LineClass {
        if self.active.contains(&i) {
            return LineClass::Active;
        }
        let dist = i.abs_diff(center);
        if !focused {
            return LineClass::Plain;
        }
        if dist <= FOCUS_RADIUS {
            LineClass::Near
        } else {
            LineClass::Far
        }
    }

    // -- words ------------------------------------------------------------

    /// Per-segment views for one line: timed karaoke words (fill set by
    /// [`Self::word_fill`]) or whitespace tokens of a plain line, each
    /// flagged for parenthetical styling (old buildLyricLineContent +
    /// lyricHTMLWithState's depth threading). A word counts as bracket
    /// content when the paren is already open at its start or the word
    /// itself opens one — matching the old per-character styling at
    /// word granularity.
    pub fn line_segments(&self, i: usize) -> Vec<WordView> {
        let Some(l) = self.lyrics.lines.get(i) else {
            return Vec::new();
        };
        let mut paren = false;
        for j in 0..i {
            let line = &self.lyrics.lines[j];
            paren = walk_paren(line, paren, |_, _| {});
        }
        let mut out: Vec<WordView> = Vec::new();
        walk_paren(l, paren, |si, inside| {
            let text = if l.words.is_empty() {
                l.text.split_whitespace().nth(si).unwrap_or("").to_owned()
            } else {
                l.words[si].text.clone()
            };
            out.push(WordView {
                bracket: inside || text.contains('('),
                text,
                fill: 0.0,
            });
        });
        out
    }

    /// Karaoke fill (0..=100) for word `wi` of line `i` at `pos`: held
    /// lines are fully lit, out-of-window words are empty, in-window
    /// words ramp between their spans (old `applyWordFill`).
    pub fn word_fill(&self, i: usize, wi: usize, pos: u64) -> f32 {
        if !self.has_karaoke {
            return 0.0;
        }
        let Some(l) = self.lyrics.lines.get(i) else {
            return 0.0;
        };
        let Some(w) = l.words.get(wi) else {
            return 0.0;
        };
        if self.held_through_gap(i, pos) {
            return 100.0;
        }
        if !self.line_active_at(i, pos) {
            return 0.0;
        }
        let start = word_ms(w);
        let end = word_end_ms(w, l, wi);
        if pos >= end {
            100.0
        } else if pos > start && end > start {
            (((pos - start) as f32 / (end - start) as f32) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        }
    }

    /// Caption under the lyrics list (old `lyricCaption`): mode, live
    /// line position, provider source. Empty when there are no lines.
    pub fn caption(&self, live: bool, focused: bool) -> String {
        if self.lyrics.lines.is_empty() {
            return String::new();
        }
        let n = self.lyrics.lines.len();
        let mode = if focused { "focused ±2" } else { "full text" };
        let where_ = if live {
            match self.primary {
                Some(p) => format!("live · line {}/{n}", p + 1),
                None => format!("live · line —/{n}"),
            }
        } else {
            "not the playing track — play it to follow".to_owned()
        };
        let src = if self.lyrics.source.is_empty() {
            String::new()
        } else {
            format!(" · {}", self.lyrics.source)
        };
        format!("live synced lyrics · {mode} · {where_}{src}")
    }

    /// Log summary (old dlog on applyLyricsResult).
    pub fn summary(&self) -> String {
        let words: usize = self.lyrics.lines.iter().map(|l| l.words.len()).sum();
        let karaoke = self
            .lyrics
            .lines
            .iter()
            .filter(|l| l.words.len() > 1)
            .count();
        format!(
            "{} lines, {words} words, {karaoke} karaoke lines · {}",
            self.lyrics.lines.len(),
            if self.lyrics.source.is_empty() {
                "?".to_owned()
            } else {
                self.lyrics.source.clone()
            }
        )
    }
}

/// Continuation-aware bg flags for a whole payload (old
/// `computeLyricBgFlags`): a line counts as backing vocal when flagged,
/// when it is bracket-only, or when it starts inside a parenthetical a
/// previous line left open.
fn compute_bg_flags(lines: &[LyricLine]) -> Vec<bool> {
    let mut flags = Vec::with_capacity(lines.len());
    let mut paren = false;
    for l in lines {
        flags.push(paren || is_bg_line(l));
        paren = walk_paren(l, paren, |_, _| {});
    }
    flags
}

// ---------------------------------------------------------------------------
// Fetch/target orchestration around the engine — the old
// `fetchLyrics`/`lyricsCache`/`autoFetchLyrics`/`openLyrics` cluster.
// ---------------------------------------------------------------------------

use std::collections::HashMap;

use crate::backend::{Backend, Pending};

/// What the views should render for the current target right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LyricState {
    /// Fetch in flight (old "Loading…").
    Loading,
    /// Synced line data (render the karaoke list).
    Lines,
    /// Plain, untimed text (render as paragraphs).
    Text,
    /// Nothing available: message for "No lyrics available (…)".
    Error(String),
}

/// Lyrics state for the app: the engine plus the session cache, the
/// single in-flight fetch, and the audible-track bookkeeping that keeps
/// auto-fetch reacting to track CHANGES only (so a menu-pinned target
/// survives until playback actually moves on).
#[derive(Default)]
pub struct LyricsUi {
    pub engine: LyricEngine,
    /// Session cache by track id (old `lyricsCache`).
    cache: HashMap<String, Lyrics>,
    /// In-flight `get_lyrics` (old `lyricsFetchInFlight` dedupe).
    pub fetch: Pending<Lyrics>,
    /// Track the in-flight fetch serves.
    fetching: Option<String>,
    /// Audible track seen by the last tick (retarget-on-change guard).
    last_playing: Option<String>,
}

impl LyricsUi {
    /// Audible-track tick (old `autoFetchLyrics`): fires once per track
    /// change — later frames never clobber a menu-pinned target.
    pub fn note_playing(
        &mut self,
        ctx: &egui::Context,
        backend: &Backend,
        id: &str,
        title: &str,
        artist: &str,
    ) {
        if self.last_playing.as_deref() == Some(id) {
            return;
        }
        self.last_playing = Some(id.to_owned());
        self.ensure(ctx, backend, id, title, artist);
    }

    /// The engine no longer has an audible track to follow.
    pub fn forget_playing(&mut self) {
        self.last_playing = None;
    }

    /// Track-menu "Lyrics": show this track's lyrics, fetching if the
    /// cache has nothing (old `openLyrics` — an already-held payload
    /// renders instantly).
    pub fn open_for(
        &mut self,
        ctx: &egui::Context,
        backend: &Backend,
        id: &str,
        title: &str,
        artist: &str,
    ) {
        self.ensure(ctx, backend, id, title, artist);
    }

    /// Make `id` the shown target: cached payloads land immediately,
    /// otherwise one fetch starts (deduped while it is the target).
    fn ensure(
        &mut self,
        ctx: &egui::Context,
        backend: &Backend,
        id: &str,
        title: &str,
        artist: &str,
    ) {
        if self.engine.holds(id) || self.fetching.as_deref() == Some(id) {
            return;
        }
        if let Some(res) = self.cache.get(id).cloned() {
            self.engine.set_target(id, title, artist);
            self.engine.set_lyrics(res);
            return;
        }
        self.start_fetch(ctx, backend, id, title, artist);
    }

    fn start_fetch(
        &mut self,
        ctx: &egui::Context,
        backend: &Backend,
        id: &str,
        title: &str,
        artist: &str,
    ) {
        // Retarget first: the body shows the loading state for exactly
        // this track while the call runs.
        self.engine.set_target(id, title, artist);
        self.fetching = Some(id.to_owned());
        let state = backend.state.clone();
        let (song, artist, title) = (id.to_owned(), artist.to_owned(), title.to_owned());
        let rx = backend.call(ctx, async move {
            shell::commands::get_lyrics(&state, song, artist, title).await
        });
        self.fetch.start(rx);
    }

    /// Poll the in-flight fetch once per frame; returns a log line when
    /// it settles (old fetchLyrics `.then` handlers).
    pub fn tick_fetch(&mut self) -> Option<String> {
        self.fetch.poll();
        if self.fetch.busy() {
            return None;
        }
        let id = self.fetching.take()?;
        if let Some(res) = self.fetch.value.take() {
            let words: usize = res.lines.iter().map(|l| l.words.len()).sum();
            let src = if res.source.is_empty() {
                "?"
            } else {
                &res.source
            };
            let line = format!(
                "lyrics: {id} — {} lines, {words} words · {src}",
                res.lines.len()
            );
            if self.engine.track_id() == Some(id.as_str()) {
                self.engine.set_lyrics(res.clone());
            }
            self.cache.insert(id, res);
            return Some(line);
        }
        if let Some(err) = self.fetch.error.take() {
            if self.engine.track_id() == Some(id.as_str()) {
                self.engine.set_error(err.clone());
            }
            return Some(format!("lyrics failed ({id}): {err}"));
        }
        None
    }

    /// Advance the engine for this frame against the audible track:
    /// live only when the target IS the playing one, otherwise the
    /// highlight clears (old `highlightLyric` guarded on `current`).
    /// Returns true when the active line/primary changed.
    pub fn position(&mut self, playing_id: Option<&str>, pos: u64) -> bool {
        if playing_id.is_some() && self.engine.track_id() == playing_id {
            return self.engine.update(pos);
        }
        let changed = !self.engine.active().is_empty() || self.engine.primary().is_some();
        if changed {
            self.engine.reset_highlight();
        }
        changed
    }

    /// The engine's target is the audible track (caption "live", fill
    /// driven by the position clock).
    pub fn live(&self, playing_id: Option<&str>) -> bool {
        playing_id.is_some() && self.engine.track_id() == playing_id
    }

    /// Track duration for the open-ended last lyric window (old
    /// `current.duration_ms` feeding lineEndMs/bgEndMs).
    pub fn set_duration(&mut self, ms: u64) {
        self.engine.set_duration((ms > 0).then_some(ms));
    }

    /// What to render for the current target.
    pub fn state(&self) -> LyricState {
        if !self.engine.lines().is_empty() {
            LyricState::Lines
        } else if !self.engine.text().is_empty() {
            LyricState::Text
        } else if let Some(e) = self.engine.error() {
            LyricState::Error(e.to_owned())
        } else {
            LyricState::Loading
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ms: u64, text: &str) -> LyricLine {
        LyricLine {
            ms,
            end_ms: None,
            text: text.to_owned(),
            words: Vec::new(),
            bg: false,
            agent: None,
        }
    }

    fn words_line(ms: u64, spans: &[(&str, u64)]) -> LyricLine {
        LyricLine {
            ms,
            end_ms: None,
            text: String::new(),
            words: spans
                .iter()
                .map(|(t, wms)| LyricWord {
                    ms: *wms,
                    end_ms: None,
                    text: (*t).to_owned(),
                })
                .collect(),
            bg: false,
            agent: None,
        }
    }

    fn engine(lines: Vec<LyricLine>, dur: u64) -> LyricEngine {
        let mut e = LyricEngine::default();
        e.set_target("t1", "Song", "Artist");
        e.set_duration(Some(dur));
        e.set_lyrics(Lyrics {
            text: String::new(),
            lines,
            synced: true,
            source: "test".into(),
        });
        e
    }

    #[test]
    fn active_window_and_primary() {
        let mut e = engine(
            vec![line(0, "one"), line(1000, "two"), line(2000, "three")],
            5000,
        );
        assert!(e.update(0));
        assert_eq!(e.active(), &[0]);
        assert_eq!(e.primary(), Some(0));
        assert!(e.update(1500));
        assert_eq!(e.primary(), Some(1));
        // Same window → the change flag stays false (scroll/caption dedupe).
        assert!(!e.update(1800));
        assert_eq!(e.primary(), Some(1));
        // Past the last line nothing stays lit.
        assert!(e.update(2500));
        assert_eq!(e.primary(), Some(2));
    }

    #[test]
    fn bracket_only_line_is_bg() {
        let e = engine(
            vec![line(0, "hey"), line(500, "(ooh)"), line(1000, "you")],
            5000,
        );
        assert!(!e.is_bg(0));
        assert!(e.is_bg(1));
        assert!(!e.is_bg(2));
    }

    #[test]
    fn split_parenthetical_continuation_is_bg() {
        let e = engine(
            vec![
                line(0, "(I"),
                line(500, "love you)"),
                line(1000, "straight"),
            ],
            5000,
        );
        // "(I" is only 2 chars — not bracket-only by the old >2 rule, so
        // the opening line stays main; the continuation is bg.
        assert!(!e.is_bg(0));
        assert!(e.is_bg(1), "line continuing the unclosed paren is bg");
        assert!(!e.is_bg(2));
    }

    #[test]
    fn same_ms_main_and_bg_pair_shares_primary() {
        let mut bg = line(1000, "(echo)");
        bg.bg = true;
        let mut e = engine(vec![line(0, "main"), bg, line(2000, "next")], 5000);
        // bg window runs [1000, 2000) (next strictly-later ms).
        e.update(1100);
        assert_eq!(e.active(), &[1]);
        assert_eq!(e.primary(), Some(1), "bg-only active falls back to it");
        e.update(300);
        assert_eq!(e.active(), &[0]);
        assert_eq!(e.primary(), Some(0));
    }

    #[test]
    fn hold_through_gap_lights_previous_line_full() {
        let mut l0 = line(0, "one");
        l0.end_ms = Some(800); // real gap before line 1 at 2000
        let mut e = engine(vec![l0, line(2000, "two"), line(4000, "three")], 9000);
        e.update(500);
        assert_eq!(e.primary(), Some(0));
        // 1200 sits in the gap: nothing is in range, but the last main
        // line we passed stays lit.
        e.update(1200);
        assert_eq!(e.primary(), Some(0), "held through the gap");
        assert!(e.held_through_gap(0, 1200));
        // The hold does not extend past the final main line.
        e.update(4500);
        assert_eq!(e.primary(), Some(2));
    }

    #[test]
    fn word_fill_ramps_between_spans() {
        let mut e = engine(
            vec![
                words_line(0, &[("hello", 0), ("world", 1000)]),
                line(3000, "bye"),
            ],
            9000,
        );
        e.update(0);
        assert_eq!(e.word_fill(0, 0, 0), 0.0);
        assert_eq!(e.word_fill(0, 0, 500), 50.0);
        assert!(
            (e.word_fill(0, 0, 999) - 99.9).abs() < 0.01,
            "ramps toward 100"
        );
        assert_eq!(e.word_fill(0, 1, 500), 0.0, "next word not started");
        // Last word has no next word and the line has no explicit end:
        // old wordEndMs falls back to start + 400.
        assert_eq!(e.word_fill(0, 1, 1200), 50.0);
        assert_eq!(e.word_fill(0, 1, 1500), 100.0);
        // Outside the line window nothing fills (old applyWordFill skips
        // inactive lines).
        e.update(3500);
        assert_eq!(e.word_fill(0, 1, 3500), 0.0);
    }

    #[test]
    fn focus_classes() {
        let e = engine((0..10).map(|i| line(i * 1000, "l")).collect(), 20000);
        // Unfocused: everything plain (unless active).
        assert_eq!(e.line_class(4, false, 4), LineClass::Plain);
        assert_eq!(e.line_class(4, true, 4), LineClass::Near);
        assert_eq!(e.line_class(2, true, 4), LineClass::Near);
        assert_eq!(e.line_class(0, true, 4), LineClass::Far);
    }

    #[test]
    fn duet_sides() {
        let mut l1 = line(0, "mine");
        l1.agent = Some("v1".into());
        let mut l2 = line(1000, "yours");
        l2.agent = Some("v2".into());
        let e = engine(vec![l1, l2], 5000);
        assert!(e.duet());
        assert_eq!(e.agent_side(0), Some(Side::Left));
        assert_eq!(e.agent_side(1), Some(Side::Right));
        assert_eq!(agent_side("v3"), Some(Side::Left));
        assert_eq!(agent_side("v0"), None);
        assert_eq!(agent_side(""), None);
    }

    #[test]
    fn caption_reports_live_position_and_source() {
        let mut e = engine(vec![line(0, "a"), line(1000, "b")], 5000);
        assert_eq!(
            caption_of(&e, false, false),
            "live synced lyrics · full text · not the playing track — play it to follow · test"
        );
        e.update(1200);
        assert_eq!(
            caption_of(&e, true, false),
            "live synced lyrics · full text · live · line 2/2 · test"
        );
        assert_eq!(
            caption_of(&e, true, true),
            "live synced lyrics · focused ±2 · live · line 2/2 · test"
        );
    }

    fn caption_of(e: &LyricEngine, live: bool, focused: bool) -> String {
        e.caption(live, focused)
    }

    #[test]
    fn plain_text_without_lines() {
        let mut e = LyricEngine::default();
        e.set_target("t1", "Song", "Artist");
        e.set_lyrics(Lyrics {
            text: "no timing here".into(),
            lines: Vec::new(),
            synced: false,
            source: "plain".into(),
        });
        assert!(!e.is_synced());
        assert_eq!(e.text(), "no timing here");
        assert!(!e.update(1234), "no lines → never active");
        assert_eq!(e.caption(true, false), "");
    }

    #[test]
    fn segments_flag_brackets_across_words_and_lines() {
        let mut e = engine(
            vec![words_line(0, &[("(I", 0), ("love", 200), ("you)", 500)])],
            3000,
        );
        let segs = e.line_segments(0);
        assert!(segs[0].bracket, "'(' opens the paren");
        assert!(segs[1].bracket, "inside the paren");
        assert!(segs[2].bracket, "closing ')' still styled");
        e = engine(vec![line(0, "(ooh) yes")], 3000);
        let segs = e.line_segments(0);
        assert!(segs[0].bracket && !segs[1].bracket);
    }
}
