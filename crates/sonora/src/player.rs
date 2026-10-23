//! Player status polling, transport actions, the UI-side queue mirror,
//! and the full play-chain ported from the old web UI: track-end advance,
//! loop-one replay, dead-id skipping, infinite-queue radio fill with
//! origin affinity + retry backoff, OS media-key queue jumps, and session
//! persistence (queue + position, restored paused on next launch).
//!
//! The backend never auto-advances: every end-of-track jump is driven
//! from here, exactly like the old `maybeAutoAdvance`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use apple_music_core::models::Track;
use apple_music_core::playback::QueueItem;
use shell::native_player::PlayerReport;

use crate::app::SonoraApp;
use crate::backend::Pending;
use crate::store;

/// Status poll cadence (matches the backend's 250ms publish loop).
const POLL: Duration = Duration::from_millis(250);
/// Discord progress refresh while playing.
const DISCORD_REFRESH: Duration = Duration::from_secs(15);
/// How many upcoming entries follow a play/jump in the backend queue
/// (old `MIRROR_AHEAD`): OS media-key next steps inside this window.
const MIRROR_AHEAD: usize = 25;
/// Track-end signature: playing → stopped with the playhead within this
/// window of the end (old `pos >= dur - 1500`).
const END_GRACE_MS: u64 = 1500;
/// Radio retry backoff: 15s → 30s → 60s cap (old scheduleRadioRetry).
const RETRY_BASE: Duration = Duration::from_secs(15);

/// One entry of the UI queue mirror: the backend item plus the UI-only
/// metadata the old web queue kept per entry (`{track, source}`).
#[derive(Clone, Debug)]
pub struct MirrorEntry {
    pub item: QueueItem,
    /// `source === 'autoplay'` — radio-filled, rendered "· radio".
    pub radio: bool,
    pub art: Option<String>,
    pub duration_ms: Option<u64>,
    /// Kept for radio origin affinity (the old origin stored genres).
    pub genres: Vec<String>,
}

/// Outcome of a `player_play`: the status message plus the id the engine
/// actually started (library ids resolve to catalog ids server-side).
pub struct PlayOutcome {
    pub track_id: Option<String>,
    pub message: String,
}

/// Fresh radio candidates for one fill, computed by the fetch task.
pub struct RadioFill {
    pub tracks: Vec<Track>,
    pub error: String,
}

