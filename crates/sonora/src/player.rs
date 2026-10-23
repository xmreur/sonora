//! Player status polling, transport actions, and the UI-side queue mirror.
//!
//! P1b scope: status poll + now-playing, transport (play/pause/next/prev/
//! seek/volume), a queue mirror kept in lockstep with the backend, basic
//! track-end advance, OS media-key counter execution, and Discord
//! presence pushes. The rich play-chain from the old web UI (radio fill,
//! dead-id skipping, session restore) lands with the player-core phase.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use apple_music_core::models::Track;
use apple_music_core::playback::QueueItem;
use shell::native_player::PlayerReport;

use crate::app::SonoraApp;
use crate::backend::Pending;

/// Status poll cadence (matches the backend's 250ms publish loop).
const POLL: Duration = Duration::from_millis(250);
/// Discord progress refresh while playing.
const DISCORD_REFRESH: Duration = Duration::from_secs(15);

pub struct PlayerUi {
    /// Latest status report from the engine.
    pub report: Option<PlayerReport>,
    /// UI mirror of the backend queue: play/append/play-next/clear keep
    /// both sides identical, so `player_next`/`player_previous` step the
    /// same list this view renders.
    pub queue: Vec<QueueItem>,
    /// Mirror index into `queue`.
    pub index: Option<usize>,
    /// Artwork templates for tracks seen this session: the queue mirror
    /// keeps ids only, so the player bar resolves its cover through this
    /// map (populated from every list the UI plays from).
    pub art_by_track: HashMap<String, String>,
    /// In-flight `player_play` outcome (message/error → status line).
    pub play: Pending<String>,
    status: Pending<PlayerReport>,
    last_poll: Instant,
    was_playing: Option<bool>,
    last_track_id: Option<String>,
    last_detail: String,
    last_os_next: u64,
    last_os_prev: u64,
    discord_at: Option<Instant>,
    discord_key: Option<(String, bool)>,
    ended_handled: Option<String>,
    /// Last values applied to the backend (change detection).
    pub volume_applied: f32,
    pub notifications_applied: bool,
    pub discord_applied: bool,
    pub discord_app_id_applied: String,
    /// While the user drags the seek slider, reports must not move it.
    pub seeking: bool,
    /// `Some(true)` once the initial settings push has run.
    settings_bootstrapped: bool,
}

impl Default for PlayerUi {
    fn default() -> Self {
        Self {
            report: None,
            queue: Vec::new(),
            index: None,
            art_by_track: HashMap::new(),
            play: Pending::default(),
            status: Pending::default(),
            last_poll: Instant::now(),
            was_playing: None,
            last_track_id: None,
            last_detail: String::new(),
            last_os_next: 0,
            last_os_prev: 0,
            discord_at: None,
            discord_key: None,
            ended_handled: None,
            volume_applied: f32::NAN,
            notifications_applied: false,
            discord_applied: false,
            discord_app_id_applied: String::new(),
            seeking: false,
            settings_bootstrapped: false,
        }
    }
}

impl PlayerUi {
    /// Remember artwork templates for these tracks (first seen wins — a
    /// session map of small strings, drained only by process exit). The
    /// queue mirror keeps ids only, so the player bar's cover resolves
    /// through this map.
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
}

impl SonoraApp {
    /// Per-frame player maintenance — call from `logic()` (runs while the
    /// window is hidden too, driven by `request_repaint_after`).
    pub fn tick_player(&mut self, ctx: &egui::Context) {
        // Consume finished calls.
        self.player.status.poll();
        self.player.play.poll();
        if let Some(msg) = self.player.play.value.take() {
            self.set_status(msg);
        }
        if let Some(err) = self.player.play.error.take() {
            self.set_status(&err);
            self.log(err);
        }

        // Surface fire-and-forget outcomes (settings toggles, playlist
        // mutations, auth actions …).
        let notices = self.backend.take_notices();
        for msg in notices {
            self.set_status(&msg);
            self.log(msg);
        }

        // React to a fresh report.
        if let Some(report) = self.player.status.value.take() {
            self.on_report(report, ctx);
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
                // Re-adopt the queue index for the now-playing track.
                self.player.index = self.find_queue_index(&report);
                if let Some(title) = report.title.clone() {
                    let artist = report.artist.clone().unwrap_or_default();
                    let line = if artist.is_empty() {
                        format!("▶ {title}")
                    } else {
                        format!("▶ {title} — {artist}")
                    };
                    self.log(line);
                }
            }
        }

