//! Jam UI: host publishes the queue it already owns, guests follow it.
//!
//! Playback stays on each person's own Apple Music session. This module
//! only decides whose button press is allowed to move that queue.

use std::time::Instant;

use apple_music_core::api::ApiClient;
use apple_music_core::playback::QueueItem;
use shell::jam::{
    expected_position_ms, unix_ms, DriftGate, JamAction, JamEvent, JamRole, JamSnapshot, JamTrack,
    Participant, PathKind, PlaybackAnchor, PublishUpdate,
};

use crate::app::SonoraApp;
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
    Replace { entries: Vec<MirrorEntry>, start: usize },
}

impl SonoraApp {
    pub(crate) fn tick_jam(&mut self, ctx: &egui::Context) {
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
        self.backend.fire(ctx, "jam", async move {
            state.jam.join(ticket, name).await
        });
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
        let showing_queue = (self.settings.queue_open || self.settings.jam_open) && !self.jam.show_jam;
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

    pub(crate) fn jam_guest_add(&mut self, ctx: &egui::Context, entries: Vec<MirrorEntry>, next: bool) -> bool {
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
        let open = matches!(
            action,
            JamAction::QueueRemove { .. }
        );
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
        let playback = self.playback_anchor();
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
                path,
                rtt_ms,
            } => {
                self.jam.path = path;
                self.jam.rtt_ms = rtt_ms;
                self.jam.notice = None;
                self.apply_remote(ctx, snapshot, est_host_now_ms);
            }
            JamEvent::Heartbeat {
                est_host_now_ms,
                playing,
                track_uid,
                path,
                rtt_ms,
                ..
            } => {
                self.jam.path = path;
                self.jam.rtt_ms = rtt_ms;
                self.on_heartbeat(ctx, playing, track_uid, est_host_now_ms);
            }
            JamEvent::GuestAction {
                name, action, ..
            } => {
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
                    let unexpected = !reason.is_empty()
                        && reason != "jam ended"
                        && reason != "left the jam";
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

    fn apply_remote(&mut self, ctx: &egui::Context, snap: JamSnapshot, est_host_now_ms: u64) {
        if !self.jam.is_guest() {
            return;
        }
        if self.jam.took_over && snap.revision < self.jam.last_revision {
            return;
        }
        let structural = !self.jam.took_over || snap.revision != self.jam.last_revision;
        self.jam.last_revision = snap.revision;
        self.jam.guests_can_control = snap.guests_can_control;
        self.jam.participants = snap.participants;
        self.jam.anchor = Some(snap.playback.clone());
        if !structural {
            self.follow_anchor(ctx, est_host_now_ms);
            return;
        }
        self.jam.took_over = true;
        self.jam.applying = true;
        let entries: Vec<MirrorEntry> = snap.queue.iter().cloned().map(mirror_from_track).collect();
        let index = snap.current_index.filter(|i| *i < entries.len());
        let incoming_id = index.and_then(|i| entries.get(i)).map(|e| e.item.id.clone());
        let already = self
            .player
            .report
            .as_ref()
            .and_then(|r| r.track_id.clone())
            .or_else(|| self.current_id());
        self.player.queue = entries;
        self.player.index = index;
        self.player.origin = None;
        if let Some(i) = index {
            let dur = self.player.queue[i].duration_ms.unwrap_or(0);
            let pos = cap_pos(expected_position_ms(&snap.playback, est_host_now_ms), dur);
            if already.as_deref() != incoming_id.as_deref() {
                self.jump_to(ctx, i);
                if pos > 1_500 {
                    self.on_seek(ctx, pos);
                }
            } else {
                self.sync_transport(ctx, snap.playback.playing);
                self.correct_drift(ctx, pos);
            }
            self.prefetch_upcoming(i);
        }
        self.jam.applying = false;
    }

    fn on_heartbeat(
        &mut self,
        ctx: &egui::Context,
        playing: bool,
        track_uid: Option<u64>,
        est_host_now_ms: u64,
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
        self.sync_transport(ctx, playing);
        if playing {
            let dur = self
                .current_entry()
                .and_then(|e| e.duration_ms)
                .unwrap_or(0);
            let pos = cap_pos(
                expected_position_ms(
                    &PlaybackAnchor {
                        playing: true,
                        ..anchor
                    },
                    est_host_now_ms,
                ),
                dur,
            );
            self.correct_drift(ctx, pos);
        }
    }

    fn follow_anchor(&mut self, ctx: &egui::Context, est_host_now_ms: u64) {
        let Some(anchor) = self.jam.anchor.clone() else {
            return;
        };
        self.sync_transport(ctx, anchor.playing);
        if anchor.playing {
            let dur = self
                .current_entry()
                .and_then(|e| e.duration_ms)
                .unwrap_or(0);
            self.correct_drift(ctx, cap_pos(expected_position_ms(&anchor, est_host_now_ms), dur));
        }
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

    fn correct_drift(&mut self, ctx: &egui::Context, expected: u64) {
        let drift = expected as i64 - self.player.clock.est() as i64;
        if self.jam.drift.should_seek(drift) {
            self.on_seek(ctx, expected);
        }
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

    fn playback_anchor(&self) -> PlaybackAnchor {
        let playing = self
            .player
            .report
            .as_ref()
            .map(|r| r.playing)
            .unwrap_or(false);
        let position_ms = if let Some(r) = &self.player.report {
            if playing {
                self.player.clock.est()
            } else {
                r.position_ms
            }
        } else {
            self.player.clock.est()
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