fn norm_genre_key(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn norm_artist_key(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Infinite-queue origin: when playback starts from a playlist/album (a
/// multi-track queue), remember the full list's artists + genres so radio
/// fills stay true to the whole collection, not just the last few tracks.
/// Artist groups are rotated per fill so every artist gets coverage.
#[derive(Clone, Default)]
pub struct QueueOrigin {
    artist_keys: Vec<String>,
    genre_set: HashSet<String>,
    /// (artist key, track ids) in first-seen order.
    artist_groups: Vec<(String, Vec<String>)>,
}

impl QueueOrigin {
    /// Build from mirror entries; `None` for single-track queues (a
    /// one-song queue has no "whole collection" to stay true to).
    pub fn from_entries(entries: &[MirrorEntry]) -> Option<Self> {
        if entries.len() < 2 {
            return None;
        }
        let mut origin = QueueOrigin::default();
        let mut seen_ids: HashSet<&str> = HashSet::new();
        let mut artist_seen: HashSet<String> = HashSet::new();
        for e in entries.iter().take(200) {
            if e.item.id.is_empty() || !seen_ids.insert(e.item.id.as_str()) {
                continue;
            }
            let artist = e.item.artist.as_deref().unwrap_or("");
            let ak = norm_artist_key(artist);
            if !ak.is_empty() && artist_seen.insert(ak.clone()) {
                origin.artist_keys.push(ak.clone());
            }
            let gk = if ak.is_empty() {
                "(unknown)".to_string()
            } else {
                ak
            };
            match origin.artist_groups.iter_mut().find(|(k, _)| *k == gk) {
                Some((_, ids)) => ids.push(e.item.id.clone()),
                None => origin.artist_groups.push((gk, vec![e.item.id.clone()])),
            }
            for g in &e.genres {
                let k = norm_genre_key(g);
                if !k.is_empty() {
                    origin.genre_set.insert(k);
                }
            }
        }
        if seen_ids.len() < 2 {
            return None;
        }
        Some(origin)
    }

    /// Extra seeds covering the origin's full artist range, rotated by
    /// fill count (old `originCoverageSeeds`): one track per distinct
    /// artist, so successive fills favor different artists instead of
    /// always the playlist head.
    fn coverage_seeds(&self, recent: &HashSet<String>, fill_count: u32, n: usize) -> Vec<String> {
        if self.artist_groups.is_empty() {
            return Vec::new();
        }
        let start = (fill_count as usize) % self.artist_groups.len();
        let mut out: Vec<String> = Vec::new();
        for k in 0..self.artist_groups.len() {
            if out.len() >= n {
                break;
            }
            let (_, ids) = &self.artist_groups[(start + k) % self.artist_groups.len()];
            let pick = ids
                .iter()
                .find(|id| !recent.contains(*id) && !out.iter().any(|o| o == *id))
                .or_else(|| ids.iter().find(|id| !out.iter().any(|o| o == *id)));
            if let Some(p) = pick {
                out.push(p.clone());
            }
        }
        out
    }

    /// Union affinity: +2 for naming any origin artist, +1 per shared
    /// genre (old `playlistAffinity`). Union scoring keeps mixed
    /// playlists mixed — any branch of the collection scores well.
    fn affinity(&self, artist: &str, genres: &[String]) -> u32 {
        let mut s = 0;
        let a = norm_artist_key(artist);
        if !a.is_empty() {
            for oa in &self.artist_keys {
                if !oa.is_empty() && (a.contains(oa.as_str()) || oa.contains(a.as_str())) {
                    s += 2;
                    break;
                }
            }
        }
        for g in genres {
            if self.genre_set.contains(&norm_genre_key(g)) {
                s += 1;
            }
        }
        s
    }
}

pub struct PlayerUi {
    /// Latest status report from the engine.
    pub report: Option<PlayerReport>,
    /// UI mirror of the backend queue: play/append/play-next/clear keep
    /// both sides identical, so `player_next`/`player_previous` step the
    /// same list this view renders.
    pub queue: Vec<MirrorEntry>,
    /// Mirror index into `queue`.
    pub index: Option<usize>,
    /// Session gray set for store-dead ids (seeded from the backend's
    /// persisted set at boot, extended on unresolvable play failures).
    /// Dead entries render grayed and are never advanced into.
    pub dead: HashSet<String>,
    /// Artwork templates for tracks seen this session: the queue mirror
    /// keeps ids only, so the player bar resolves its cover through this
    /// map (populated from every list the UI plays from).
    pub art_by_track: HashMap<String, String>,
    /// In-flight `player_play` outcome (message → status, id adoption).
    pub play: Pending<PlayOutcome>,
    /// In-flight radio fill (fetch → append as autoplay entries).
    radio: Pending<RadioFill>,
    status: Pending<PlayerReport>,
    last_poll: Instant,
    was_playing: Option<bool>,
    last_track_id: Option<String>,
    last_detail: String,
    last_os_next: u64,
    last_os_prev: u64,
    /// Once-per-track end dedupe (old `trackEndHandled`).
    ended_handled: Option<String>,
    /// User pressed pause (old `userPaused`): no auto-advance on stop.
    user_paused: bool,
    /// Restored (position, duration) shown until the first report lands.
    pub restored: Option<(u64, u64)>,
    /// Playlist/album origin for radio affinity (old `queueOrigin`).
    pub origin: Option<QueueOrigin>,
    /// Origin fills so far (rotates the artist coverage seeds).
    radio_fill_count: u32,
    radio_fetching: bool,
    /// Consecutive dry fills per seed track: paginate deeper (max 8).
    radio_depth: HashMap<String, u32>,
    radio_retry_at: Option<Instant>,
    radio_retry_id: Option<String>,
    /// Track already reported as stalled (message once per track).
    radio_stall_for: Option<String>,
    /// Consecutive dry episodes; grows the retry backoff.
    radio_dry_streak: u32,
    last_radio_error: String,
    /// Last queue snapshot written to the session store.
    last_persist: Instant,
    discord_at: Option<Instant>,
    discord_key: Option<(String, bool)>,
    pub volume_applied: f32,
    notifications_applied: bool,
    discord_applied: bool,
    discord_app_id_applied: String,
    settings_bootstrapped: bool,
    pub seeking: bool,
}

impl Default for PlayerUi {
    fn default() -> Self {
        Self {
            report: None,
            queue: Vec::new(),
            index: None,
            dead: HashSet::new(),
            art_by_track: HashMap::new(),
            play: Pending::default(),
            radio: Pending::default(),
            status: Pending::default(),
            last_poll: Instant::now(),
            was_playing: None,
            last_track_id: None,
            last_detail: String::new(),
            last_os_next: 0,
            last_os_prev: 0,
            ended_handled: None,
            user_paused: false,
            restored: None,
            origin: None,
            radio_fill_count: 0,
            radio_fetching: false,
            radio_depth: HashMap::new(),
            radio_retry_at: None,
            radio_retry_id: None,
            radio_stall_for: None,
            radio_dry_streak: 0,
            last_radio_error: String::new(),
            last_persist: Instant::now(),
            discord_at: None,
            discord_key: None,
            volume_applied: f32::NAN,
            notifications_applied: false,
            discord_applied: false,
            discord_app_id_applied: String::new(),
            settings_bootstrapped: false,
            seeking: false,
        }
    }
}

impl PlayerUi {
    /// Remember artwork templates for these tracks (first seen wins — a
    /// session map of small strings, drained only by process exit).
    pub fn note_art(&mut self, tracks: &[Track]) {
        for t in tracks {
            if let Some(a) = &t.artwork {
                if !a.url.is_empty() {
                    self.art_by_track
                        .entry(t.id.clone())
                        .or_insert_with(|| a.url.clone());
                }
            }
        }
    }

    /// Learn a store-dead id for the session: gray it everywhere it
    /// shows and never advance into it (old `markDead`). The backend
    /// persists its own set on unresolvable play errors; this is the
    /// in-memory view of it.
    fn mark_dead(&mut self, id: &str, why: &str) {
        if id.trim().is_empty() || !self.dead.insert(id.to_string()) {
            return;
        }
        eprintln!("sonora: unplayable, graying: {id} ({why})");
    }

    /// Per-track fill depth: consecutive dry fills for the SAME track
    /// paginate backend windows deeper (old depthFor, max 8).
    fn depth_for(&self, id: &str) -> u32 {
        self.radio_depth.get(id).copied().unwrap_or(0).min(8)
    }

    fn bump_depth(&mut self, id: &str) -> u32 {
        let d = (self.radio_depth.get(id).copied().unwrap_or(0) + 1).min(8);
        self.radio_depth.insert(id.to_string(), d);
        if self.radio_depth.len() > 50 {
            if let Some(k) = self.radio_depth.keys().next().cloned() {
                self.radio_depth.remove(&k);
            }
        }
        d
    }

    /// Drop the pending radio retry (old cancelRadioRetry): any
    /// navigation, fresh play, clear, or toggle-off cancels it.
    pub fn cancel_radio_retry(&mut self) {
        self.radio_retry_at = None;
        self.radio_retry_id = None;
        self.radio_stall_for = None;
        self.radio_dry_streak = 0;
    }

    /// One pending retry while the queue sits exhausted with Infinite on
    /// (old scheduleRadioRetry): backs off 15s → 30s → 60s so a hopeless
    /// stall doesn't hammer the API.
    fn schedule_radio_retry(&mut self, current_id: Option<String>, infinite: bool) {
        if !infinite || self.radio_retry_at.is_some() {
            return;
        }
        let Some(id) = current_id else {
            return;
        };
        let delay =
            Duration::from_millis((RETRY_BASE.as_millis() as u64) << self.radio_dry_streak.min(2))
                .min(Duration::from_secs(60));
        self.radio_dry_streak += 1;
        self.radio_retry_at = Some(Instant::now() + delay);
        self.radio_retry_id = Some(id);
    }
}

impl SonoraApp {
    pub fn tick_player(&mut self, ctx: &egui::Context) {
        // Consume finished calls.
        self.player.status.poll();
        self.player.play.poll();
        self.player.radio.poll();
        if let Some(out) = self.player.play.value.take() {
            self.adopt_played_id(out);
        }
        if let Some(err) = self.player.play.error.take() {
            self.set_status(&err);
            self.log(err.clone());
            // Store-dead despite metadata: learn it (gray + skip) for
            // the session (old markDead on UNRESOLVABLE errors).
            if shell::commands::is_unresolvable_play_error(&err) {
                if let Some(cur) = self.current_id() {
                    self.player.mark_dead(&cur, "play failed");
                }
            }
        }
        if let Some(fill) = self.player.radio.value.take() {
            self.on_radio_fill(fill, ctx);
        }
        if let Some(err) = self.player.radio.error.take() {
            self.player.radio_fetching = false;
            self.log(format!("radio: {err}"));
        }

        // Surface fire-and-forget outcomes (settings toggles, playlist
        // mutations, auth actions …). Batch append errors name dead ids
        // inline: gray and drop them from the mirror.
        let notices = self.backend.take_notices();
        for msg in notices {
            self.drop_unresolved(&msg);
            self.set_status(&msg);
            self.log(msg);
        }

        // React to a fresh report.
        if let Some(report) = self.player.status.value.take() {
            self.on_report(report, ctx);
        }

        // Radio retry due (queue exhausted with Infinite on).
        if let Some(at) = self.player.radio_retry_at {
            if Instant::now() >= at {
                self.player.radio_retry_at = None;
                let same_track = self
                    .player
                    .radio_retry_id
                    .take()
                    .is_some_and(|id| self.current_id().as_deref() == Some(id.as_str()));
                if same_track && self.settings.infinite {
                    if self.player.user_paused {
                        self.player
                            .schedule_radio_retry(self.current_id(), self.settings.infinite);
                    } else {
                        self.maybe_fill_radio(ctx);
                    }
                }
            }
        }

        // Keep the poll chain alive at the 250ms cadence.
        if !self.player.status.busy() && self.player.status.value.is_none() {
            let since = self.player.last_poll.elapsed();
            if since >= POLL {
                let state = self.backend.state.clone();
                let rx =
                    self.backend.call(
                        ctx,
                        async move { shell::commands::player_status(&state).await },
                    );
                self.player.status.start(rx);
                self.player.last_poll = Instant::now();
            }
        }

        // Persist the mirror periodically while playing (old pollTick%40).
        if self
            .player
            .report
            .as_ref()
            .map(|r| r.playing)
            .unwrap_or(false)
            && self.player.last_persist.elapsed() >= Duration::from_secs(10)
        {
            self.persist_now();
        }

        // Push settings-derived integrations on change (+ once at boot).
        self.apply_integrations(ctx);

        // Throttled Discord progress refresh while playing.
        self.maybe_push_discord(ctx, false);

        ctx.request_repaint_after(POLL);
    }

    fn on_report(&mut self, report: PlayerReport, ctx: &egui::Context) {
        let tid = report.track_id.clone();
        let track_changed = tid != self.player.last_track_id;

        // Player detail line (resolution failures surface as activity).
        if !report.detail.is_empty() && report.detail != self.player.last_detail {
            self.player.last_detail = report.detail.clone();
            if report.detail.contains("could not be resolved") {
                self.log_dbg(format!("player: {}", report.detail));
            } else {
                self.set_status(format!("Player: {}", report.detail));
            }
        }

        if track_changed {
            self.player.ended_handled = None;
            if tid.is_some() {
                // Re-adopt the queue index for the now-playing track; a
                // storefront-resolved id swaps into the mirror entry so
                // later jumps and ends match on it (old adoptPlayedId).
                if let Some(i) = self.find_queue_index(&report) {
                    self.player.index = Some(i);
                    if let Some(tid) = &tid {
                        let old = self.player.queue[i].item.id.clone();
                        if &old != tid {
                            self.player.queue[i].item.id = tid.clone();
                            self.log_dbg(format!("adopted resolved id {old} → {tid}"));
                        }
                    }
                }
                if let Some(title) = report.title.clone() {
                    let artist = report.artist.clone().unwrap_or_default();
                    let line = if artist.is_empty() {
                        format!("▶ {title}")
                    } else {
                        format!("▶ {title} — {artist}")
                    };
                    self.log(line);
                }
                // Top the radio up when a new track lands (old
                // adoptExternalTrack called maybeFillRadio here).
                self.maybe_fill_radio(ctx);
            }
        }

        let playing_flipped = self.player.was_playing != Some(report.playing);
        if track_changed || playing_flipped {
            self.maybe_push_discord(ctx, true);
        }

        // OS media-key skips arrive as counter bumps (MPRIS), never as
        // blind backend commands — execute each as a UI queue jump, same
        // as the in-app buttons (old poll handler).
        let os_next = report.os_next;
        if os_next != self.player.last_os_next {
            let diff = os_next.saturating_sub(self.player.last_os_next);
            let delta = if (1..=8).contains(&diff) { diff } else { 1 };
            for _ in 0..delta {
                self.on_next(ctx);
            }
        }
        let os_prev = report.os_prev;
        if os_prev != self.player.last_os_prev {
            let diff = os_prev.saturating_sub(self.player.last_os_prev);
            let delta = if (1..=8).contains(&diff) { diff } else { 1 };
            for _ in 0..delta {
                self.on_prev(ctx);
            }
        }
        self.player.last_os_next = os_next;
        self.player.last_os_prev = os_prev;

        // Track end (old maybeAutoAdvance signature): was playing →
        // stopped on the same track with the playhead at the end. A
        // mid-track pause does not qualify, and a pending jump owns the
        // UI meanwhile.
        let ended = self.player.was_playing == Some(true)
            && !report.playing
            && !track_changed
            && tid.is_some()
            && report.duration_ms > 0
            && report.position_ms + END_GRACE_MS >= report.duration_ms
            && !self.player.user_paused
            && !self.player.play.busy()
            && self.current_id().as_deref() == tid.as_deref();
        let already = self.player.ended_handled.as_deref() == tid.as_deref() && tid.is_some();
        if ended && !already {
            self.player.ended_handled = tid.clone();
            self.on_track_end(ctx);
        }

        self.player.was_playing = Some(report.playing);
        self.player.last_track_id = tid;
        self.player.report = Some(report);
    }

    /// The mirror's current entry, if any.
    fn current_entry(&self) -> Option<&MirrorEntry> {
        self.player.index.and_then(|i| self.player.queue.get(i))
    }

    fn current_id(&self) -> Option<String> {
        self.current_entry().map(|e| e.item.id.clone())
    }

    /// Locate the queue entry for a report: exact id first (neighbors
    /// resolve duplicates directionally), then title/artist fallback for
    /// storefront-re-resolved ids. Returns `None` when nothing matches.
    fn find_queue_index(&self, report: &PlayerReport) -> Option<usize> {
        let tid = report.track_id.as_deref()?;
        let q = &self.player.queue;
        if let Some(cur) = self.player.index {
            if q.get(cur + 1).map(|e| e.item.id.as_str()) == Some(tid) {
                return Some(cur + 1);
            }
            if cur > 0 && q.get(cur - 1).map(|e| e.item.id.as_str()) == Some(tid) {
                return Some(cur - 1);
            }
        }
        if let Some(at) = q.iter().position(|e| e.item.id == tid) {
            return Some(at);
        }
        let title = report.title.as_deref()?.trim().to_lowercase();
        if title.is_empty() {
            return None;
        }
        let artist = report.artist.as_deref().unwrap_or("").trim().to_lowercase();
        q.iter().position(|e| {
            e.item
                .title
                .as_deref()
                .map(|t| t.trim().to_lowercase() == title)
                .unwrap_or(false)
                && e.item
                    .artist
                    .as_deref()
                    .map(|a| a.trim().to_lowercase() == artist)
                    .unwrap_or(true)
        })
    }

    /// Apply a `player_play` result: surface the message and, when the
    /// engine resolved a different id (library → catalog), swap it into
    /// the current mirror entry (old adoptPlayedId).
    fn adopt_played_id(&mut self, out: PlayOutcome) {
        self.set_status(out.message);
        let Some(pid) = out.track_id.filter(|p| !p.is_empty()) else {
            return;
        };
        let Some(i) = self.player.index else {
            return;
        };
        let old = self.player.queue[i].item.id.clone();
        if old != pid {
            self.player.queue[i].item.id = pid.clone();
            self.log_dbg(format!("play resolved {old} → {pid}"));
        }
    }

    fn is_dead(&self, e: &MirrorEntry) -> bool {
        self.player.dead.contains(&e.item.id)
    }

    /// Next playable queue position from the current index walking `dir`,
    /// skipping dead entries (old nextPlayableIndex). `None` when nothing
    /// in that direction is playable.
    fn next_playable(&self, dir: i64) -> Option<usize> {
        let base = self.player.index.map(|i| i as i64).unwrap_or(-1);
        let mut i = base + dir;
        while i >= 0 && (i as usize) < self.player.queue.len() {
            if !self.is_dead(&self.player.queue[i as usize]) {
                return Some(i as usize);
            }
            i += dir;
        }
        None
    }

    /// Batch append/resolve errors name the dead ids inline (old
    /// handleResolveError): gray them and drop them from the mirror.
    fn drop_unresolved(&mut self, text: &str) {
        const PHRASE: &str = "could not be resolved:";
        let lower = text.to_lowercase();
        let Some(pos) = lower.find(PHRASE) else {
            return;
        };
        let ids: Vec<String> = text[pos + PHRASE.len()..]
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .collect();
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            self.player.mark_dead(id, "unresolvable");
        }
        self.player.queue.retain(|e| !ids.contains(&e.item.id));
        if self.player.queue.is_empty() {
            self.player.index = None;
        } else if let Some(i) = self.player.index {
            self.player.index = Some(i.min(self.player.queue.len() - 1));
        }
        self.log_dbg(format!("dropped unresolvable: {}", ids.join(", ")));
    }

    /// End of the current track (old maybeAutoAdvance tail): loop-one
    /// replays it, else advance to the next playable entry; when the
    /// queue runs out, infinite mode fills from radio and retries.
    fn on_track_end(&mut self, ctx: &egui::Context) {
        if self.settings.loop_one {
            if let Some(i) = self.player.index {
                if let Some(title) = self.player.report.as_ref().and_then(|r| r.title.clone()) {
                    self.set_status(format!("Looping “{title}”"));
                }
                self.jump_to(ctx, i);
                return;
            }
        }
        if let Some(nx) = self.next_playable(1) {
            self.jump_to(ctx, nx);
            return;
        }
        self.maybe_fill_radio(ctx);
        if let Some(nx) = self.next_playable(1) {
            self.jump_to(ctx, nx);
            return;
        }
        if self.settings.infinite {
            // Stall: message once per track, retry with backoff instead
            // of stopping (old scheduleRadioRetry).
            let cur = self.current_id();
            if cur.is_some() && self.player.radio_stall_for != cur {
                self.player.radio_stall_for = cur.clone();
                let why = if self.player.last_radio_error.is_empty() {
                    "no similar songs found".to_string()
                } else {
                    self.player.last_radio_error.clone()
                };
                self.set_status(format!("Infinite queue: {why} — retrying"));
            }
            let cur = self.current_id();
            self.player
                .schedule_radio_retry(cur, self.settings.infinite);
        } else {
            self.set_status("Queue finished");
        }
    }

    /// After a fill lands while a track-end is waiting: jump to the next
    /// playable entry, or keep retrying (old scheduleRadioRetry body).
    fn advance_after_fill(&mut self, ctx: &egui::Context) {
        if let Some(nx) = self.next_playable(1) {
            self.jump_to(ctx, nx);
        } else if self.settings.infinite {
            let cur = self.current_id();
            self.player
                .schedule_radio_retry(cur, self.settings.infinite);
        }
    }

    /// Grow the queue from radio when it runs thin (old maybeFillRadio):
    /// one fill at a time, seeded from the current track plus recent
    /// history, bridging through already-queued results into fresh
    /// neighborhoods. Appends land as autoplay entries.
    pub fn maybe_fill_radio(&mut self, ctx: &egui::Context) {
        self.player.last_radio_error.clear();
        if !self.settings.infinite || self.player.radio_fetching || self.player.radio.busy() {
            return;
        }
        let Some(cur) = self.current_id() else {
            return;
        };
        let idx = self
            .player
            .index
            .unwrap_or(0)
            .min(self.player.queue.len().saturating_sub(1));
        let remaining = self.player.queue.len().saturating_sub(idx + 1);
        let user_remaining = self.player.queue[idx + 1..]
            .iter()
            .filter(|e| !e.radio)
            .count();
        if remaining > 2 && user_remaining > 1 {
            return;
        }
        let depth = self.player.depth_for(&cur);
        if depth > 0 {
            self.log_dbg(format!("radio: fill depth {depth} for {cur}"));
        }
        // Seeds: the current track plus recent history walking backwards
        // (up to 6), then origin coverage across the full artist range.
        let mut seeds: Vec<String> = Vec::new();
        let mut i = idx;
        loop {
            if let Some(id) = self.player.queue.get(i).map(|e| e.item.id.clone()) {
                if !id.is_empty() && !seeds.contains(&id) {
                    seeds.push(id);
                }
            }
            if seeds.len() >= 6 || i == 0 {
                break;
            }
            i -= 1;
        }
        if !seeds.contains(&cur) {
            seeds.insert(0, cur.clone());
        }
        if let Some(o) = &self.player.origin {
            let recent: HashSet<String> = seeds.iter().cloned().collect();
            for id in o.coverage_seeds(&recent, self.player.radio_fill_count, 8) {
                if !seeds.contains(&id) {
                    seeds.push(id);
                }
            }
        }
        let queued: HashSet<String> = self
            .player
            .queue
            .iter()
            .map(|e| e.item.id.clone())
            .collect();
        let dead = self.player.dead.clone();
        // Exclude what's already queued so the batch budget is spent on
        // genuinely fresh tracks.
        let len = self.player.queue.len();
        let exclude: Vec<String> = self
            .player
            .queue
            .iter()
            .skip(len.saturating_sub(200))
            .map(|e| e.item.id.clone())
            .collect();
        let origin = self.player.origin.clone();
        self.player.radio_fetching = true;
        let state = self.backend.state.clone();
        let rx = self.backend.call(ctx, async move {
            radio_fetch(state, seeds, queued, dead, depth, origin, exclude).await
        });
        self.player.radio.start(rx);
    }

    /// Append a successful radio fill as autoplay entries; dry fills
    /// deepen the next attempt's pagination and, when a track-end is
    /// waiting on the queue, advance or re-arm the retry.
    fn on_radio_fill(&mut self, fill: RadioFill, ctx: &egui::Context) {
        self.player.radio_fetching = false;
        if self.player.origin.is_some() {
            self.player.radio_fill_count += 1;
        }
        let cur = self.current_id();
        let mut added: Vec<MirrorEntry> = Vec::new();
        for t in fill.tracks {
            if t.id.is_empty() || t.unavailable || self.player.dead.contains(&t.id) {
                continue;
            }
            if self.player.queue.iter().any(|e| e.item.id == t.id) {
                continue;
            }
            added.push(MirrorEntry {
                item: QueueItem {
                    id: t.id.clone(),
                    kind: "song".into(),
                    title: (!t.title.is_empty()).then(|| t.title.clone()),
                    artist: (!t.artist.is_empty()).then(|| t.artist.clone()),
                },
                radio: true,
                art: t
                    .artwork
                    .as_ref()
                    .map(|a| a.url.clone())
                    .filter(|u| !u.is_empty()),
                duration_ms: t.duration_ms,
                genres: t.genres.clone(),
            });
        }
        let waiting = cur.is_some() && self.player.ended_handled == cur;
        if !added.is_empty() {
            self.log_dbg(format!("radio: +{} tracks", added.len()));
            if let Some(id) = &cur {
                self.player.radio_depth.remove(id);
            }
            let items: Vec<QueueItem> = added.iter().map(|e| e.item.clone()).collect();
            self.player.queue.extend(added);
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "radio append", async move {
                shell::commands::player_append(&state, items).await
            });
            self.persist_now();
            if waiting {
                self.advance_after_fill(ctx);
            }
        } else {
            if let Some(id) = &cur {
                self.player.last_radio_error = if fill.error.is_empty() {
                    "similar: none found for this song".to_string()
                } else {
                    fill.error
                };
                self.player.bump_depth(id);
            }
            if waiting && self.settings.infinite {
                if self.player.radio_stall_for != cur {
                    self.player.radio_stall_for = cur.clone();
                    let why = if self.player.last_radio_error.is_empty() {
                        "no similar songs found".to_string()
                    } else {
                        self.player.last_radio_error.clone()
                    };
                    self.set_status(format!("Infinite queue: {why} — retrying"));
                }
                let cur = self.current_id();
                self.player
                    .schedule_radio_retry(cur, self.settings.infinite);
            }
        }
    }

    // -- actions -----------------------------------------------------------

    /// Build mirror entries from tracks, dropping dead and unavailable
    /// ones (old playTrack filtered before the queue was ever set) and
    /// recording artwork for the player bar.
    pub fn track_entries(&mut self, tracks: &[Track]) -> Vec<MirrorEntry> {
        self.player.note_art(tracks);
        tracks
            .iter()
            .filter(|t| !t.unavailable && !t.id.is_empty() && !self.player.dead.contains(&t.id))
            .map(|t| MirrorEntry {
                item: QueueItem {
                    id: t.id.clone(),
                    kind: "song".into(),
                    title: (!t.title.is_empty()).then(|| t.title.clone()),
                    artist: (!t.artist.is_empty()).then(|| t.artist.clone()),
                },
                radio: false,
                art: t
                    .artwork
                    .as_ref()
                    .map(|a| a.url.clone())
                    .filter(|u| !u.is_empty()),
                duration_ms: t.duration_ms,
                genres: t.genres.clone(),
            })
            .collect()
    }

    /// Play `entries` from `start`, replacing the queue on both sides.
    pub fn play_queue(&mut self, ctx: &egui::Context, entries: Vec<MirrorEntry>, start: usize) {
        if entries.is_empty() {
            self.set_status("Nothing playable in that queue.");
            return;
        }
        self.player.cancel_radio_retry();
        let start = start.min(entries.len() - 1);
        self.player.queue = entries;
        self.player.index = Some(start);
        self.player.ended_handled = None;
        self.player.user_paused = false;
        self.player.restored = None;
        self.player.origin = QueueOrigin::from_entries(&self.player.queue);
        self.player.radio_fill_count = 0;
        let items: Vec<QueueItem> = self.player.queue.iter().map(|e| e.item.clone()).collect();
        let state = self.backend.state.clone();
        let rx = self.backend.call(ctx, async move {
            shell::commands::player_play(&state, items, Some(start as u32))
                .await
                .map(|r| PlayOutcome {
                    track_id: (!r.track_id.is_empty()).then_some(r.track_id),
                    message: r.message,
                })
        });
        self.player.play.start(rx);
        self.persist_now();
        self.maybe_fill_radio(ctx);
    }

    /// Jump the queue to `i` (old scheduleQueueJump/commitQueueJump): the
    /// backend plays just this entry, then the mirror-ahead window is
    /// re-appended so OS media-key next steps inside tracks the UI knows.
    pub fn jump_to(&mut self, ctx: &egui::Context, i: usize) {
        let Some(entry) = self.player.queue.get(i).cloned() else {
            return;
        };
        self.player.cancel_radio_retry();
        self.player.index = Some(i);
        self.player.ended_handled = None;
        self.player.user_paused = false;
        self.player.restored = None;
        let ahead: Vec<QueueItem> = self
            .player
            .queue
            .iter()
            .skip(i + 1)
            .take(MIRROR_AHEAD)
            .map(|e| e.item.clone())
            .collect();
        let state = self.backend.state.clone();
        let rx = self.backend.call(ctx, async move {
            shell::commands::player_play(&state, vec![entry.item], Some(0))
                .await
                .map(|r| PlayOutcome {
                    track_id: (!r.track_id.is_empty()).then_some(r.track_id),
                    message: r.message,
                })
        });
        self.player.play.start(rx);
        if !ahead.is_empty() {
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "queue append", async move {
                shell::commands::player_append(&state, ahead).await
            });
        }
        self.persist_now();
        self.maybe_fill_radio(ctx);
    }

    /// Append entries to the queue (both sides).
    pub fn queue_append(&mut self, ctx: &egui::Context, entries: Vec<MirrorEntry>) {
        if entries.is_empty() {
            return;
        }
        let pre: Vec<QueueItem> = entries.iter().map(|e| e.item.clone()).collect();
        self.player.queue.extend(entries);
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "queue append", async move {
            shell::commands::player_append(&state, pre).await
        });
        self.persist_now();
    }

    /// Insert right after the current track (both sides).
    pub fn queue_play_next(&mut self, ctx: &egui::Context, entries: Vec<MirrorEntry>) {
        if entries.is_empty() {
            return;
        }
        let pre: Vec<QueueItem> = entries.iter().map(|e| e.item.clone()).collect();
        let at = self.player.index.map(|i| i + 1).unwrap_or(0);
        let at = at.min(self.player.queue.len());
        for (offset, entry) in entries.into_iter().enumerate() {
            self.player.queue.insert(at + offset, entry);
        }
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "queue play-next", async move {
            shell::commands::player_play_next(&state, pre).await
        });
        self.persist_now();
    }

    /// Remove an entry from the mirror (old removeFromQueue): no player
    /// remove command exists, so the backend queue may still hold the id;
    /// adoption treats unknown player tracks as authoritative anyway.
    pub fn queue_remove(&mut self, i: usize) {
        if i >= self.player.queue.len() {
            return;
        }
        self.player.queue.remove(i);
        match self.player.index {
            Some(cur) if i < cur => self.player.index = Some(cur - 1),
            Some(cur) if i == cur => {
                self.player.index = if self.player.queue.is_empty() {
                    None
                } else {
                    Some(cur.min(self.player.queue.len() - 1))
                };
            }
            _ => {}
        }
        self.persist_now();
    }

    pub fn queue_clear(&mut self, ctx: &egui::Context) {
        self.player.queue.clear();
        self.player.index = None;
        self.player.ended_handled = None;
        self.player.origin = None;
        self.player.radio_fill_count = 0;
        self.player.user_paused = false;
        self.player.restored = None;
        self.player.cancel_radio_retry();
        self.persist_now();
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "clear queue", async move {
            shell::commands::player_clear(&state).await
        });
    }

    /// Snapshot the mirror + position for the session store (old
    /// persistQueue): on mutations, every ~10s while playing, and at
    /// shutdown. Restores paused on the next launch, never auto-plays.
    pub fn persist_now(&mut self) {
        let position = self
            .player
            .report
            .as_ref()
            .map(|r| r.position_ms)
            .or(self.player.restored.map(|(p, _)| p))
            .unwrap_or(0);
        let session = store::Session {
            v: 1,
            queue_index: self.player.index.map(|i| i as i64).unwrap_or(-1),
            queue: self
                .player
                .queue
                .iter()
                .take(store::SESSION_MAX)
                .map(store::SessionEntry::from)
                .collect(),
            position_ms: position,
        };
        store::save_session(&session);
        self.player.last_persist = Instant::now();
    }

    pub fn on_prev(&mut self, ctx: &egui::Context) {
        // Old transport previous: jump back a playable entry; at the
        // head, replay the current one.
        if let Some(pv) = self.next_playable(-1) {
            self.jump_to(ctx, pv);
            return;
        }
        if let Some(i) = self.player.index {
            self.jump_to(ctx, i);
        }
    }

    pub fn on_play_pause(&mut self, ctx: &egui::Context) {
        let playing = self
            .player
            .report
            .as_ref()
            .map(|r| r.playing)
            .unwrap_or(false);
        let has_track = self
            .player
            .report
            .as_ref()
            .map(|r| r.track_id.is_some())
            .unwrap_or(false);
        if playing {
            self.player.user_paused = true;
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "pause", async move {
                shell::commands::player_pause(&state).await
            });
        } else if !has_track {
            // After an app restart the backend holds no audio (the queue
            // lives UI-side only), so replay the current queue entry
            // instead of resuming thin air (old transport play parity).
            if let Some(i) = self.player.index {
                self.jump_to(ctx, i);
            } else {
                self.set_status("Nothing queued.");
            }
        } else {
            self.player.user_paused = false;
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "resume", async move {
                shell::commands::player_resume(&state).await
            });
        }
    }

    pub fn on_next(&mut self, ctx: &egui::Context) {
        // Single skip path: everything goes through UI queue jumps so
        // progress and queue state can't diverge from the audible track
        // (old transport next). At the tail, grow the radio queue first;
        // dead entries are skipped, never played. The dead end stays
        // put — a replay here would feel like a phantom skip.
        if self.player.queue.is_empty() {
            self.set_status("Nothing queued.");
            return;
        }
        if let Some(nx) = self.next_playable(1) {
            self.jump_to(ctx, nx);
            return;
        }
        self.maybe_fill_radio(ctx);
        if let Some(nx) = self.next_playable(1) {
            self.jump_to(ctx, nx);
        }
    }

    pub fn on_seek(&mut self, ctx: &egui::Context, position_ms: u64) {
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "seek", async move {
            shell::commands::player_seek(&state, position_ms).await
        });
    }

    /// Push settings-derived backend toggles when they change (volume,
    /// notifications, Discord). Runs every tick; costs nothing idle.
    fn apply_integrations(&mut self, ctx: &egui::Context) {
        if !self.player.settings_bootstrapped {
            // Defaults match the backend's, so unchanged values never fire;
            // a non-default persisted value is pushed exactly once here.
            self.player.volume_applied = 80.0;
            self.player.notifications_applied = false;
            self.player.discord_applied = false;
            self.player.settings_bootstrapped = true;
        }
        let volume = self.settings.volume.clamp(0.0, 100.0);
        if volume != self.player.volume_applied {
            self.player.volume_applied = volume;
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "volume", async move {
                shell::commands::player_volume(&state, volume / 100.0).await
            });
        }
        if self.settings.notifications != self.player.notifications_applied {
            self.player.notifications_applied = self.settings.notifications;
            let enabled = self.settings.notifications;
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "notifications", async move {
                shell::commands::set_player_notifications(&state, enabled)
            });
        }
        if self.settings.discord != self.player.discord_applied {
            self.player.discord_applied = self.settings.discord;
            let enabled = self.settings.discord;
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "discord", async move {
                shell::commands::set_discord_enabled(&state, enabled)
            });
        }
        let app_id = self.settings.discord_app_id.trim().to_owned();
        if app_id != self.player.discord_app_id_applied {
            self.player.discord_app_id_applied = app_id.clone();
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "discord app id", async move {
                shell::commands::set_discord_app_id(&state, app_id)
            });
        }
    }

    /// Push playback snapshots to Discord (backend no-ops unless enabled
    /// with an app id): on track/play flips, and at most every 15s while
    /// playing to keep the progress timestamps fresh.
    fn maybe_push_discord(&mut self, ctx: &egui::Context, force: bool) {
        if !self.settings.discord {
            return;
        }
        let Some(r) = &self.player.report else {
            if self.player.discord_key.is_some() {
                self.player.discord_key = None;
                let state = self.backend.state.clone();
                self.backend.fire(ctx, "discord clear", async move {
                    shell::commands::clear_discord_presence(&state)
                });
            }
            return;
        };
        let playing = r.playing;
        let key = (r.track_id.clone().unwrap_or_default(), playing);
        if !force && self.player.discord_key.as_ref() == Some(&key) {
            if let Some(at) = self.player.discord_at {
                if at.elapsed() < DISCORD_REFRESH || !playing {
                    return;
                }
            } else {
                return;
            }
        }
        self.player.discord_key = Some(key);
        self.player.discord_at = Some(Instant::now());
        let payload = shell::discord::PresencePayload {
            title: r.title.clone().unwrap_or_default(),
            artist: r.artist.clone().unwrap_or_default(),
            album: r.album.clone().unwrap_or_default(),
            playing,
            position_ms: r.position_ms,
            duration_ms: r.duration_ms,
        };
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "discord presence", async move {
            shell::commands::update_discord_presence(&state, payload)
        });
    }
}

