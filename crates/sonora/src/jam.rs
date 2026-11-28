//! Jam UI: host publishes the queue it already owns, guests follow it.
//!
//! Playback stays on each person's own Apple Music session. This module
//! only decides whose button press is allowed to move that queue.

use std::time::{Duration, Instant};

use apple_music_core::api::ApiClient;
use apple_music_core::playback::QueueItem;
use shell::jam::{
    expected_position_ms, unix_ms, DriftGate, JamAction, JamEvent, JamRole, JamSnapshot, JamTrack,
    Participant, PathKind, PlaybackAnchor, PublishUpdate,
};

use crate::app::SonoraApp;
use crate::backend::Pending;
use crate::player::MirrorEntry;

pub struct SavedQueue {
    pub queue: Vec<MirrorEntry>,
    pub index: Option<usize>,
    pub position_ms: u64,
}

pub struct JamUi {
    pub role: Option<JamRole>,
    /// Set between Start/Join and `Connected` so the panel shows progress
    /// instead of looking idle.
    pub connecting: Option<JamRole>,
    pub ticket: String,
    pub invite_input: String,
    pub participants: Vec<Participant>,
    pub path: PathKind,
    pub rtt_ms: Option<u64>,
    pub guests_can_control: bool,
    /// A remote snapshot is being applied. Local transport must not echo
    /// it back onto the wire.
    pub applying: bool,
    pub saved: Option<SavedQueue>,
    /// The guest has replaced its queue with the room at least once.
    /// Leaving before that must not stop whatever was already playing.
    pub took_over: bool,
    pub drift: DriftGate,
    pub last_revision: u64,
    pub publish_gen: u64,
    next_uid: u64,
    pub peer_id: String,
    pub anchor: Option<PlaybackAnchor>,
    /// Guest's latest offset: host_now ≈ guest_now + offset.
    pub clock_offset_ms: i64,
    /// `jam_uid` of the row the local engine was last told to play.
    pub playing_uid: Option<u64>,
    /// The loaded track still needs its first seek/pause to match the host.
    pub pending_sync: bool,
    /// One local seek at a time. A second heartbeat must not start another
    /// decode wait on top of the first.
    pub guest_seek: Pending<()>,
    /// Last anchor this host published, so a real stall can be told apart
    /// from the position we already claimed.
    pub last_host_anchor: Option<PlaybackAnchor>,
    /// When the host last published an explicit seek. Reports still catching
    /// up to it are not a new stall.
    pub last_host_seek_at: Option<Instant>,
    /// Right-hand pane is showing the jam tab rather than the queue.
    pub show_jam: bool,
    /// When the user last copied the invite code (brief "Copied" feedback).
    pub copied_at: Option<Instant>,
    /// Inline status or error banner inside the jam panel.
    pub notice: Option<String>,
}

impl Default for JamUi {
    fn default() -> Self {
        Self {
            role: None,
            connecting: None,
            ticket: String::new(),
            invite_input: String::new(),
            participants: Vec::new(),
            path: PathKind::Unknown,
            rtt_ms: None,
            guests_can_control: false,
            applying: false,
            saved: None,
            took_over: false,
            drift: DriftGate::default(),
            last_revision: 0,
            publish_gen: 0,
            next_uid: 0,
            peer_id: String::new(),
            anchor: None,
            clock_offset_ms: 0,
            playing_uid: None,
            pending_sync: false,
            guest_seek: Pending::default(),
            last_host_anchor: None,
            last_host_seek_at: None,
            show_jam: true,
            copied_at: None,
            notice: None,
        }
    }
}

impl JamUi {
    pub fn is_guest(&self) -> bool {
        self.role == Some(JamRole::Guest)
    }

    pub fn is_host(&self) -> bool {
        self.role == Some(JamRole::Host)
    }

    pub fn live(&self) -> bool {
        self.role.is_some()
    }

    pub fn busy(&self) -> bool {
        self.connecting.is_some()
    }
}

enum Submit {
    Append(Vec<MirrorEntry>),
    PlayNext(Vec<MirrorEntry>),
    Replace {
        entries: Vec<MirrorEntry>,
        start: usize,
    },
}

impl SonoraApp {
    pub(crate) fn tick_jam(&mut self, ctx: &egui::Context) {
        self.poll_guest_seek(ctx);
        while let Some(ev) = self.backend.state.jam.poll_event() {
            self.on_jam_event(ctx, ev);
        }
    }