        let playing_flipped = self.player.was_playing != Some(report.playing);
        if track_changed || playing_flipped {
            self.maybe_push_discord(ctx, true);
        }

        // OS media-key skips arrive as counter bumps (MPRIS), never as
        // blind backend commands — execute each here.
        let os_next = report.os_next;
        let os_prev = report.os_prev;
        if self.player.last_os_next != 0 || os_next != 0 {
            let delta = os_next.saturating_sub(self.player.last_os_next);
            if self.player.last_os_next != 0 && delta > 0 && delta <= 8 {
                for _ in 0..delta {
                    self.on_next(ctx);
                }
            }
        }
        if self.player.last_os_prev != 0 || os_prev != 0 {
            let delta = os_prev.saturating_sub(self.player.last_os_prev);
            if self.player.last_os_prev != 0 && delta > 0 && delta <= 8 {
                for _ in 0..delta {
                    self.on_prev(ctx);
                }
            }
        }
        self.player.last_os_next = os_next;
        self.player.last_os_prev = os_prev;

        // Track end: was playing → stopped on the same track with the
        // playhead at (or wrapped past) the end. A mid-track pause does
        // not qualify.
        let ended = self.player.was_playing == Some(true)
            && !report.playing
            && !track_changed
            && report.duration_ms > 0
            && (report.position_ms + 1200 >= report.duration_ms || report.position_ms < 500);
        let already = self.player.ended_handled.as_deref() == tid.as_deref() && tid.is_some();
        if ended && !already {
            self.player.ended_handled = tid.clone();
            self.on_track_end(ctx);
        }

