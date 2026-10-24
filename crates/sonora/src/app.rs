//! Application state and the persistent chrome (sidebar, top bar, player bar).

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use apple_music_core::models::{
    AlbumDetail, ArtistDetail, Playlist, PlaylistDetail, SearchResults,
};
use egui::{vec2, Margin, Pos2, RichText, Sense, Stroke};

use crate::artwork;
use crate::backend::{Backend, Pending};
use crate::fonts;
use crate::icons;
use crate::player::{MirrorEntry, PlayerUi, QueueOrigin};
use crate::theme::c;
use crate::widgets;

/// Sidebar footer build tag.
pub const BUILD: &str = concat!("build ", env!("CARGO_PKG_VERSION"));

const ACTIVITY_CAP: usize = 120;
const SIDEBAR_W: f32 = 248.0;

/// Which screen the content area shows. Detail pages carry their id, so
/// re-clicking the same card is a no-op and `history` restores the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    Browse,
    Search,
    Playlists,
    Queue,
    Lyrics,
    Settings,
    Album(String),
    Artist(String),
    Playlist(String),
}

impl View {
    /// Top-level pages the sidebar can highlight (`true` for them only).
    pub fn is_root(&self) -> bool {
        matches!(
            self,
            Self::Browse | Self::Playlists | Self::Queue | Self::Lyrics | Self::Settings
        )
    }
}

// ---------------------------------------------------------------------------
// Settings — the old `aml-settings` localStorage object as native JSON in
// the config dir (old localStorage values do not migrate; accepted gap).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FsLayout {
    Vertical,
    Horizontal,
}

impl FsLayout {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vertical => "vertical",
            Self::Horizontal => "horizontal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImLayout {
    Split,
    Overlay,
}

impl ImLayout {
    pub fn label(self) -> &'static str {
        match self {
            Self::Split => "split (cover + lyrics)",
            Self::Overlay => "overlay (lyrics over background)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImBg {
    Flow,
    Static,
    Motion,
    Dynamic,
    Gradient,
}

impl ImBg {
    pub fn label(self) -> &'static str {
        match self {
            Self::Flow => "immersive (old)",
            Self::Static => "blurred still cover",
            Self::Motion => "animated cover",
            Self::Dynamic => "immersive",
            Self::Gradient => "colors only",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImCover {
    Animated,
    Still,
    Hidden,
}

impl ImCover {
    pub fn label(self) -> &'static str {
        match self {
            Self::Animated => "animated",
            Self::Still => "still",
            Self::Hidden => "hidden",
        }
    }
}

/// Container-level default: any field missing from the JSON falls back to
/// [`Settings::default`] (old settings files stay forward-compatible).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    // Display
    pub fs_lyrics: bool,
    pub lyrics_focus: bool,
    pub fs_layout: FsLayout,
    pub native_fs: bool,
    // Immersive fullscreen
    pub immersive: bool,
    pub im_layout: ImLayout,
    pub im_bg: ImBg,
    pub motion_covers: bool,
    pub im_cover: ImCover,
    pub im_blur: f32,
    pub im_dim: f32,
    // Integrations
    pub notifications: bool,
    pub discord: bool,
    pub discord_app_id: String,
    // Debug
    pub debug: bool,
    // Playback (UI-side)
    pub infinite: bool,
    pub loop_one: bool,
    pub volume: f32,
    // Auth
    pub advanced_auth: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fs_lyrics: true,
            lyrics_focus: false,
            fs_layout: FsLayout::Vertical,
            native_fs: false,
            immersive: true,
            im_layout: ImLayout::Split,
            im_bg: ImBg::Dynamic,
            motion_covers: true,
            im_cover: ImCover::Animated,
            im_blur: 24.0,
            im_dim: 55.0,
            notifications: false,
            discord: false,
            discord_app_id: String::new(),
            debug: false,
            infinite: true,
            loop_one: false,
            volume: 80.0,
            advanced_auth: false,
        }
    }
}

// ---------------------------------------------------------------------------

pub struct ActivityRow {
    pub stamp: String,
    pub text: String,
    pub dbg: bool,
}