    pub(crate) fn jam_start(&mut self, ctx: &egui::Context) {
        if self.jam.live() || self.jam.busy() || self.backend.state.jam.is_active() {
            self.set_status("Already in a jam.");
            return;
        }
        let name = self.jam_name();
        let control = self.settings.jam_guests_can_control;
        let state = self.backend.state.clone();
        self.jam.connecting = Some(JamRole::Host);
        self.jam.notice = None;
        self.backend.fire(ctx, "jam", async move {
            state.jam.start_host(name, control).await
        });
        self.settings.jam_open = true;
        self.jam.show_jam = true;
        self.set_status("Starting a jam…");
    }

    pub(crate) fn jam_join(&mut self, ctx: &egui::Context) {
        if self.jam.live() || self.jam.busy() || self.backend.state.jam.is_active() {
            self.set_status("Leave the current jam first.");
            return;
        }
        let ticket = self.jam.invite_input.trim().to_string();
        if ticket.is_empty() {
            self.jam.notice = Some("Paste a jam code first.".into());
            self.set_status("Paste a jam code first.");
            return;
        }
        self.stash_personal();
        let name = self.jam_name();
        let state = self.backend.state.clone();
        self.jam.connecting = Some(JamRole::Guest);
        self.jam.notice = None;
        self.backend.fire(
            ctx,
            "jam",
            async move { state.jam.join(ticket, name).await },
        );
        self.settings.jam_open = true;
        self.jam.show_jam = true;
        self.set_status("Joining jam…");
    }

    pub(crate) fn jam_end(&mut self, ctx: &egui::Context) {
        let reason = if self.jam.busy() && !self.jam.live() {
            "Cancelled."
        } else if self.jam.is_guest() {
            "Left the jam."
        } else {
            "Jam ended."
        };
        self.jam.notice = None;
        self.finish_jam(ctx, reason.to_string());
    }

    /// Toggle the right-hand drawer to the Queue tab (or close if already there).
    pub(crate) fn toggle_queue_drawer(&mut self) {
        let showing_queue =
            (self.settings.queue_open || self.settings.jam_open) && !self.jam.show_jam;
        if showing_queue {
            self.settings.queue_open = false;
            self.settings.jam_open = false;
        } else {
            self.settings.queue_open = true;
            self.settings.jam_open = false;
            self.jam.show_jam = false;
        }
    }

    /// Toggle the right-hand drawer to the Jam tab (or close if already there).
    pub(crate) fn toggle_jam_drawer(&mut self) {
        let showing_jam = (self.settings.queue_open || self.settings.jam_open) && self.jam.show_jam;
        if showing_jam {
            self.settings.queue_open = false;
            self.settings.jam_open = false;
        } else {
            self.settings.jam_open = true;
            self.settings.queue_open = false;
            self.jam.show_jam = true;
        }
    }

    pub(crate) fn jam_kick(&mut self, peer_id: &str) {
        self.backend.state.jam.kick(peer_id.to_string());
    }

    pub(crate) fn jam_set_control(&mut self, on: bool) {
        self.settings.jam_guests_can_control = on;
        self.jam.guests_can_control = on;
        self.backend.state.jam.set_control(on);
        self.sync_jam_host();
    }

    /// `true` when every entry was a local file and the caller should stop.
    pub(crate) fn jam_strip_files(&mut self, entries: &mut Vec<MirrorEntry>) -> bool {
        if !self.jam.live() {
            return false;
        }
        let before = entries.len();
        entries.retain(|e| e.item.kind != "file");
        if entries.len() != before {
            self.set_status("Local files can't be shared in a jam.");
        }
        entries.is_empty()
    }

    /// Guest "play this" — with control, replace the room; otherwise append.
    /// `true` when the local queue must not change.
    pub(crate) fn jam_guest_play(
        &mut self,
        ctx: &egui::Context,
        entries: &[MirrorEntry],
        start: usize,
    ) -> bool {
        if !self.jam.is_guest() || self.jam.applying {
            return false;
        }
        if self.jam.guests_can_control {
            self.jam_submit(
                ctx,
                Submit::Replace {
                    entries: entries.to_vec(),
                    start,
                },
            );
        } else {
            self.jam_submit(ctx, Submit::Append(entries.to_vec()));
            self.set_status("Added to the jam.");
        }
        true
    }

    pub(crate) fn jam_guest_add(
        &mut self,
        ctx: &egui::Context,
        entries: Vec<MirrorEntry>,
        next: bool,
    ) -> bool {
        if !self.jam.is_guest() || self.jam.applying {
            return false;
        }
        self.jam_submit(
            ctx,
            if next {
                Submit::PlayNext(entries)
            } else {
                Submit::Append(entries)
            },
        );
        true
    }