        self.player.was_playing = Some(report.playing);
        self.player.last_track_id = tid;
        self.player.report = Some(report);
    }

    /// Locate the queue entry for a report: exact id first (neighbors
    /// resolve duplicates directionally), then title/artist fallback for
    /// storefront-re-resolved ids. Returns `None` when nothing matches.
    fn find_queue_index(&self, report: &PlayerReport) -> Option<usize> {
        let tid = report.track_id.as_deref()?;
        let q = &self.player.queue;
        if let Some(cur) = self.player.index {
            if q.get(cur + 1).map(|i| i.id.as_str()) == Some(tid) {
                return Some(cur + 1);
            }
            if cur > 0 && q.get(cur - 1).map(|i| i.id.as_str()) == Some(tid) {
                return Some(cur - 1);
            }
        }
        if let Some(at) = q.iter().position(|i| i.id == tid) {
            return Some(at);
        }
        let title = report.title.as_deref()?.trim().to_lowercase();
        if title.is_empty() {
            return None;
        }
        let artist = report.artist.as_deref().unwrap_or("").trim().to_lowercase();
        q.iter().position(|i| {
            i.title
                .as_deref()
                .map(|t| t.trim().to_lowercase() == title)
                .unwrap_or(false)
                && i.artist
                    .as_deref()
                    .map(|a| a.trim().to_lowercase() == artist)
                    .unwrap_or(true)
        })
    }

    /// End of the current track: loop-one replays it, else advance to the
    /// next mirrored queue entry. Infinite-queue fill lands with the
    /// player-core phase (radio).
    fn on_track_end(&mut self, ctx: &egui::Context) {
        if self.settings.loop_one {
            if let Some(i) = self.player.index {
                if let Some(title) = self.player.report.as_ref().and_then(|r| r.title.clone()) {
                    self.set_status(format!("Looping “{title}”"));
                }
                let items = self.player.queue.clone();
                self.play_queue(ctx, items, i);
                return;
            }
        }
        let next = self.player.index.map(|i| i + 1).unwrap_or(0);
        if next < self.player.queue.len() {
            self.player.index = Some(next);
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "next", async move {
                shell::commands::player_next(&state).await
            });
        } else if self.settings.infinite {
            self.set_status("Infinite queue: radio fill lands with the player-core phase");
        } else {
            self.set_status("Queue finished");
        }
    }

    // -- actions -----------------------------------------------------------

    /// Play `items` from `start`, replacing the queue on both sides.
    pub fn play_queue(&mut self, ctx: &egui::Context, items: Vec<QueueItem>, start: usize) {
        if items.is_empty() {
            self.set_status("Nothing playable in that queue.");
            return;
        }
        let start = start.min(items.len() - 1);
        self.player.queue = items.clone();
        self.player.index = Some(start);
        self.player.ended_handled = None;
        let state = self.backend.state.clone();
        let rx = self.backend.call(ctx, async move {
            shell::commands::player_play(&state, items, Some(start as u32))
                .await
                .map(|r| r.message)
        });
        self.player.play.start(rx);
    }

    /// Append to the queue (both sides).
    pub fn queue_append(&mut self, ctx: &egui::Context, items: Vec<QueueItem>) {
        if items.is_empty() {
            return;
        }
        let pre = items.clone();
        self.player.queue.extend(items.iter().cloned());
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "queue append", async move {
            shell::commands::player_append(&state, pre).await
        });
    }

    /// Insert right after the current track (both sides).
    pub fn queue_play_next(&mut self, ctx: &egui::Context, items: Vec<QueueItem>) {
        if items.is_empty() {
            return;
        }
        let pre = items.clone();
        let at = self.player.index.map(|i| i + 1).unwrap_or(0);
        let at = at.min(self.player.queue.len());
        for (offset, item) in items.into_iter().enumerate() {
            self.player.queue.insert(at + offset, item);
        }
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "queue play-next", async move {
            shell::commands::player_play_next(&state, pre).await
        });
    }

    pub fn queue_clear(&mut self, ctx: &egui::Context) {
        self.player.queue.clear();
        self.player.index = None;
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "clear queue", async move {
            shell::commands::player_clear(&state).await
        });
    }

    pub fn on_prev(&mut self, ctx: &egui::Context) {
        // Restart the track when more than 3s in (transport convention).
        if let Some(r) = &self.player.report {
            if r.position_ms > 3000 {
                self.on_seek(ctx, 0);
                return;
            }
        }
        if let Some(i) = self.player.index {
            if i > 0 {
                self.player.index = Some(i - 1);
            }
        }
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "previous", async move {
            shell::commands::player_previous(&state).await
        });
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
        if !has_track {
            self.set_status("Nothing queued.");
            return;
        }
        let state = self.backend.state.clone();
        if playing {
            self.backend.fire(ctx, "pause", async move {
                shell::commands::player_pause(&state).await
            });
        } else {
            self.backend.fire(ctx, "resume", async move {
                shell::commands::player_resume(&state).await
            });
        }
    }

    pub fn on_next(&mut self, ctx: &egui::Context) {
        let next = self.player.index.map(|i| i + 1).unwrap_or(0);
        if next >= self.player.queue.len() {
            // Mirror the old Next dead end: at the last track, stay put.
            if self.player.queue.is_empty() {
                self.set_status("Nothing queued.");
            }
            return;
        }
        self.player.index = Some(next);
        self.player.ended_handled = None;
        let state = self.backend.state.clone();
        self.backend.fire(ctx, "next", async move {
            shell::commands::player_next(&state).await
        });
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