/// One radio fill (old maybeFillRadio's fetch loop): walk seeds through
/// `similar_songs`, bridging through already-queued results into fresh
/// neighborhoods (transitive expansion), then rank by origin affinity.
/// Runs on the backend runtime; the UI appends the result as autoplay
/// entries. No mainstream fallback — a regional top chart shares nothing
/// with the vibe and poisons it.
async fn radio_fetch(
    state: std::sync::Arc<shell::AppState>,
    seeds: Vec<String>,
    queued: HashSet<String>,
    dead: HashSet<String>,
    depth: u32,
    origin: Option<QueueOrigin>,
    exclude: Vec<String>,
) -> Result<RadioFill, String> {
    let fresh_target = if origin.is_some() { 8 } else { 6 };
    let max_fetches = if origin.is_some() { 12 } else { 8 };
    // Origin fills draw from many artists at once: cap each seed's intake
    // so one broad seed (e.g. a 25-track station batch) can't flood the
    // pool before the other artists' seeds are queried. 0 = uncapped.
    let per_seed_cap = if origin.is_some() { 4 } else { 0 };
    let mut have = queued;
    let mut tried: HashSet<String> = HashSet::new();
    let mut pending: VecDeque<String> = seeds.into_iter().collect();
    let mut fresh: Vec<Track> = Vec::new();
    let mut error = String::new();
    let mut fetches = 0usize;
    while let Some(seed) = pending.pop_front() {
        if fresh.len() >= fresh_target || fetches >= max_fetches {
            break;
        }
        if seed.is_empty() || !tried.insert(seed.clone()) {
            continue;
        }
        fetches += 1;
        let similar = match shell::commands::similar_songs(
            &state,
            seed.clone(),
            Some(exclude.clone()),
            Some(depth),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                error = e;
                continue;
            }
        };
        let mut added = 0usize;
        for t in similar {
            // Known-dead (tombstone or session-learned): never suggested.
            if t.id.is_empty() || t.unavailable || dead.contains(&t.id) {
                continue;
            }
            if !have.contains(&t.id) {
                if per_seed_cap > 0 && added >= per_seed_cap {
                    // Pool budget for this seed spent: keep the leftover
                    // as a bridge so its neighborhood isn't lost, but
                    // don't append it.
                    if !tried.contains(&t.id) && !pending.contains(&t.id) {
                        pending.push_back(t.id);
                    }
                    continue;
                }
                have.insert(t.id.clone());
                fresh.push(t);
                added += 1;
            } else if !tried.contains(&t.id) && !pending.contains(&t.id) {
                pending.push_back(t.id); // bridge into a new neighborhood
            }
        }
    }
    // Playlist-origin fills: rank the merged pool by affinity to the FULL
    // origin (stable — per-seed backend order survives ties), then DROP
    // anything sharing neither an artist nor a genre with the playlist.
    let tracks = match &origin {
        Some(o) => {
            let mut ranked: Vec<(u32, Track)> = fresh
                .into_iter()
                .map(|t| (o.affinity(&t.artist, &t.genres), t))
                .collect();
            ranked.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
            ranked
                .into_iter()
                .filter(|(s, _)| *s >= 1)
                .take(fresh_target)
                .map(|(_, t)| t)
                .collect()
        }
        None => fresh,
    };
    Ok(RadioFill { tracks, error })
}