/// Data slots for the views — one in-flight backend call each. Views
/// auto-start a slot while it is empty, idle and error-free (`busy()`
/// plus `value`/`error` gate this), and re-start it after `clear()`.
#[derive(Default)]
pub struct Catalog {
    /// Browse charts (Top Songs / Top Albums / charts playlists).
    pub charts: Pending<SearchResults>,
    /// Search results for `SonoraApp::search_term`.
    pub search: Pending<SearchResults>,
    pub albums: Pending<AlbumDetail>,
    pub artists: Pending<ArtistDetail>,
    pub playlist: Pending<PlaylistDetail>,
    /// Library playlists (Playlists view).
    pub playlists: Pending<Vec<Playlist>>,
    /// One-shot fetch for the playlist-grid context menu's "Play".
    pub card_play: Pending<PlaylistDetail>,
    /// Track-menu "Artist →"/"Album →" name resolution (old
    /// openArtistByName/openAlbumByName: search → first hit → detail).
    pub resolve: Pending<(ResolveKind, SearchResults)>,
    /// Detail ids currently open (empty = closed).
    pub album_id: String,
    pub artist_id: String,
    pub playlist_id: String,
    /// Modal dialog state (create / rename / delete playlist).
    pub dialog: Option<Dialog>,
}

/// What a `Catalog::resolve` search should open once it lands (the old
/// openArtistByName/openAlbumByName one-shot flows).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveKind {
    /// Open the artist whose name matches (exact match preferred, else
    /// the first hit — old openArtistByName).
    Artist { name: String },
    /// Open the first album hit for `artist album` (old openAlbumByName).
    Album { artist: String, album: String },
}

/// Modal dialogs — egui has no native prompts, so each carries its state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dialog {
    /// `add_song` — track to append right after creation (track-menu
    /// "+ New playlist…", the old `window.prompt` flow).
    NewPlaylist {
        name: String,
        add_song: Option<String>,
    },
    RenamePlaylist {
        id: String,
        name: String,
    },
    DeletePlaylist {
        id: String,
        name: String,
    },
    /// Duplicate a playlist: create `name` (pre-filled `… (copy)`), then
    /// append `ids` (old `duplicatePlaylistFlow`).
    DuplicatePlaylist {
        ids: Vec<String>,
        source: String,
        name: String,
    },
}

/// Sign-in state, mirrored into the sidebar and the account card.
pub struct AuthUi {
    /// `auth_state` result (None until the first probe answers).
    pub signed_in: Option<bool>,
    /// `token_status` result ("official (env)" / "shared web-player …").
    pub token_status: Option<String>,
    probe: Pending<bool>,
    token_probe: Pending<String>,
    pub signin: Pending<String>,
    /// Re-run both probes once their in-flight calls settle.
    pub(crate) probe_dirty: bool,
}

impl Default for AuthUi {
    fn default() -> Self {
        Self {
            signed_in: None,
            token_status: None,
            probe: Pending::default(),
            token_probe: Pending::default(),
            signin: Pending::default(),
            probe_dirty: true,
        }
    }
}