    /// `true` when a guest transport click was forwarded or refused.
    pub(crate) fn jam_guest_control(&mut self, action: JamAction) -> bool {
        if !self.jam.is_guest() || self.jam.applying {
            return false;
        }
        let open = matches!(action, JamAction::QueueRemove { .. });
        if !open && !self.jam.guests_can_control {
            self.set_status("Only the host can control playback.");
            return true;
        }
        self.backend.state.jam.send_action(action);
        true
    }

    pub(crate) fn sync_jam_host(&mut self) {
        if !self.jam.is_host() || self.jam.applying {
            return;
        }
        self.assign_uids();
        self.jam.publish_gen = self.jam.publish_gen.saturating_add(1);
        let gen = self.jam.publish_gen;
        let mut tracks = Vec::new();
        let mut index = None;
        for (i, entry) in self.player.queue.iter().enumerate() {
            if entry.item.kind == "file" {
                continue;
            }
            if self.player.index == Some(i) {
                index = Some(tracks.len());
            }
            tracks.push(track_from_entry(entry));
        }
        let playback = self.playback_anchor(None, None);
        self.jam.last_host_anchor = Some(playback.clone());
        let control = self.settings.jam_guests_can_control;
        let state = self.backend.state.clone();
        self.backend.spawn(async move {
            let tracks = resolve_tracks(&state, tracks).await;
            state.jam.publish(PublishUpdate {
                gen,
                guests_can_control: control,
                queue: tracks,
                current_index: index,
                playback,
            });
        });
    }

    pub(crate) fn finish_jam(&mut self, ctx: &egui::Context, reason: String) {
        let restore = self.jam.is_guest() && self.jam.took_over;
        let saved = self.jam.saved.take();
        self.jam.role = None;
        self.jam.connecting = None;
        self.jam.ticket.clear();
        self.jam.participants.clear();
        self.jam.anchor = None;
        self.jam.last_revision = 0;
        self.jam.peer_id.clear();
        self.jam.path = PathKind::Unknown;
        self.jam.rtt_ms = None;
        self.jam.took_over = false;
        self.jam.applying = false;
        self.jam.copied_at = None;
        self.jam.clock_offset_ms = 0;
        self.jam.playing_uid = None;
        self.jam.pending_sync = false;
        self.jam.guest_seek.clear();
        self.jam.last_host_anchor = None;
        self.jam.last_host_seek_at = None;
        self.backend.state.jam.shutdown();
        if restore {
            if let Some(saved) = saved {
                self.restore_personal(ctx, saved);
            }
        }
        if !reason.is_empty() {
            self.set_status(reason);
        }
    }

    fn jam_name(&self) -> String {
        let name = self.settings.jam_name.trim();
        if name.is_empty() {
            default_jam_name()
        } else {
            name.to_string()
        }
    }

    fn stash_personal(&mut self) {
        if self.jam.saved.is_some() {
            return;
        }
        self.jam.saved = Some(SavedQueue {
            queue: self.player.queue.clone(),
            index: self.player.index,
            position_ms: self.player.clock.est(),
        });
    }

    fn on_jam_event(&mut self, ctx: &egui::Context, ev: JamEvent) {
        match ev {
            JamEvent::Connected {
                role,
                ticket,
                self_id,
                snapshot,
            } => {
                self.jam.role = Some(role);
                self.jam.connecting = None;
                self.jam.notice = None;
                self.jam.peer_id = self_id;
                self.jam.guests_can_control = snapshot.guests_can_control;
                self.jam.participants = snapshot.participants.clone();
                if let Some(ticket) = ticket {
                    self.jam.ticket = ticket;
                }
                match role {
                    JamRole::Host => {
                        self.set_status("Jam is live — copy the code and share it.");
                        self.sync_jam_host();
                    }
                    JamRole::Guest => {
                        if self.jam.saved.is_none() {
                            self.stash_personal();
                        }
                        self.jam.invite_input.clear();
                        self.set_status("Joined the jam.");
                    }
                }
            }
            JamEvent::RemoteState {
                snapshot,
                est_host_now_ms,
                clock_offset_ms,
                path,
                rtt_ms,
            } => {
                self.jam.path = path;
                self.jam.rtt_ms = rtt_ms;
                self.jam.clock_offset_ms = clock_offset_ms;
                self.jam.notice = None;
                self.apply_remote(ctx, snapshot, est_host_now_ms);
            }
            JamEvent::Heartbeat {
                anchor_host_ms,
                playing,
                position_ms,
                track_uid,
                clock_offset_ms,
                path,
                rtt_ms,
                ..
            } => {
                self.jam.path = path;
                self.jam.rtt_ms = rtt_ms;
                self.jam.clock_offset_ms = clock_offset_ms;
                self.on_heartbeat(ctx, playing, position_ms, anchor_host_ms, track_uid);
            }
            JamEvent::Clock {
                offset_ms,
                path,
                rtt_ms,
            } => {
                self.jam.clock_offset_ms = offset_ms;
                self.jam.path = path;
                self.jam.rtt_ms = rtt_ms;
                self.sync_guest_to_anchor(ctx, false);
            }
            JamEvent::GuestAction { name, action, .. } => {
                if self.jam.is_host() {
                    self.apply_guest_action(ctx, &name, action);
                }
            }
            JamEvent::Roster {
                participants,
                path,
                rtt_ms,
            } => {
                if !participants.is_empty() {
                    self.jam.participants = participants;
                }
                self.jam.path = path;
                self.jam.rtt_ms = rtt_ms;
            }
            JamEvent::Ended(reason) => {
                if self.jam.live() || self.jam.busy() || self.jam.saved.is_some() {
                    let unexpected =
                        !reason.is_empty() && reason != "jam ended" && reason != "left the jam";
                    self.jam.notice = unexpected.then(|| reason.clone());
                    self.finish_jam(ctx, reason);
                }
            }
            JamEvent::Notice(text) => {
                self.jam.notice = Some(text.clone());
                self.set_status(text);
            }
        }
    }