pub struct SonoraApp {
    pub view: View,
    /// Back-stack for detail pages (album/artist/playlist).
    pub history: Vec<View>,
    pub search_input: String,
    pub search_term: String,
    pub status: String,
    pub activity: VecDeque<ActivityRow>,
    pub settings: Settings,
    pub fullscreen: bool,
    pub mut_input: String,
    pub auth_url_input: String,
    seek: f32,
    /// Tokio runtime + shared `shell` state.
    pub backend: Backend,
    /// Player status, transport actions, queue mirror.
    pub player: PlayerUi,
    /// View data slots.
    pub catalog: Catalog,
    /// Sign-in state and its in-flight calls.
    pub auth: AuthUi,
    /// Decoded cover textures (disk-backed fetch, 48 MB LRU).
    pub covers: artwork::Covers,
    /// Timed-lyrics engine + fetch/cache (old `lyric` + friends).
    pub lyrics: crate::lyrics::LyricsUi,
    /// Last JSON written to disk (settings change detection).
    settings_saved: Option<String>,
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

impl Default for SonoraApp {
    fn default() -> Self {
        Self::new(Backend::new())
    }
}

impl SonoraApp {
    pub fn new(backend: Backend) -> Self {
        let settings = crate::store::load();
        let mut app = Self {
            view: View::Browse,
            history: Vec::new(),
            search_input: String::new(),
            search_term: String::new(),
            status: String::new(),
            activity: VecDeque::new(),
            settings,
            fullscreen: false,
            mut_input: String::new(),
            auth_url_input: String::new(),
            seek: 0.0,
            backend,
            player: PlayerUi::default(),
            catalog: Catalog::default(),
            auth: AuthUi::default(),
            covers: artwork::Covers::default(),
            lyrics: crate::lyrics::LyricsUi::default(),
            settings_saved: None,
        };
        app.log(format!("sonora {} — egui shell", env!("CARGO_PKG_VERSION")));
        // Session boot (old restoreSession): seed the gray set from the
        // backend's persisted dead ids before anything renders, then
        // restore the persisted queue — paused near the last position,
        // never auto-plays. The backend dies with the process, so there
        // is no live audio to adopt on a fresh launch.
        if let Ok(ids) = shell::commands::dead_ids(&app.backend.state) {
            for id in ids {
                if !id.is_empty() {
                    app.player.dead.insert(id);
                }
            }
        }
        if let Some(s) = crate::store::load_session() {
            if !s.queue.is_empty() {
                let entries: Vec<MirrorEntry> = s
                    .queue
                    .into_iter()
                    .map(crate::store::SessionEntry::into_mirror)
                    .collect();
                let len = entries.len();
                let idx = s.queue_index.max(0) as usize;
                let idx = idx.min(len - 1);
                app.player.origin = QueueOrigin::from_entries(&entries);
                app.player.restored = Some((s.position_ms, entries[idx].duration_ms.unwrap_or(0)));
                // Seed the smooth clock so `est` is sane before the
                // first status report lands (paused anchor).
                app.player
                    .clock
                    .seed(s.position_ms, entries[idx].duration_ms.unwrap_or(0));
                app.player.index = Some(idx);
                app.player.queue = entries;
                app.log("Queue restored — press play to resume");
            }
        }
        app
    }

    pub fn log(&mut self, text: impl Into<String>) {
        self.activity.push_back(ActivityRow {
            stamp: stamp(),
            text: text.into(),
            dbg: false,
        });
        while self.activity.len() > ACTIVITY_CAP {
            self.activity.pop_front();
        }
    }

    pub fn log_dbg(&mut self, text: impl Into<String>) {
        self.activity.push_back(ActivityRow {
            stamp: stamp(),
            text: text.into(),
            dbg: true,
        });
        while self.activity.len() > ACTIVITY_CAP {
            self.activity.pop_front();
        }
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    pub fn run_search(&mut self) {
        let term = self.search_input.trim().to_owned();
        if term.is_empty() {
            return;
        }
        self.log(format!("search “{term}”"));
        self.search_term = term;
        // Results refetch for the new term (the slot auto-starts in-view).
        self.catalog.search.clear();
        self.history.clear();
        self.view = View::Search;
    }

    // -- navigation ----------------------------------------------------------

    /// Navigate to a top-level view (clears the detail back-stack).
    pub fn go(&mut self, view: View) {
        self.history.clear();
        self.view = view;
    }

    /// Open a detail page, pushing the current page onto the back-stack.
    pub fn open(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.history.push(std::mem::replace(&mut self.view, view));
    }

    /// Back button target: pop the detail history (no-op on root views).
    pub fn back(&mut self) {
        if let Some(prev) = self.history.pop() {
            self.view = prev;
        }
    }

    pub fn open_album(&mut self, id: &str) {
        if self.view != View::Album(id.to_owned()) {
            self.open(View::Album(id.to_owned()));
        }
        if self.catalog.album_id != id {
            self.catalog.album_id = id.to_owned();
            self.catalog.albums.clear();
        }
    }

    pub fn open_artist(&mut self, id: &str) {
        if self.view != View::Artist(id.to_owned()) {
            self.open(View::Artist(id.to_owned()));
        }
        if self.catalog.artist_id != id {
            self.catalog.artist_id = id.to_owned();
            self.catalog.artists.clear();
        }
    }

    pub fn open_playlist(&mut self, id: &str) {
        if self.view != View::Playlist(id.to_owned()) {
            self.open(View::Playlist(id.to_owned()));
        }
        if self.catalog.playlist_id != id {
            self.catalog.playlist_id = id.to_owned();
            self.catalog.playlist.clear();
        }
    }

    // -- name-based flows (track menu: Lyrics / Artist→ / Album→) ----------

    /// Track-menu "Lyrics": show this track's lyrics (old openLyrics —
    /// fetches that track's lyrics, not the playing one's).
    pub fn open_lyrics_for(&mut self, ctx: &egui::Context, id: &str, title: &str, artist: &str) {
        self.lyrics.open_for(ctx, &self.backend, id, title, artist);
        self.go(View::Lyrics);
    }

    /// Track-menu "Artist → name": search, then open the best match
    /// (old openArtistByName).
    pub fn open_artist_by_name(&mut self, ctx: &egui::Context, name: &str) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        self.set_status(format!("Finding artist “{name}”…"));
        let state = self.backend.state.clone();
        let rx = self.backend.call(ctx, async move {
            shell::commands::search_catalog(&state, name.clone())
                .await
                .map(|r| (ResolveKind::Artist { name }, r))
        });
        self.catalog.resolve.start(rx);
    }

    /// Track-menu "Album → name": search `artist album`, open the first
    /// hit (old openAlbumByName).
    pub fn open_album_by_name(&mut self, ctx: &egui::Context, artist: &str, album: &str) {
        let album = album.trim().to_owned();
        if album.is_empty() {
            return;
        }
        let artist = artist.trim().to_owned();
        let term = if artist.is_empty() {
            album.clone()
        } else {
            format!("{artist} {album}")
        };
        self.set_status(format!("Finding album “{album}”…"));
        let state = self.backend.state.clone();
        let rx = self.backend.call(ctx, async move {
            shell::commands::search_catalog(&state, term)
                .await
                .map(|r| {
                    (
                        ResolveKind::Album {
                            artist: artist.clone(),
                            album,
                        },
                        r,
                    )
                })
        });
        self.catalog.resolve.start(rx);
    }

    /// Land a name resolution (called every frame from `logic`): open
    /// the detail page or report that nothing matched.
    pub fn tick_resolve(&mut self) {
        self.catalog.resolve.poll();
        let Some((kind, results)) = self.catalog.resolve.value.take() else {
            if let Some(err) = self.catalog.resolve.error.take() {
                self.set_status(err);
            }
            return;
        };
        match kind {
            ResolveKind::Artist { name } => {
                let hit = results
                    .artists
                    .iter()
                    .find(|a| a.name.eq_ignore_ascii_case(&name))
                    .or_else(|| results.artists.first());
                match hit {
                    Some(a) => {
                        self.log(format!("artist → {}", a.name));
                        let id = a.id.clone();
                        self.open_artist(&id);
                    }
                    None => self.set_status(format!("No artist found for {name}")),
                }
            }
            ResolveKind::Album { album, .. } => match results.albums.first() {
                Some(a) => {
                    self.log(format!("album → {}", a.title));
                    let id = a.id.clone();
                    self.open_album(&id);
                }
                None => self.set_status(format!("No album found for {album}")),
            },
        }
    }

    /// Lyrics auto-fetch for the audible track (old autoFetchLyrics,
    /// which ran from the track-change path only) plus the in-flight
    /// fetch landing. The track duration feeds the open-ended last
    /// lyric window.
    pub fn tick_lyrics(&mut self, ctx: &egui::Context) {
        if let Some(line) = self.lyrics.tick_fetch() {
            self.log_dbg(line);
        }
        self.lyrics.set_duration(self.player.clock.duration());
        let cur = self.player.report.as_ref().and_then(|r| {
            Some((
                r.track_id.clone()?,
                r.title.clone().unwrap_or_default(),
                r.artist.clone().unwrap_or_default(),
            ))
        });
        match cur {
            Some((id, title, artist)) => {
                self.lyrics
                    .note_playing(ctx, &self.backend, &id, &title, &artist);
            }
            None => self.lyrics.forget_playing(),
        }
    }

    // -- auth actions (account card / sidebar) --------------------------------

    /// "Get authorize URL": fetch it, open it in the system browser, and
    /// copy it into the manual field (old `#authBtn` behavior).
    pub fn auth_get_url(&mut self, ctx: &egui::Context) {
        let state = self.backend.state.clone();
        self.backend.fire_msg(ctx, "authorize URL", async move {
            let url = shell::commands::authorize_url(&state).await?;
            let _ = shell::open_url(url.clone());
            Ok(url)
        });
        self.set_status("Approve in the browser, then paste MUT / redirect URL below.");
    }