    fn apply_guest_action(&mut self, ctx: &egui::Context, _name: &str, action: JamAction) {
        match action {
            JamAction::QueueAppend { tracks } => {
                self.queue_append(ctx, tracks.into_iter().map(mirror_from_track).collect());
            }
            JamAction::QueuePlayNext { tracks } => {
                self.queue_play_next(ctx, tracks.into_iter().map(mirror_from_track).collect());
            }
            JamAction::QueueRemove { uid } => {
                if let Some(i) = self.player.queue.iter().position(|e| e.jam_uid == uid) {
                    self.queue_remove(i);
                }
            }
            JamAction::PlayPause => self.on_play_pause(ctx),
            JamAction::Next => self.on_next(ctx),
            JamAction::Prev => self.on_prev(ctx),
            JamAction::JumpTo { uid } => {
                if let Some(i) = self.player.queue.iter().position(|e| e.jam_uid == uid) {
                    self.jump_to(ctx, i);
                }
            }
            JamAction::Seek { position_ms } => self.on_seek(ctx, position_ms),
            JamAction::QueueClear => self.queue_clear(ctx),
            JamAction::Replace { tracks, start } => {
                let entries: Vec<MirrorEntry> = tracks.into_iter().map(mirror_from_track).collect();
                if !entries.is_empty() {
                    self.play_queue(ctx, entries, start);
                }
            }
        }
    }

    fn apply_remote(&mut self, ctx: &egui::Context, snap: JamSnapshot, _est_host_now_ms: u64) {
        if !self.jam.is_guest() {
            return;
        }
        if self.jam.took_over && snap.revision < self.jam.last_revision {
            return;
        }
        let previous = self.jam.anchor.clone();
        self.jam.last_revision = snap.revision;
        self.jam.guests_can_control = snap.guests_can_control;
        self.jam.participants = snap.participants;
        self.jam.anchor = Some(snap.playback.clone());
        self.jam.took_over = true;

        let entries: Vec<MirrorEntry> = snap.queue.iter().cloned().map(mirror_from_track).collect();
        let index = snap.current_index.filter(|i| *i < entries.len());
        self.jam.applying = true;
        self.player.queue = entries;
        self.player.index = index;
        self.player.origin = None;
        self.jam.applying = false;

        let Some(i) = index else {
            self.stop_guest_playback();
            return;
        };
        let incoming_uid = self.player.queue[i].jam_uid;
        // Identity is the jam row, not the catalog id. Storefronts resolve
        // the same song to different ids, and a pause used to look like a
        // new track and restart it from the top.
        let same_row = incoming_uid != 0 && self.jam.playing_uid == Some(incoming_uid);
        let need_load = !same_row
            || (!self.player.engine_has_source
                && snap.playback.playing
                && self.player.awaiting.is_none());
        if need_load {
            self.jam.playing_uid = Some(incoming_uid);
            self.jam.pending_sync = true;
            self.jam.drift.reset();
            self.jam.applying = true;
            self.jump_to(ctx, i);
            self.jam.applying = false;
            self.prefetch_upcoming(i);
            return;
        }
        let anchor_changed = previous.as_ref() != Some(&snap.playback);
        self.sync_guest_to_anchor(ctx, anchor_changed);
    }

    fn stop_guest_playback(&mut self) {
        self.jam.playing_uid = None;
        self.jam.pending_sync = false;
        self.jam.applying = true;
        self.player.user_paused = true;
        self.player.engine_has_source = false;
        let state = self.backend.state.clone();
        self.backend.spawn(async move {
            let _ = shell::commands::player_stop(&state);
        });
        self.jam.applying = false;
    }