    /// Automatic sign-in: opens the browser on a one-shot localhost page
    /// and waits (up to 5 minutes) for the approval.
    pub fn auth_start_signin(&mut self, ctx: &egui::Context) {
        if self.auth.signin.busy() {
            self.set_status("Sign-in already in progress — finish or cancel it first.");
            return;
        }
        let state = self.backend.state.clone();
        let rx = self.backend.call(
            ctx,
            async move { shell::commands::start_signin(&state).await },
        );
        self.auth.signin.start(rx);
        self.set_status("Waiting for sign-in in the browser (up to 5 minutes)…");
    }

    pub fn auth_cancel_signin(&mut self, ctx: &egui::Context) {
        let state = self.backend.state.clone();
        self.backend.fire_msg(ctx, "cancel sign-in", async move {
            shell::commands::cancel_signin(&state)
        });
        self.auth.signin.clear();
        self.auth.probe_dirty = true;
    }

    /// Paste the full music user token.
    pub fn auth_submit_token(&mut self, ctx: &egui::Context) {
        let token = self.mut_input.trim().to_owned();
        if token.is_empty() {
            self.set_status("Paste a music user token first.");
            return;
        }
        let state = self.backend.state.clone();
        self.backend.fire_msg(ctx, "save MUT", async move {
            shell::commands::submit_user_token(&state, token).map(|_| "MUT saved.".to_string())
        });
        self.auth.probe_dirty = true;
    }

    /// Paste the redirect URL of an approved authorize flow.
    pub fn auth_submit_url(&mut self, ctx: &egui::Context) {
        let url = self.auth_url_input.trim().to_owned();
        if url.is_empty() {
            self.set_status("Paste the redirect URL first.");
            return;
        }
        let state = self.backend.state.clone();
        self.backend.fire_msg(ctx, "save redirect URL", async move {
            shell::commands::submit_auth_url(&state, url)
        });
        self.auth.probe_dirty = true;
    }

    pub fn auth_logout(&mut self, ctx: &egui::Context) {
        let state = self.backend.state.clone();
        self.backend.fire_msg(
            ctx,
            "logout",
            async move { shell::commands::logout(&state) },
        );
        self.auth.signed_in = Some(false);
        self.auth.probe_dirty = true;
    }

    /// Consume finished auth calls; start the probes while dirty.
    fn tick_auth(&mut self, ctx: &egui::Context) {
        self.auth.probe.poll();
        self.auth.token_probe.poll();
        self.auth.signin.poll();

        if let Some(signed) = self.auth.probe.value.take() {
            self.auth.signed_in = Some(signed);
        }
        if let Some(err) = self.auth.probe.error.take() {
            self.auth.signed_in = Some(false);
            self.log_dbg(format!("auth state: {err}"));
        }
        if let Some(tok) = self.auth.token_probe.value.take() {
            self.auth.token_status = Some(tok);
        }
        if let Some(msg) = self.auth.signin.value.take() {
            self.set_status(&msg);
            self.log(msg);
            self.auth.probe_dirty = true;
        }
        if let Some(err) = self.auth.signin.error.take() {
            self.set_status(&err);
            self.log(err);
        }

        if self.auth.probe_dirty
            && !self.auth.probe.busy()
            && !self.auth.token_probe.busy()
            && !self.auth.signin.busy()
        {
            self.auth.probe_dirty = false;
            let state = self.backend.state.clone();
            let rx = self
                .backend
                .call(ctx, async move { shell::commands::auth_state(&state) });
            self.auth.probe.start(rx);
            let state = self.backend.state.clone();
            let rx = self.backend.call(
                ctx,
                async move { shell::commands::token_status(&state).await },
            );
            self.auth.token_probe.start(rx);
        }
    }

    /// off → loop-one → infinite queue → off (the old cycle button).
    pub fn cycle_mode(&mut self) {
        if self.settings.loop_one {
            self.settings.loop_one = false;
            self.settings.infinite = true;
            self.set_status("Infinite queue on");
        } else if self.settings.infinite {
            self.settings.infinite = false;
            self.set_status("Playback mode off");
        } else {
            self.settings.loop_one = true;
            self.set_status("Looping the current track");
        }
        self.log("mode cycle");
    }

    // -- chrome -------------------------------------------------------------

    fn render_topbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("topbar")
            .frame(
                egui::Frame::new()
                    .fill(c::BG)
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                let mut run = false;
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.search_input)
                            .hint_text("Search songs, albums, playlists…")
                            .desired_width(f32::INFINITY),
                    );
                    if resp.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter)) {
                        run = true;
                    }
                    let search_btn = egui::Button::new(
                        RichText::new("Search")
                            .color(c::WHITE)
                            .font(fonts::strong_font(14.0)),
                    )
                    .fill(c::ACCENT)
                    .corner_radius(egui::CornerRadius::from(10.0))
                    .min_size(vec2(0.0, 38.0));
                    if ui.add(search_btn).clicked() {
                        run = true;
                    }
                });

                // hairline under the bar
                let r = ui.max_rect();
                ui.painter().line_segment(
                    [
                        Pos2::new(r.left(), r.bottom() - 0.5),
                        Pos2::new(r.right(), r.bottom() - 0.5),
                    ],
                    Stroke::new(1.0, c::LINE),
                );

                if run {
                    self.run_search();
                }
            });
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("sidebar")
            .exact_size(SIDEBAR_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(c::PANEL).inner_margin(14.0))
            .show(ui, |ui| {
                // brand
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("♪")
                            .font(fonts::strong_font(20.0))
                            .color(c::ACCENT),
                    );
                    ui.label(
                        RichText::new("Sonora")
                            .font(fonts::strong_font(20.0))
                            .color(c::TEXT),
                    );
                });
                ui.add_space(6.0);

                // nav
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
                    let items = [
                        (View::Browse, "Browse"),
                        (View::Playlists, "Playlists"),
                        (View::Queue, "Queue"),
                        (View::Lyrics, "Lyrics"),
                        (View::Settings, "Settings"),
                    ];
                    for (view, label) in items {
                        let selected = self.view == view;
                        if nav_button(ui, label, selected).clicked() {
                            self.go(view);
                        }
                    }
                });

                ui.add_space(10.0);
                ui.label(RichText::new(&self.status).size(12.0).color(c::DIM));
                ui.add_space(10.0);

                // Activity section
                section_line(ui);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("ACTIVITY")
                            .font(fonts::strong_font(12.0))
                            .color(c::DIM),
                    );
                    ui.checkbox(&mut self.settings.debug, "debug");
                });
                if self.settings.debug {
                    self.activity_log(ui);
                }

                ui.add_space(8.0);
                ui.label(RichText::new(BUILD).size(11.0).color(c::DIM));
                let signed = match self.auth.signed_in {
                    Some(true) => "Signed in",
                    Some(false) => "Not signed in",
                    None => "…",
                };
                ui.label(RichText::new(signed).size(12.0).color(c::DIM));
            });
    }

    fn activity_log(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("activity")
            .max_height(150.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for row in &self.activity {
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        &format!("{} ", row.stamp),
                        0.0,
                        egui::TextFormat {
                            font_id: fonts::body_font(11.0),
                            color: c::DIM,
                            ..Default::default()
                        },
                    );
                    job.append(
                        &row.text,
                        0.0,
                        egui::TextFormat {
                            font_id: fonts::body_font(11.0),
                            color: if row.dbg { c::DIM } else { c::TEXT },
                            ..Default::default()
                        },
                    );
                    ui.add(egui::Label::new(job));
                }
            });
    }

    fn render_playerbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("playerbar")
            .frame(
                egui::Frame::new()
                    .fill(c::PANEL)
                    .inner_margin(Margin::symmetric(14, 10)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().line_segment(
                    [
                        Pos2::new(r.left(), r.top() + 0.5),
                        Pos2::new(r.right(), r.top() + 0.5),
                    ],
                    Stroke::new(1.0, c::LINE),
                );

                ui.horizontal(|ui| {
                    // Cover: resolved art for the current track, else the
                    // music-note placeholder.
                    let (cover, _) = ui.allocate_exact_size(vec2(52.0, 52.0), Sense::click());
                    let bar_cover = self
                        .player
                        .report
                        .as_ref()
                        .and_then(|r| r.track_id.clone())
                        .and_then(|tid| self.player.art_by_track.get(&tid).cloned())
                        .and_then(|url| self.covers.get(ui.ctx(), &self.backend, &url, 60));
                    if let Some(tex) = &bar_cover {
                        egui::Image::new(egui::load::SizedTexture::new(tex.id(), vec2(52.0, 52.0)))
                            .corner_radius(8.0)
                            .paint_at(ui, cover);
                    } else {
                        ui.painter()
                            .rect_filled(cover, egui::CornerRadius::from(8.0), c::PANEL2);
                        icons::music_note(ui.painter(), cover, c::DIM);
                    }

                    // now-playing meta
                    let title = self.player.report.as_ref().and_then(|r| r.title.clone());
                    let artist = self.player.report.as_ref().and_then(|r| r.artist.clone());
                    ui.allocate_ui_with_layout(
                        vec2(240.0, 52.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| match (title, artist) {
                            (Some(t), Some(a)) if !a.is_empty() => {
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(t)
                                        .font(fonts::strong_font(14.0))
                                        .color(c::TEXT),
                                );
                                ui.label(RichText::new(a).size(12.0).color(c::DIM));
                            }
                            (Some(t), _) => {
                                ui.add_space(17.0);
                                ui.label(
                                    RichText::new(t)
                                        .font(fonts::strong_font(14.0))
                                        .color(c::TEXT),
                                );
                            }
                            _ => {
                                ui.add_space(17.0);
                                ui.label(
                                    RichText::new("Not playing.")
                                        .font(fonts::strong_font(14.0))
                                        .color(c::TEXT),
                                );
                            }
                        },
                    );

                    // transport
                    ui.add_space(10.0);
                    let have = self.player.report.is_some() || !self.player.queue.is_empty();
                    ui.add_enabled_ui(have, |ui| {
                        if icons::button(ui, vec2(34.0, 34.0), icons::previous, "Previous")
                            .clicked()
                        {
                            let ctx = ui.ctx().clone();
                            self.on_prev(&ctx);
                        }
                        let playing = self
                            .player
                            .report
                            .as_ref()
                            .map(|r| r.playing)
                            .unwrap_or(false);
                        let pause_resp = if playing {
                            icons::button(ui, vec2(38.0, 38.0), icons::pause, "Pause")
                        } else {
                            icons::button(ui, vec2(38.0, 38.0), icons::play, "Play")
                        };
                        if pause_resp.clicked() {
                            let ctx = ui.ctx().clone();
                            self.on_play_pause(&ctx);
                        }
                        if icons::button(ui, vec2(34.0, 34.0), icons::next, "Next").clicked() {
                            let ctx = ui.ctx().clone();
                            self.on_next(&ctx);
                        }
                    });

                    // playback mode (off → loop-one → infinite)
                    let badge = if self.settings.loop_one {
                        Some("1")
                    } else if self.settings.infinite {
                        Some("∞")
                    } else {
                        None
                    };
                    let tip = if self.settings.loop_one {
                        "Playback mode: loop one (click for infinite queue)"
                    } else if self.settings.infinite {
                        "Playback mode: infinite queue (click for off)"
                    } else {
                        "Playback mode: off (click for loop)"
                    };
                    let (mrect, mresp) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::click());
                    if mresp.hovered() {
                        ui.painter()
                            .rect_filled(mrect, egui::CornerRadius::from(17.0), c::PANEL2);
                    }
                    let mode_color = if badge.is_some() { c::ACCENT2 } else { c::TEXT };
                    icons::loop_icon(ui.painter(), mrect, mode_color, badge);
                    let mresp = mresp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(tip);
                    if mresp.clicked() {
                        self.cycle_mode();
                    }

                    // position / seek / duration (report-driven; dragging
                    // freezes the readout until release). The position is
                    // the interpolated clock — frame-rate smooth instead of
                    // 250ms steps. A restored queue shows its saved spot
                    // until the engine holds a track — an empty
                    // post-restart report must not zero it out.
                    let (pos_ms, dur_ms) = match &self.player.report {
                        Some(r) if r.track_id.is_some() => (self.player.clock.est(), r.duration_ms),
                        _ => self.player.restored.unwrap_or((0, 0)),
                    };
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(widgets::fmt_time(pos_ms))
                            .size(12.0)
                            .color(c::DIM),
                    );
                    // 0.37 Slider width comes from `spacing::slider_width`, so
                    // size the seek bar to whatever is left after the trailing
                    // widgets (duration, queue button, volume, fullscreen).
                    let trailing =
                        46.0 + 30.0 + 110.0 + 30.0 + ui.spacing().item_spacing.x * 6.0 + 18.0;
                    let seek_w = (ui.available_width() - trailing).max(120.0);
                    if !self.player.seeking {
                        self.seek = if dur_ms > 0 {
                            (pos_ms as f32 / dur_ms as f32 * 1000.0).clamp(0.0, 1000.0)
                        } else {
                            0.0
                        };
                    }
                    ui.scope(|ui| {
                        ui.spacing_mut().slider_width = seek_w;
                        let resp = ui.add_enabled(
                            dur_ms > 0,
                            egui::Slider::new(&mut self.seek, 0.0..=1000.0).show_value(false),
                        );
                        if resp.drag_started() {
                            self.player.seeking = true;
                        }
                        if resp.drag_stopped() && self.player.seeking {
                            self.player.seeking = false;
                            let ms = (self.seek / 1000.0 * dur_ms as f32) as u64;
                            let ctx = ui.ctx().clone();
                            self.on_seek(&ctx, ms);
                        }
                    });
                    ui.label(
                        RichText::new(widgets::fmt_time(dur_ms))
                            .size(12.0)
                            .color(c::DIM),
                    );

                    // queue / volume / fullscreen
                    ui.add_space(6.0);
                    if icons::button(ui, vec2(30.0, 30.0), icons::queue_list, "Queue").clicked() {
                        self.go(View::Queue);
                    }
                    ui.scope(|ui| {
                        ui.spacing_mut().slider_width = 110.0;
                        ui.add(
                            egui::Slider::new(&mut self.settings.volume, 0.0..=100.0)
                                .show_value(false),
                        )
                        .on_hover_text("Volume");
                    });
                    let fs_tip = if self.fullscreen {
                        "Exit fullscreen"
                    } else {
                        "Fullscreen"
                    };
                    if icons::button(ui, vec2(30.0, 30.0), icons::fullscreen, fs_tip).clicked() {
                        self.fullscreen = !self.fullscreen;
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                    }
                });
            });
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }
}