    fn on_heartbeat(
        &mut self,
        ctx: &egui::Context,
        playing: bool,
        position_ms: u64,
        anchor_host_ms: u64,
        track_uid: Option<u64>,
    ) {
        if !self.jam.is_guest() {
            return;
        }
        let Some(anchor) = self.jam.anchor.clone() else {
            return;
        };
        if track_uid.is_some() && track_uid != anchor.track_uid {
            return;
        }
        let playing_changed = anchor.playing != playing;
        self.jam.anchor = Some(PlaybackAnchor {
            playing,
            position_ms,
            anchor_host_ms,
            ..anchor
        });
        self.sync_guest_to_anchor(ctx, playing_changed);
    }

    pub(crate) fn on_guest_report(&mut self, ctx: &egui::Context, load_confirmed: bool) {
        if !self.jam.is_guest() {
            return;
        }
        if load_confirmed {
            self.jam.pending_sync = true;
            self.jam.drift.reset();
        }
        if self.player.awaiting.is_some() {
            return;
        }
        if self.jam.pending_sync || load_confirmed {
            self.sync_guest_to_anchor(ctx, true);
            if !self.jam.guest_seek.busy() {
                self.jam.pending_sync = false;
            }
        }
    }

    pub(crate) fn on_host_report(
        &mut self,
        track_changed: bool,
        playing_flipped: bool,
        load_confirmed: bool,
    ) {
        if !self.jam.is_host() || self.jam.applying {
            return;
        }
        let mut republish = load_confirmed || playing_flipped || track_changed;
        if let (Some(anchor), Some(live)) =
            (self.jam.last_host_anchor.clone(), self.live_audio_pos_ms())
        {
            let expected = expected_position_ms(&anchor, unix_ms());
            let drift = (live as i64).saturating_sub(expected as i64).unsigned_abs();
            let seeking = self
                .jam
                .last_host_seek_at
                .is_some_and(|t| t.elapsed() < Duration::from_millis(800));
            if seeking && drift < DriftGate::THRESHOLD_MS {
                self.jam.last_host_seek_at = None;
            } else if !seeking && drift > DriftGate::THRESHOLD_MS {
                republish = true;
            }
        }
        if republish {
            self.sync_jam_playback(None, None);
        }
    }

    /// Play, pause, or seek, broadcast immediately. The queue resolve is a
    /// different message and must not delay or overwrite this one.
    pub(crate) fn sync_jam_playback(
        &mut self,
        playing_override: Option<bool>,
        pos_override: Option<u64>,
    ) {
        if !self.jam.is_host() || self.jam.applying {
            return;
        }
        self.jam.publish_gen = self.jam.publish_gen.saturating_add(1);
        let gen = self.jam.publish_gen;
        let anchor = self.playback_anchor(playing_override, pos_override);
        self.jam.last_host_anchor = Some(anchor.clone());
        if pos_override.is_some() {
            self.jam.last_host_seek_at = Some(Instant::now());
        }
        self.backend.state.jam.publish_playback(gen, anchor);
    }

    fn est_host_now(&self) -> u64 {
        unix_ms().saturating_add_signed(self.jam.clock_offset_ms)
    }

    fn poll_guest_seek(&mut self, ctx: &egui::Context) {
        let was_busy = self.jam.guest_seek.busy();
        self.jam.guest_seek.poll();
        if was_busy && !self.jam.guest_seek.busy() {
            // The seek may have waited on decode. Re-measure against the
            // host's position now, not the one we aimed at when it started.
            self.jam.pending_sync = true;
            self.jam.drift.reset();
            self.poll_now(ctx);
        }
    }

    fn sync_guest_to_anchor(&mut self, ctx: &egui::Context, force: bool) {
        if !self.jam.is_guest() || self.player.awaiting.is_some() {
            return;
        }
        let Some(anchor) = self.jam.anchor.clone() else {
            return;
        };
        self.sync_transport(ctx, anchor.playing);
        if self.player.awaiting.is_some() || self.jam.guest_seek.busy() {
            return;
        }
        let dur = self
            .current_entry()
            .and_then(|e| e.duration_ms)
            .unwrap_or(0);
        let expected = cap_pos(expected_position_ms(&anchor, self.est_host_now()), dur);
        let local = self
            .live_audio_pos_ms()
            .unwrap_or_else(|| self.player.clock.est());
        let drift = expected as i64 - local as i64;
        if force {
            self.jam.drift.reset();
        }
        if self.jam.drift.should_seek(drift) {
            self.guest_local_seek(ctx, expected);
        }
    }

    /// Seek this machine only. Going through `on_seek` would either refuse
    /// the guest or forward the seek back to the host.
    fn guest_local_seek(&mut self, ctx: &egui::Context, target_ms: u64) {
        if self.jam.guest_seek.busy() {
            return;
        }
        self.jam.applying = true;
        self.player.clock.mark_seek(target_ms);
        let state = self.backend.state.clone();
        let rx = self.backend.call(ctx, async move {
            shell::commands::player_seek(&state, target_ms)
                .await
                .map(|_| ())
        });
        self.jam.guest_seek.start(rx);
        self.jam.applying = false;
    }

    fn sync_transport(&mut self, ctx: &egui::Context, want_playing: bool) {
        let playing = self
            .player
            .report
            .as_ref()
            .map(|r| r.playing)
            .unwrap_or(false);
        if want_playing == playing {
            return;
        }
        self.jam.applying = true;
        if want_playing {
            self.player.user_paused = false;
            if !self.player.engine_has_source {
                if let Some(i) = self.player.index {
                    self.jam.pending_sync = true;
                    self.jam.drift.reset();
                    self.jump_to(ctx, i);
                }
            } else {
                let state = self.backend.state.clone();
                self.backend.fire(ctx, "jam resume", async move {
                    shell::commands::player_resume(&state).await
                });
            }
        } else {
            self.player.user_paused = true;
            let state = self.backend.state.clone();
            self.backend.fire(ctx, "jam pause", async move {
                shell::commands::player_pause(&state).await
            });
        }
        self.jam.applying = false;
    }

    fn prefetch_upcoming(&self, index: usize) {
        let items: Vec<QueueItem> = self
            .player
            .queue
            .iter()
            .skip(index + 1)
            .take(3)
            .map(|e| e.item.clone())
            .collect();
        if items.is_empty() {
            return;
        }
        let state = self.backend.state.clone();
        self.backend.spawn(async move {
            shell::commands::prefetch_ids(&state, items).await;
        });
    }

    fn playback_anchor(
        &self,
        playing_override: Option<bool>,
        pos_override: Option<u64>,
    ) -> PlaybackAnchor {
        // A jump has started and the engine still reports the previous
        // song. Publishing that position would park everyone at the old
        // track's playhead.
        let loading =
            self.player.awaiting.is_some() && playing_override.is_none() && pos_override.is_none();
        let playing = if loading {
            true
        } else {
            playing_override.unwrap_or_else(|| {
                self.player
                    .report
                    .as_ref()
                    .map(|r| r.playing)
                    .unwrap_or(false)
            })
        };
        let position_ms = if loading {
            0
        } else if let Some(pos) = pos_override {
            pos
        } else {
            self.live_audio_pos_ms()
                .unwrap_or_else(|| self.player.clock.est())
        };
        let track_uid = self.current_entry().and_then(|e| {
            if e.item.kind == "file" || e.jam_uid == 0 {
                None
            } else {
                Some(e.jam_uid)
            }
        });
        PlaybackAnchor {
            playing,
            position_ms,
            anchor_host_ms: unix_ms(),
            track_uid,
            loop_one: self.settings.loop_one,
            shuffle: self.settings.shuffle,
        }
    }

    fn assign_uids(&mut self) {
        let me = self.jam.peer_id.clone();
        let name = self.jam_name();
        for entry in &mut self.player.queue {
            if entry.item.kind == "file" {
                continue;
            }
            if entry.jam_uid == 0 {
                self.jam.next_uid = self.jam.next_uid.saturating_add(1);
                entry.jam_uid = self.jam.next_uid;
            }
            if entry.added_by_id.is_empty() {
                entry.added_by_id.clone_from(&me);
                entry.added_by.clone_from(&name);
            }
        }
    }

    fn jam_submit(&mut self, _ctx: &egui::Context, submit: Submit) {
        let action = match submit {
            Submit::Append(entries) => JamAction::QueueAppend {
                tracks: entries.iter().map(track_from_entry).collect(),
            },
            Submit::PlayNext(entries) => JamAction::QueuePlayNext {
                tracks: entries.iter().map(track_from_entry).collect(),
            },
            Submit::Replace { entries, start } => {
                let n = entries.len();
                JamAction::Replace {
                    tracks: entries.iter().map(track_from_entry).collect(),
                    start: start.min(n.saturating_sub(1)),
                }
            }
        };
        let state = self.backend.state.clone();
        self.backend.spawn(async move {
            let action = resolve_action(&state, action).await;
            state.jam.send_action(action);
        });
    }