impl eframe::App for SonoraApp {
    /// Runs before every frame *and* while the window is hidden (kept
    /// ticking by `tick_player`'s `request_repaint_after`): player status
    /// poll, auth probes, settings autosave. No UI may be shown here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick_player(ctx);
        self.tick_auth(ctx);
        // Lyrics auto-fetch + name resolutions (track-menu flows).
        self.tick_lyrics(ctx);
        self.tick_resolve();
        // Land finished artwork fetches (and evict past the byte cap)
        // before the views ask for their covers.
        self.covers.pump(ctx);
        crate::store::save_if_changed(&self.settings, &mut self.settings_saved);
    }

    /// Persist the queue mirror + position on shutdown (old persistQueue
    /// on hide/close): the last stretch of the session travels to the
    /// next launch.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_now();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Order matters: the player bar must span the full window width, so
        // it claims space before the sidebar and top bar.
        self.render_playerbar(ui);
        self.render_sidebar(ui);
        self.render_topbar(ui);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(c::BG)
                    .inner_margin(Margin::symmetric(22, 18)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("view")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.render_view(ui));
            });

        self.handle_keys(ui.ctx());
    }
}

// ---------------------------------------------------------------------------

/// Sidebar navigation item: transparent pill, accent2 + strong when active.
fn nav_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width().max(1.0), 32.0), Sense::click());
    if selected || resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::from(8.0), c::PANEL2);
    }
    let color = if selected { c::ACCENT2 } else { c::TEXT };
    let font = if selected {
        fonts::strong_font(14.0)
    } else {
        fonts::body_font(14.0)
    };
    ui.painter().text(
        rect.left_center() + vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        color,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Hairline separator used between sidebar sections (old `.side-section`).
fn section_line(ui: &egui::Ui) {
    let y = ui.cursor().top() - ui.spacing().item_spacing.y * 0.5;
    let r = ui.max_rect();
    ui.painter().line_segment(
        [Pos2::new(r.left(), y), Pos2::new(r.right(), y)],
        Stroke::new(1.0, c::LINE),
    );
}