    fn restore_personal(&mut self, _ctx: &egui::Context, saved: SavedQueue) {
        let dur = saved
            .index
            .and_then(|i| saved.queue.get(i))
            .and_then(|e| e.duration_ms)
            .unwrap_or(0);
        self.player.queue = saved.queue;
        self.player.index = saved.index;
        self.player.restored = Some((saved.position_ms, dur));
        self.player.clock.seed(saved.position_ms, dur);
        self.player.report = None;
        self.player.user_paused = true;
        self.player.awaiting = None;
        self.player.ended_handled = None;
        let items: Vec<QueueItem> = self.player.queue.iter().map(|e| e.item.clone()).collect();
        let idx = self.player.index.unwrap_or(0);
        let state = self.backend.state.clone();
        self.backend.spawn(async move {
            let _ = state.native.set_queue(items, idx);
            let _ = state.native.stop();
        });
        self.persist_now();
    }
}

fn cap_pos(pos: u64, dur: u64) -> u64 {
    if dur == 0 {
        pos
    } else {
        pos.min(dur)
    }
}

fn track_from_entry(entry: &MirrorEntry) -> JamTrack {
    JamTrack {
        uid: entry.jam_uid,
        catalog_id: entry.item.id.clone(),
        title: entry.item.title.clone().unwrap_or_default(),
        artist: entry.item.artist.clone().unwrap_or_default(),
        album: entry.album.clone().unwrap_or_default(),
        duration_ms: entry.duration_ms.unwrap_or(0),
        art_url: entry.art.clone(),
        isrc: None,
        added_by: entry.added_by.clone(),
        added_by_id: entry.added_by_id.clone(),
        radio: entry.radio,
    }
}

fn mirror_from_track(track: JamTrack) -> MirrorEntry {
    MirrorEntry {
        item: QueueItem {
            id: track.catalog_id,
            kind: "song".into(),
            title: (!track.title.is_empty()).then_some(track.title),
            artist: (!track.artist.is_empty()).then_some(track.artist),
        },
        radio: track.radio,
        art: track.art_url.filter(|u| !u.is_empty()),
        duration_ms: (track.duration_ms > 0).then_some(track.duration_ms),
        album: (!track.album.is_empty()).then_some(track.album),
        genres: Vec::new(),
        jam_uid: track.uid,
        added_by: track.added_by,
        added_by_id: track.added_by_id,
    }
}

async fn resolve_tracks(state: &shell::AppState, mut tracks: Vec<JamTrack>) -> Vec<JamTrack> {
    for track in &mut tracks {
        if ApiClient::is_library_song_id(&track.catalog_id) {
            if let Ok(id) = shell::commands::resolve_track_id(state, track.catalog_id.clone()).await
            {
                track.catalog_id = id;
            }
        }
    }
    tracks
}

async fn resolve_action(state: &shell::AppState, action: JamAction) -> JamAction {
    match action {
        JamAction::QueueAppend { tracks } => JamAction::QueueAppend {
            tracks: resolve_tracks(state, tracks).await,
        },
        JamAction::QueuePlayNext { tracks } => JamAction::QueuePlayNext {
            tracks: resolve_tracks(state, tracks).await,
        },
        JamAction::Replace { tracks, start } => JamAction::Replace {
            tracks: resolve_tracks(state, tracks).await,
            start,
        },
        other => other,
    }
}

pub fn default_jam_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Listener".into())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use shell::jam::{JamRole, JamSnapshot, JamTrack, PlaybackAnchor};
    use shell::native_player::PlayerReport;

    use super::*;
    use crate::player::tests::test_app;
    use crate::player::MirrorEntry;

    fn song(uid: u64) -> JamTrack {
        JamTrack {
            uid,
            catalog_id: "1811922756".into(),
            title: "Kool-Aid".into(),
            artist: "Bring Me The Horizon".into(),
            album: String::new(),
            duration_ms: 208_000,
            art_url: None,
            isrc: None,
            added_by: "Ada".into(),
            added_by_id: "host".into(),
            radio: false,
        }
    }

    fn row(track: &JamTrack) -> MirrorEntry {
        mirror_from_track(track.clone())
    }

    fn room(revision: u64, playing: bool, position_ms: u64) -> JamSnapshot {
        JamSnapshot {
            revision,
            guests_can_control: true,
            participants: Vec::new(),
            queue: vec![song(7)],
            current_index: Some(0),
            playback: PlaybackAnchor {
                playing,
                position_ms,
                anchor_host_ms: unix_ms(),
                track_uid: Some(7),
                loop_one: false,
                shuffle: false,
            },
        }
    }

    fn playing_report(pos: u64) -> PlayerReport {
        PlayerReport {
            playing: true,
            track_id: Some("1811922756".into()),
            position_ms: pos,
            duration_ms: 208_000,
            engine_has_source: true,
            ..PlayerReport::default()
        }
    }

    #[test]
    fn host_pause_publishes_the_clicked_state_not_the_old_report() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        app.jam.role = Some(JamRole::Host);
        app.player.report = Some(playing_report(10_000));
        app.player.report_at = Instant::now();
        app.on_play_pause(&ctx);
        let anchor = app.jam.last_host_anchor.clone().expect("pause published");
        assert!(!anchor.playing);
        assert!(anchor.position_ms >= 10_000);
    }

    #[test]
    fn host_jump_does_not_publish_the_previous_track_position() {
        let mut app = test_app();
        app.jam.role = Some(JamRole::Host);
        app.player.awaiting = Some(("next".into(), Instant::now()));
        app.player.report = Some(playing_report(180_000));
        app.player.report_at = Instant::now();
        let anchor = app.playback_anchor(None, None);
        assert!(anchor.playing);
        assert_eq!(anchor.position_ms, 0);
    }

    #[test]
    fn host_report_republishes_an_external_pause() {
        let mut app = test_app();
        app.jam.role = Some(JamRole::Host);
        app.jam.last_host_anchor = Some(PlaybackAnchor {
            playing: true,
            position_ms: 10_000,
            anchor_host_ms: unix_ms(),
            track_uid: Some(7),
            loop_one: false,
            shuffle: false,
        });
        app.player.report = Some(PlayerReport {
            playing: false,
            position_ms: 12_000,
            ..playing_report(12_000)
        });
        app.player.report_at = Instant::now();
        app.on_host_report(false, true, false);
        let anchor = app.jam.last_host_anchor.clone().expect("republished");
        assert!(!anchor.playing);
        assert_eq!(anchor.position_ms, 12_000);
    }

    #[test]
    fn guest_drift_seeks_locally_even_when_guests_can_control() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        let track = song(7);
        app.jam.role = Some(JamRole::Guest);
        app.jam.guests_can_control = true;
        app.jam.playing_uid = Some(7);
        app.jam.took_over = true;
        app.player.engine_has_source = true;
        app.player.queue = vec![row(&track)];
        app.player.index = Some(0);
        app.player.report = Some(playing_report(45_000));
        app.player.report_at = Instant::now();
        app.apply_remote(&ctx, room(1, true, 119_000), unix_ms());
        assert!(
            app.player.awaiting.is_none(),
            "the same jam row must not reload from the start"
        );
        assert!(app.jam.guest_seek.busy(), "the guest seeks its own player");
        assert!(
            app.player.clock.est() > 100_000,
            "local clock jumps to the host position, est={}",
            app.player.clock.est()
        );
    }

    #[test]
    fn guest_pause_on_the_same_row_does_not_restart_the_track() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        let track = song(7);
        app.jam.role = Some(JamRole::Guest);
        app.jam.playing_uid = Some(7);
        app.jam.took_over = true;
        app.player.engine_has_source = true;
        app.player.queue = vec![row(&track)];
        app.player.index = Some(0);
        app.player.report = Some(playing_report(119_000));
        app.player.report_at = Instant::now();
        app.apply_remote(&ctx, room(1, false, 119_000), unix_ms());
        assert!(app.player.awaiting.is_none());
        assert!(app.player.user_paused);
        assert!(!app.jam.guest_seek.busy());
    }

    #[test]
    fn guest_joins_mid_song_and_snaps_after_the_track_loads() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        app.jam.role = Some(JamRole::Guest);
        app.apply_remote(&ctx, room(1, true, 119_000), unix_ms());
        assert!(app.jam.pending_sync);
        assert!(app.player.awaiting.is_some(), "the row has to load first");
        assert!(
            !app.jam.guest_seek.busy(),
            "seeking during the load is dropped"
        );

        app.player.awaiting = None;
        app.player.engine_has_source = true;
        app.player.report = Some(playing_report(0));
        app.player.report_at = Instant::now();
        app.on_guest_report(&ctx, true);
        assert!(app.jam.guest_seek.busy());
        assert!(app.player.clock.est() > 100_000);
    }

    #[test]
    fn guest_stops_when_the_host_clears_the_queue() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        let track = song(7);
        app.jam.role = Some(JamRole::Guest);
        app.jam.playing_uid = Some(7);
        app.jam.took_over = true;
        app.player.engine_has_source = true;
        app.player.queue = vec![row(&track)];
        app.player.index = Some(0);
        app.player.report = Some(playing_report(30_000));
        let mut cleared = room(2, false, 0);
        cleared.queue.clear();
        cleared.current_index = None;
        app.apply_remote(&ctx, cleared, unix_ms());
        assert!(app.jam.playing_uid.is_none());
        assert!(app.player.user_paused);
        assert!(app.player.queue.is_empty());
    }
}
