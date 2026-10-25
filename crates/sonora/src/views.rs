//! View rendering — one method per screen, driven by live backend data.
//!
//! Data slots (`SonoraApp::catalog`) auto-start on first render, poll once
//! per frame, and expose value/error for the loading and retry UIs. Detail
//! pages take the slot's value out while rendering and put it back after
//! (take/restore), so rows are built from real data while row actions
//! still take `&mut self`.

use apple_music_core::models::{
    Album, AlbumDetail, Artist, Playlist, PlaylistDetail, SearchResults, Track,
};
use egui::{vec2, Color32, FontId, RichText, Ui};

use crate::app::{Dialog, FsLayout, ImBg, ImCover, ImLayout, SonoraApp, View};
use crate::fonts;
use crate::icons;
use crate::lyrics::{LineClass, LyricEngine, LyricState, Side};
use crate::theme::c;
use crate::widgets::{self, Card, TrackRow};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Row subtitle: `Artist · Album` (the old list format).
fn sub_of(t: &Track) -> String {
    match (t.artist.trim(), t.album.trim()) {
        (a, "") if !a.is_empty() => a.to_owned(),
        (a, b) if !a.is_empty() && !b.is_empty() => format!("{a} · {b}"),
        ("", b) => b.to_owned(),
        (a, _) => a.to_owned(),
    }
}

/// `N songs` subtitle for playlist cards — empty when the API omits the
/// count (the old UI never rendered a zero).
fn count_sub(n: Option<u32>) -> String {
    n.filter(|&n| n > 0)
        .map(|n| format!("{n} songs"))
        .unwrap_or_default()
}

fn album_card(a: &Album) -> Card {
    Card::new(&a.title, &a.artist)
}

fn playlist_card(p: &Playlist) -> Card {
    Card::new(&p.name, &count_sub(p.track_count))
}

fn artist_card(a: &Artist) -> Card {
    let sub = if a.genres.is_empty() {
        String::new()
    } else {
        a.genres.join(" · ")
    };
    Card::new(&a.name, &sub)
}

/// Wrap cards into rows of the auto-fill grid; returns the clicked index.
fn card_grid(ui: &mut Ui, cards: &[Card]) -> Option<usize> {
    let (_, width) = widgets::cards_layout(ui, 150.0);
    let mut clicked = None;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(16.0, 16.0);
        ui.horizontal_wrapped(|ui| {
            for (i, card) in cards.iter().enumerate() {
                if widgets::card(ui, width, card).clicked() {
                    clicked = Some(i);
                }
            }
        });
    });
    clicked
}

/// Paint one synced lyric list (old buildLyricList + applyLyricClasses +
/// applyWordFill): per-segment word strips with per-word karaoke fill,
/// line wrapping, duet alignment, focus classes and click-to-seek.
/// Returns the seek targets of clicked rows (word timing when a word
/// was hit, else the line's).
///
/// Styling mirrors the old CSS: 24px semibold body at dim/52%, active
/// 1.22em white (karaoke words start at white/62% and fill to white),
/// backing lines 0.92em at dim/72% (active backing #c9c9d1), near lines
/// 0.92em at dim/62%, far lines collapsed, bracket runs 0.8em, duet rows
/// inset 22% on the far side.
pub(crate) fn lyric_lines(
    ui: &mut Ui,
    engine: &LyricEngine,
    focused: bool,
    pos: u64,
    col_w: f32,
    scroll_to: Option<usize>,
) -> Vec<u64> {
    let center = engine.focus_center(pos);
    let duet = engine.duet();
    let has_karaoke = engine.has_karaoke();
    let mut seeks: Vec<u64> = Vec::new();
    let painter = ui.painter().clone();

    for i in 0..engine.line_count() {
        let class = engine.line_class(i, focused, center);
        if class == LineClass::Far {
            // Far lines are collapsed (max-height 0) in the old list.
            continue;
        }
        let bg = engine.is_bg(i);
        let active = class == LineClass::Active;
        let line = &engine.lines()[i];
        let side = if duet { engine.agent_side(i) } else { None };

        let base = 24.0;
        let (size, color) = if active {
            if bg {
                (
                    base * 0.92,
                    Color32::from_rgb(0xc9, 0xc9, 0xd1).gamma_multiply(0.9),
                )
            } else if line.words.is_empty() {
                (base * 1.22, c::WHITE)
            } else {
                // Karaoke base: unfilled rest color; fill paints white
                // over it (old linear-gradient sweep).
                (base * 1.22, Color32::from_white_alpha(158))
            }
        } else if class == LineClass::Near {
            (base * 0.92, c::DIM.gamma_multiply(0.62))
        } else if bg {
            (base * 0.92, c::DIM.gamma_multiply(0.72))
        } else {
            (base, c::DIM.gamma_multiply(0.52))
        };

        let segs = engine.line_segments(i);
        let agent_pad = if side.is_some() { col_w * 0.22 } else { 0.0 };
        let x0 = 12.0
            + if side == Some(Side::Right) {
                agent_pad
            } else {
                0.0
            };
        let cw = (col_w - 24.0 - agent_pad).max(40.0);

        // Measure the segments (galley metrics don't depend on the paint
        // color — painting passes it as an override).
        let measured: Vec<std::sync::Arc<egui::Galley>> = segs
            .iter()
            .map(|s| {
                let font = if s.bracket {
                    FontId::proportional(size * 0.8)
                } else {
                    fonts::strong_font(size)
                };
                painter.layout_job(egui::text::LayoutJob::single_section(
                    s.text.clone(),
                    egui::TextFormat {
                        font_id: font,
                        color,
                        ..Default::default()
                    },
                ))
            })
            .collect();

        // Pack segments into wrapped rows (old lines wrap at word
        // boundaries; a boundary space in the text suppresses the gap).
        let mut rows: Vec<Vec<(usize, f32)>> = Vec::new();
        let mut row: Vec<(usize, f32)> = Vec::new();
        let mut x = 0.0;
        for (gi, g) in measured.iter().enumerate() {
            let w = g.size().x;
            let gap = if row.is_empty()
                || segs[gi - 1].text.ends_with(char::is_whitespace)
                || segs[gi].text.starts_with(char::is_whitespace)
            {
                // Row start needs no gap; a boundary space in the text
                // already separates the words.
                0.0
            } else {
                size * 0.3
            };
            if !row.is_empty() && x + gap + w > cw {
                rows.push(std::mem::take(&mut row));
                row.push((gi, 0.0));
                x = w;
            } else {
                if !row.is_empty() {
                    x += gap;
                }
                row.push((gi, x));
                x += w;
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }

        let row_h = |row: &[(usize, f32)]| -> f32 {
            row.iter()
                .map(|&(gi, _)| measured[gi].size().y)
                .fold(0.0, f32::max)
                * 1.45
        };
        let total_h: f32 = rows.iter().map(|r| row_h(r)).sum::<f32>() + 12.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(col_w, total_h), egui::Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let paint_color = if !active && resp.hovered() {
            // Old .lyr-line:hover: brighten toward the text color.
            c::TEXT.gamma_multiply(0.85)
        } else {
            color
        };

        let mut y = rect.min.y + 6.0;
        let mut hits: Vec<(egui::Rect, Option<u64>)> = Vec::new();
        for row in &rows {
            let h = row_h(row);
            let row_w = row
                .iter()
                .map(|&(gi, gx)| gx + measured[gi].size().x)
                .fold(0.0, f32::max);
            let row_left = rect.min.x
                + x0
                + if side == Some(Side::Right) {
                    (cw - row_w).max(0.0)
                } else {
                    0.0
                };
            for &(gi, gx) in row {
                let g = &measured[gi];
                let gh = g.size().y;
                let pt = egui::pos2(row_left + gx, y + (h - gh) * 0.5);
                painter.galley(pt, g.clone(), paint_color);
                // Karaoke fill sweep (old applyWordFill → --fill).
                if active && !bg && has_karaoke {
                    let fill = engine.word_fill(i, gi, pos) / 100.0;
                    if fill > 0.0 {
                        let clip = egui::Rect::from_min_max(pt, pt + vec2(g.size().x * fill, gh));
                        painter
                            .with_clip_rect(clip)
                            .galley(pt, g.clone(), Color32::WHITE);
                    }
                }
                hits.push((
                    egui::Rect::from_min_size(pt, g.size()),
                    line.words.get(gi).map(|w| w.ms),
                ));
            }
            y += h;
        }

        // Click → the word's timing under the pointer, else the line's
        // (old lyrics click handler).
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let mut ms = line.ms;
                for (r, word_ms) in &hits {
                    if r.contains(p) {
                        if let Some(w) = word_ms {
                            ms = *w;
                        }
                        break;
                    }
                }
                seeks.push(ms);
            }
        }
        // Center the primary line when it changes (old scrollIntoView).
        if scroll_to == Some(i) {
            resp.scroll_to_me(Some(egui::Align::Center));
        }
    }
    seeks
}

/// Loading / error + retry line for a data slot; `true` → render the data.
fn slot_ready(
    ui: &mut Ui,
    label: &str,
    loading: bool,
    error: Option<&str>,
    on_retry: impl FnOnce(),
) -> bool {
    if loading {
        ui.label(
            RichText::new(format!("Loading {label}…"))
                .size(13.0)
                .color(c::DIM),
        );
        return false;
    }
    if let Some(err) = error {
        ui.label(
            RichText::new(format!("Couldn't load {label}: {err}"))
                .size(13.0)
                .color(c::ACCENT2),
        );
        if ui.button("Retry").clicked() {
            on_retry();
        }
        return false;
    }
    true
}

/// Back button for detail pages; `true` → go back.
fn back_button(ui: &mut Ui) -> bool {
    ui.button("← Back")
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// A `T`-valued option row with a label (old `<label>… <select>` lines).
fn combo<T: Copy + PartialEq>(
    ui: &mut Ui,
    id: &'static str,
    current: &mut T,
    label: &str,
    options: &[(T, &'static str)],
) {
    ui.horizontal(|ui| {
        ui.label(label);
        let selected = options
            .iter()
            .find(|(v, _)| v == current)
            .map(|(_, l)| *l)
            .unwrap_or("…");
        egui::ComboBox::from_id_salt(id)
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for (v, l) in options {
                    ui.selectable_value(current, *v, *l);
                }
            });
    });
}

/// Row actions collected while rendering, executed after the row loop so
/// the list borrow of `self` is released.
enum RowAct {
    Play(usize),
    PlayNext(usize),
    Append(usize),
    Fav(usize),
    AddTo(String, usize),
    Remove(String, usize),
    /// Track-menu "+ New playlist…": create one, then append this track.
    NewPlaylistWith(usize),
}

/// Playlist-grid context-menu actions (old `openPlaylistMenu`).
enum GridAct {
    Open(usize),
    Play(usize),
    Rename(usize),
    Share(usize),
    Unshare(usize),
    Delete(usize),
}

/// Queue-row actions (old `openTrackMenu` with `inQueue: true`).
enum QueueAct {
    /// Click / "Play": jump the queue to this row (old jumpToQueueIndex).
    Jump(usize),
    PlayNext(usize),
    Fav(usize),
    Remove(usize),
    /// Add this queued song to an existing playlist.
    AddTo(String, usize),
    /// Queue-row "+ New playlist…": create one, then append this song.
    NewPlaylistWith(usize),
}

/// Shared track-menu navigation actions (old openTrackMenu tail: Lyrics /
/// "Artist → name" / "Album → name"), collected while rendering the menu
/// and run after the row loop releases `self`.
enum NavAct {
    Lyrics {
        id: String,
        title: String,
        artist: String,
    },
    Artist {
        name: String,
    },
    Album {
        artist: String,
        album: String,
    },
}

/// Tail of both track-row context menus (old openTrackMenu: "Lyrics",
/// "Artist → …" when the row has an artist, "Album → …" when it has an
/// album). Owned data only, so it works for `Track` rows and queue
/// mirror entries alike.
fn track_nav_menu(
    ui: &mut Ui,
    id: &str,
    title: &str,
    artist: &str,
    album: &str,
    out: &mut Vec<NavAct>,
) {
    if ui.button("Lyrics").clicked() {
        out.push(NavAct::Lyrics {
            id: id.to_owned(),
            title: title.to_owned(),
            artist: artist.to_owned(),
        });
        ui.close();
    }
    if !artist.is_empty() && ui.button(format!("Artist → {artist}")).clicked() {
        out.push(NavAct::Artist {
            name: artist.to_owned(),
        });
        ui.close();
    }
    if !album.is_empty() && ui.button(format!("Album → {album}")).clicked() {
        out.push(NavAct::Album {
            artist: artist.to_owned(),
            album: album.to_owned(),
        });
        ui.close();
    }
}

// ---------------------------------------------------------------------------

impl SonoraApp {
    /// Land shared track-menu navigation (post-loop so the row borrow of
    /// `self` is released): open the lyrics view for one track, or
    /// resolve an artist/album by name and open its detail page.
    fn run_nav(&mut self, ctx: &egui::Context, nav: Vec<NavAct>) {
        for act in nav {
            match act {
                NavAct::Lyrics { id, title, artist } => {
                    self.open_lyrics_for(ctx, &id, &title, &artist);
                }
                NavAct::Artist { name } => self.open_artist_by_name(ctx, &name),
                NavAct::Album { artist, album } => {
                    self.open_album_by_name(ctx, &artist, &album);
                }
            }
        }
    }

    // -- shared: cards ---------------------------------------------------------

    /// Album cards with cover textures requested at `size` (letter tiles
    /// until the fetch lands).
    fn album_cards(&mut self, ui: &Ui, albums: &[Album], size: u32) -> Vec<Card> {
        albums
            .iter()
            .map(|a| {
                let mut card = album_card(a);
                card.cover = a
                    .artwork
                    .as_ref()
                    .and_then(|art| self.covers.get(ui.ctx(), &self.backend, &art.url, size));
                card
            })
            .collect()
    }

    /// Playlist cards with cover textures at `size` (artists carry no
    /// artwork in the model and keep their letter tiles).
    fn playlist_cards(&mut self, ui: &Ui, playlists: &[Playlist], size: u32) -> Vec<Card> {
        playlists
            .iter()
            .map(|p| {
                let mut card = playlist_card(p);
                card.cover = p
                    .artwork
                    .as_ref()
                    .and_then(|art| self.covers.get(ui.ctx(), &self.backend, &art.url, size));
                card
            })
            .collect()
    }

    pub fn render_view(&mut self, ui: &mut Ui) {
        match self.view.clone() {
            View::Browse => self.render_browse(ui),
            View::Search => self.render_search(ui),
            View::Playlists => self.render_playlists(ui),
            View::Queue => self.render_queue(ui),
            View::Lyrics => self.render_lyrics(ui),
            View::Settings => self.render_settings(ui),
            View::Album(_) => self.render_album(ui),
            View::Artist(_) => self.render_artist(ui),
            View::Playlist(_) => self.render_playlist(ui),
        }
        let ctx = ui.ctx().clone();
        self.render_dialog(&ctx);
    }

    // -- shared: track list --------------------------------------------------

    /// Track rows with click-to-play-from-index and the old context menu
    /// (play / play next / add to queue / add to playlist / remove).
    /// `tracks` must be a local snapshot, not a borrow of `self`.
    fn track_list(&mut self, ui: &mut Ui, tracks: &[Track], playlist: Option<&str>) {
        if tracks.is_empty() {
            widgets::hint(ui, "No tracks.");
            return;
        }
        // Mirror entries drop dead/unavailable tracks before they can
        // ever be played (old playTrack), and record art for the bar.
        let entries = self.track_entries(tracks);
        let current_id = self.player.report.as_ref().and_then(|r| r.track_id.clone());
        let mut acts: Vec<RowAct> = Vec::new();
        let mut nav: Vec<NavAct> = Vec::new();

        for (i, t) in tracks.iter().enumerate() {
            let cover = t
                .artwork
                .as_ref()
                .and_then(|a| self.covers.get(ui.ctx(), &self.backend, &a.url, 60));
            let row = TrackRow {
                num: Some(i as u32 + 1),
                letter: t.title.trim().chars().next(),
                cover,
                title: t.title.clone(),
                sub: sub_of(t),
                duration: t.duration_ms.map(widgets::fmt_time),
                unavailable: t.unavailable || self.player.dead.contains(&t.id),
                current: current_id.as_deref() == Some(t.id.as_str()),
                ..Default::default()
            };
            let resp = widgets::track_row(ui, &row);
            if resp.clicked() {
                acts.push(RowAct::Play(i));
            }
            resp.context_menu(|ui| {
                if ui.button("Play").clicked() {
                    acts.push(RowAct::Play(i));
                    ui.close();
                }
                if ui.button("Play next").clicked() {
                    acts.push(RowAct::PlayNext(i));
                    ui.close();
                }
                if ui.button("Add to queue").clicked() {
                    acts.push(RowAct::Append(i));
                    ui.close();
                }
                if ui.button("♥ Add to favorites").clicked() {
                    acts.push(RowAct::Fav(i));
                    ui.close();
                }
                ui.separator();
                track_nav_menu(ui, &t.id, &t.title, &t.artist, &t.album, &mut nav);
                ui.menu_button("Add to playlist", |ui| {
                    if self.catalog.playlists.value.is_none()
                        && !self.catalog.playlists.busy()
                        && self.catalog.playlists.error.is_none()
                    {
                        let state = self.backend.state.clone();
                        let ctx = ui.ctx().clone();
                        let rx = self.backend.call(&ctx, async move {
                            shell::commands::library_playlists(&state).await
                        });
                        self.catalog.playlists.start(rx);
                    }
                    if let Some(list) = self.catalog.playlists.value.as_ref() {
                        if list.is_empty() {
                            ui.label("No playlists yet.");
                        }
                        for p in list {
                            if ui.button(&p.name).clicked() {
                                acts.push(RowAct::AddTo(p.id.clone(), i));
                                ui.close();
                            }
                        }
                    } else {
                        ui.label("Loading…");
                    }
                });
                if let Some(pid) = playlist {
                    ui.separator();
                    if ui.button("Remove from playlist").clicked() {
                        acts.push(RowAct::Remove(pid.to_owned(), i));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("+ New playlist…").clicked() {
                    acts.push(RowAct::NewPlaylistWith(i));
                    ui.close();
                }
            });
        }

        for act in acts {
            match act {
                RowAct::Play(i) => {
                    let t = &tracks[i];
                    if t.unavailable || self.player.dead.contains(&t.id) {
                        self.set_status(format!(
                            "“{}” is unavailable and can't be played",
                            t.title
                        ));
                    } else {
                        // Start index lands on this track after the
                        // dead/unavailable filter (old playTrack's
                        // findIndex on the filtered list).
                        let start = entries.iter().position(|e| e.item.id == t.id).unwrap_or(0);
                        self.play_queue(ui.ctx(), entries.clone(), start);
                    }
                }
                RowAct::PlayNext(i) => {
                    let t = &tracks[i];
                    if t.unavailable || self.player.dead.contains(&t.id) {
                        self.set_status(format!(
                            "“{}” is unavailable and can't be played",
                            t.title
                        ));
                    } else {
                        let ctx = ui.ctx().clone();
                        if let Some(e) = self.track_entries(&tracks[i..=i]).into_iter().next() {
                            self.queue_play_next(&ctx, vec![e]);
                            self.set_status("Playing next.");
                        }
                    }
                }
                RowAct::Append(i) => {
                    let t = &tracks[i];
                    if t.unavailable || self.player.dead.contains(&t.id) {
                        self.set_status(format!(
                            "“{}” is unavailable and can't be played",
                            t.title
                        ));
                    } else {
                        let ctx = ui.ctx().clone();
                        if let Some(e) = self.track_entries(&tracks[i..=i]).into_iter().next() {
                            self.queue_append(&ctx, vec![e]);
                            self.set_status("Added to queue.");
                        }
                    }
                }
                RowAct::Fav(i) => {
                    let state = self.backend.state.clone();
                    let ctx = ui.ctx().clone();
                    let song = tracks[i].id.clone();
                    self.backend.fire_msg(&ctx, "favorites", async move {
                        shell::commands::add_to_favorites(&state, vec![song]).await
                    });
                }
                RowAct::NewPlaylistWith(i) => {
                    self.catalog.dialog = Some(Dialog::NewPlaylist {
                        name: String::new(),
                        add_song: Some(tracks[i].id.clone()),
                    });
                }
                RowAct::AddTo(playlist_id, i) => {
                    let state = self.backend.state.clone();
                    let ctx = ui.ctx().clone();
                    let song = tracks[i].id.clone();
                    self.backend.fire_msg(&ctx, "add to playlist", async move {
                        shell::commands::add_to_playlist(&state, playlist_id, vec![song]).await
                    });
                }
                RowAct::Remove(playlist_id, i) => {
                    let state = self.backend.state.clone();
                    let ctx = ui.ctx().clone();
                    let song = tracks[i].id.clone();
                    self.backend
                        .fire_msg(&ctx, "remove from playlist", async move {
                            shell::commands::remove_from_playlist(&state, playlist_id, vec![song])
                                .await
                        });
                    self.catalog.playlist.clear();
                }
            }
        }
        // Shared tail actions (Lyrics / Artist→ / Album→).
        let ctx = ui.ctx().clone();
        self.run_nav(&ctx, nav);
    }

    // -- browse ---------------------------------------------------------------

    fn render_browse(&mut self, ui: &mut Ui) {
        self.catalog.charts.poll();
        if self.catalog.charts.value.is_none()
            && !self.catalog.charts.busy()
            && self.catalog.charts.error.is_none()
        {
            let state = self.backend.state.clone();
            let ctx = ui.ctx().clone();
            let rx =
                self.backend.call(
                    &ctx,
                    async move { shell::commands::browse_charts(&state).await },
                );
            self.catalog.charts.start(rx);
        }
        let err = self.catalog.charts.error.clone();
        if !slot_ready(
            ui,
            "charts",
            self.catalog.charts.loading,
            err.as_deref(),
            || self.catalog.charts.clear(),
        ) {
            return;
        }
        let value = self.catalog.charts.value.take();
        if let Some(data) = &value {
            self.render_charts(ui, data);
        }
        self.catalog.charts.value = value;
    }

    fn render_charts(&mut self, ui: &mut Ui, data: &SearchResults) {
        if !data.tracks.is_empty() {
            widgets::h2(ui, "Top Songs");
            ui.add_space(4.0);
            self.track_list(ui, &data.tracks, None);
        }
        if !data.albums.is_empty() {
            ui.add_space(16.0);
            widgets::h2(ui, "Top Albums");
            ui.add_space(6.0);
            let cards = self.album_cards(ui, &data.albums, 300);
            if let Some(i) = card_grid(ui, &cards) {
                let id = data.albums[i].id.clone();
                self.open_album(&id);
            }
        }
        if !data.playlists.is_empty() {
            ui.add_space(16.0);
            widgets::h2(ui, "Top Playlists");
            ui.add_space(6.0);
            let cards = self.playlist_cards(ui, &data.playlists, 300);
            if let Some(i) = card_grid(ui, &cards) {
                let id = data.playlists[i].id.clone();
                self.open_playlist(&id);
            }
        }
    }

    // -- search ----------------------------------------------------------------

    fn render_search(&mut self, ui: &mut Ui) {
        widgets::h2(ui, &format!("Results for '{}'", self.search_term));
        let term = self.search_term.clone();
        self.catalog.search.poll();
        if !term.is_empty()
            && self.catalog.search.value.is_none()
            && !self.catalog.search.busy()
            && self.catalog.search.error.is_none()
        {
            let state = self.backend.state.clone();
            let ctx = ui.ctx().clone();
            let rx = self.backend.call(&ctx, async move {
                shell::commands::search_catalog(&state, term).await
            });
            self.catalog.search.start(rx);
        }
        let err = self.catalog.search.error.clone();
        if !slot_ready(
            ui,
            "results",
            self.catalog.search.loading,
            err.as_deref(),
            || self.catalog.search.clear(),
        ) {
            return;
        }
        let value = self.catalog.search.value.take();
        if let Some(data) = &value {
            self.render_search_results(ui, data);
        }
        self.catalog.search.value = value;
    }

    fn render_search_results(&mut self, ui: &mut Ui, data: &SearchResults) {
        if data.tracks.is_empty()
            && data.albums.is_empty()
            && data.playlists.is_empty()
            && data.artists.is_empty()
        {
            widgets::hint(ui, "No results.");
            return;
        }

        if !data.tracks.is_empty() {
            widgets::h3(ui, "Songs");
            ui.add_space(4.0);
            self.track_list(ui, &data.tracks, None);
        }

        if !data.albums.is_empty() {
            ui.add_space(14.0);
            widgets::h3(ui, "Albums");
            ui.add_space(6.0);
            let cards = self.album_cards(ui, &data.albums, 300);
            if let Some(i) = card_grid(ui, &cards) {
                let id = data.albums[i].id.clone();
                self.open_album(&id);
            }
        }

        if !data.playlists.is_empty() {
            ui.add_space(14.0);
            widgets::h3(ui, "Playlists");
            ui.add_space(6.0);
            let cards = self.playlist_cards(ui, &data.playlists, 300);
            if let Some(i) = card_grid(ui, &cards) {
                let id = data.playlists[i].id.clone();
                self.open_playlist(&id);
            }
        }

        if !data.artists.is_empty() {
            ui.add_space(14.0);
            widgets::h3(ui, "Artists");
            ui.add_space(6.0);
            let cards: Vec<Card> = data.artists.iter().map(artist_card).collect();
            if let Some(i) = card_grid(ui, &cards) {
                let id = data.artists[i].id.clone();
                self.open_artist(&id);
            }
        }
    }

    // -- playlists -------------------------------------------------------------

    /// Old `sharePlaylist`: library playlists publish on first share
    /// (backend returns the link); catalog items open their addressable URL.
    fn share_playlist_now(&mut self, ctx: &egui::Context, playlist: &Playlist) {
        if playlist.id.starts_with("p.") {
            let state = self.backend.state.clone();
            let pid = playlist.id.clone();
            self.backend.fire_msg(ctx, "share playlist", async move {
                shell::commands::share_playlist(&state, pid).await
            });
        } else if let Some(url) = playlist.share_url.clone() {
            let _ = shell::open_url(url);
            self.set_status("Playlist link opened.");
        } else {
            self.set_status("No shareable link for this playlist yet.");
        }
    }

    /// Old `unsharePlaylistFlow` — privatize (no-op server-side when
    /// already private), then refresh the open detail.
    fn unshare_playlist_now(&mut self, ctx: &egui::Context, playlist: &Playlist) {
        let state = self.backend.state.clone();
        let pid = playlist.id.clone();
        self.backend.fire_msg(ctx, "unshare playlist", async move {
            shell::commands::unshare_playlist(&state, pid).await
        });
        self.catalog.playlist.clear();
    }

    fn render_playlists(&mut self, ui: &mut Ui) {
        // Finish a grid-menu "Play" (fetch tracks → replace the queue).
        self.catalog.card_play.poll();
        if let Some(err) = self.catalog.card_play.error.take() {
            self.set_status(&err);
        }
        if let Some(detail) = self.catalog.card_play.value.take() {
            let entries = self.track_entries(&detail.tracks);
            let ctx = ui.ctx().clone();
            self.play_queue(&ctx, entries, 0);
        }

        ui.horizontal(|ui| {
            widgets::h2(ui, "Your Playlists");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("New playlist")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.catalog.dialog = Some(Dialog::NewPlaylist {
                        name: String::new(),
                        add_song: None,
                    });
                }
            });
        });
        ui.add_space(6.0);

        self.catalog.playlists.poll();
        if self.catalog.playlists.value.is_none()
            && !self.catalog.playlists.busy()
            && self.catalog.playlists.error.is_none()
        {
            let state = self.backend.state.clone();
            let ctx = ui.ctx().clone();
            let rx = self.backend.call(&ctx, async move {
                shell::commands::library_playlists(&state).await
            });
            self.catalog.playlists.start(rx);
        }
        let err = self.catalog.playlists.error.clone();
        if !slot_ready(
            ui,
            "playlists",
            self.catalog.playlists.loading,
            err.as_deref(),
            || self.catalog.playlists.clear(),
        ) {
            return;
        }
        let value = self.catalog.playlists.value.take();
        if let Some(list) = &value {
            if list.is_empty() {
                widgets::hint(ui, "No playlists yet — create one.");
            } else {
                // Cards with the old right-click menu (open / rename /
                // share / make private / play / delete).
                let cards = self.playlist_cards(ui, list, 300);
                let mut clicked: Option<usize> = None;
                let mut acts: Vec<GridAct> = Vec::new();
                let (_, width) = widgets::cards_layout(ui, 150.0);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing = vec2(16.0, 16.0);
                    ui.horizontal_wrapped(|ui| {
                        for (i, card) in cards.iter().enumerate() {
                            let resp = widgets::card(ui, width, card);
                            if resp.clicked() {
                                clicked = Some(i);
                            }
                            let p = &list[i];
                            let editable = p.id.starts_with("p.") && p.can_edit != Some(false);
                            resp.context_menu(|ui| {
                                if ui.button("Open").clicked() {
                                    acts.push(GridAct::Open(i));
                                    ui.close();
                                }
                                if editable && ui.button("Rename…").clicked() {
                                    acts.push(GridAct::Rename(i));
                                    ui.close();
                                }
                                if ui.button("Share…").clicked() {
                                    acts.push(GridAct::Share(i));
                                    ui.close();
                                }
                                // Keep the make-private entry visible while
                                // visibility is unknown (library list omits
                                // it) — the backend no-ops when already private.
                                let private = if p.is_public == Some(true) {
                                    "Make private ✓ public"
                                } else {
                                    "Make private…"
                                };
                                if p.id.starts_with("p.")
                                    && p.is_public != Some(false)
                                    && ui.button(private).clicked()
                                {
                                    acts.push(GridAct::Unshare(i));
                                    ui.close();
                                }
                                if ui.button("Play").clicked() {
                                    acts.push(GridAct::Play(i));
                                    ui.close();
                                }
                                if editable && ui.button("Delete…").clicked() {
                                    acts.push(GridAct::Delete(i));
                                    ui.close();
                                }
                            });
                        }
                    });
                });

                if let Some(i) = clicked {
                    let id = list[i].id.clone();
                    self.open_playlist(&id);
                }
                let ctx = ui.ctx().clone();
                for act in acts {
                    match act {
                        GridAct::Open(i) => {
                            let id = list[i].id.clone();
                            self.open_playlist(&id);
                        }
                        GridAct::Play(i) => {
                            let state = self.backend.state.clone();
                            let pid = list[i].id.clone();
                            let rx = self.backend.call(&ctx, async move {
                                shell::commands::get_playlist(&state, pid).await
                            });
                            self.catalog.card_play.start(rx);
                        }
                        GridAct::Rename(i) => {
                            self.catalog.dialog = Some(Dialog::RenamePlaylist {
                                id: list[i].id.clone(),
                                name: list[i].name.clone(),
                            });
                        }
                        GridAct::Share(i) => {
                            let p = list[i].clone();
                            self.share_playlist_now(&ctx, &p);
                        }
                        GridAct::Unshare(i) => {
                            let p = list[i].clone();
                            self.unshare_playlist_now(&ctx, &p);
                        }
                        GridAct::Delete(i) => {
                            self.catalog.dialog = Some(Dialog::DeletePlaylist {
                                id: list[i].id.clone(),
                                name: list[i].name.clone(),
                            });
                        }
                    }
                }
            }
        }
        self.catalog.playlists.value = value;
    }

    // -- queue -------------------------------------------------------------------

    fn render_queue(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            widgets::h2(ui, "Queue");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("Clear queue")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    let ctx = ui.ctx().clone();
                    self.queue_clear(&ctx);
                    self.set_status("Queue cleared.");
                }
                if ui
                    .checkbox(&mut self.settings.infinite, "∞ Infinite")
                    .on_hover_text("Keep playing similar songs when the queue runs out")
                    .changed()
                    && !self.settings.infinite
                {
                    // Toggling off cancels the pending fill retry (old
                    // cancelRadioRetry on the toggle).
                    self.player.cancel_radio_retry();
                }
            });
        });
        ui.add_space(8.0);

        let entries = self.player.queue.clone();
        if entries.is_empty() {
            widgets::hint(ui, "Nothing queued.");
            return;
        }
        let current = self.player.index;
        let mut acts: Vec<QueueAct> = Vec::new();
        let mut nav: Vec<NavAct> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            let title = e
                .item
                .title
                .clone()
                .unwrap_or_else(|| format!("Track {}", e.item.id));
            let cover = e
                .art
                .as_ref()
                .and_then(|a| self.covers.get(ui.ctx(), &self.backend, a, 60));
            let row = TrackRow {
                num: Some(i as u32 + 1),
                letter: title.trim().chars().next(),
                cover,
                title,
                sub: e.item.artist.clone().unwrap_or_default(),
                duration: e.duration_ms.map(widgets::fmt_time),
                unavailable: self.player.dead.contains(&e.item.id),
                current: current == Some(i),
                radio: e.radio,
            };
            let resp = widgets::track_row(ui, &row);
            if resp.clicked() {
                acts.push(QueueAct::Jump(i));
            }
            resp.context_menu(|ui| {
                if ui.button("Play").clicked() {
                    acts.push(QueueAct::Jump(i));
                    ui.close();
                }
                if ui.button("Play next").clicked() {
                    acts.push(QueueAct::PlayNext(i));
                    ui.close();
                }
                if ui.button("♥ Add to favorites").clicked() {
                    acts.push(QueueAct::Fav(i));
                    ui.close();
                }
                if ui.button("Remove from queue").clicked() {
                    acts.push(QueueAct::Remove(i));
                    ui.close();
                }
                ui.separator();
                track_nav_menu(
                    ui,
                    &e.item.id,
                    e.item.title.as_deref().unwrap_or_default(),
                    e.item.artist.as_deref().unwrap_or_default(),
                    e.album.as_deref().unwrap_or_default(),
                    &mut nav,
                );
                ui.menu_button("Add to playlist", |ui| {
                    if self.catalog.playlists.value.is_none()
                        && !self.catalog.playlists.busy()
                        && self.catalog.playlists.error.is_none()
                    {
                        let state = self.backend.state.clone();
                        let ctx = ui.ctx().clone();
                        let rx = self.backend.call(&ctx, async move {
                            shell::commands::library_playlists(&state).await
                        });
                        self.catalog.playlists.start(rx);
                    }
                    if let Some(list) = self.catalog.playlists.value.as_ref() {
                        if list.is_empty() {
                            ui.label("No playlists yet.");
                        }
                        for p in list {
                            if ui.button(&p.name).clicked() {
                                acts.push(QueueAct::AddTo(p.id.clone(), i));
                                ui.close();
                            }
                        }
                    } else {
                        ui.label("Loading…");
                    }
                });
                if ui.button("+ New playlist…").clicked() {
                    acts.push(QueueAct::NewPlaylistWith(i));
                    ui.close();
                }
            });
        }

        for act in acts {
            match act {
                QueueAct::Jump(i) => {
                    let ctx = ui.ctx().clone();
                    self.jump_to(&ctx, i);
                }
                QueueAct::PlayNext(i) => {
                    let ctx = ui.ctx().clone();
                    let mut e = entries[i].clone();
                    e.radio = false;
                    self.queue_play_next(&ctx, vec![e]);
                    self.set_status("Playing next.");
                }
                QueueAct::Fav(i) => {
                    let state = self.backend.state.clone();
                    let ctx = ui.ctx().clone();
                    let song = entries[i].item.id.clone();
                    self.backend.fire_msg(&ctx, "favorites", async move {
                        shell::commands::add_to_favorites(&state, vec![song]).await
                    });
                }
                QueueAct::Remove(i) => {
                    self.queue_remove(i);
                    self.set_status("Removed from queue.");
                }
                QueueAct::AddTo(playlist_id, i) => {
                    let state = self.backend.state.clone();
                    let ctx = ui.ctx().clone();
                    let song = entries[i].item.id.clone();
                    self.backend.fire_msg(&ctx, "add to playlist", async move {
                        shell::commands::add_to_playlist(&state, playlist_id, vec![song]).await
                    });
                }
                QueueAct::NewPlaylistWith(i) => {
                    self.catalog.dialog = Some(Dialog::NewPlaylist {
                        name: String::new(),
                        add_song: Some(entries[i].item.id.clone()),
                    });
                }
            }
        }
        // Shared tail actions (Lyrics / Artist→ / Album→).
        let ctx = ui.ctx().clone();
        self.run_nav(&ctx, nav);
    }

    // -- lyrics ----------------------------------------------------------------

    /// The lyrics view: header (target title — artist + caption), then the
    /// synced karaoke list (old view-lyrics: buildLyricList +
    /// applyLyricClasses + applyWordFill + click-to-seek) or the plain /
    /// loading / error states. The engine advances against the smooth
    /// position clock every frame, so the fill and classes stay live.
    fn render_lyrics(&mut self, ui: &mut Ui) {
        let playing_id = self.player.report.as_ref().and_then(|r| r.track_id.clone());
        let pos = self.player.clock.est();
        let before = self.lyrics.engine.primary();
        self.lyrics.position(playing_id.as_deref(), pos);
        let primary = self.lyrics.engine.primary();
        let scroll_to = if primary.is_some() && primary != before {
            primary
        } else {
            None
        };
        let live = self.lyrics.live(playing_id.as_deref());
        let focused = self.settings.lyrics_focus;
        let state = self.lyrics.state();

        let col_w = ui.available_width().min(760.0);
        let mut seeks: Vec<u64> = Vec::new();
        ui.vertical_centered(|ui| {
            widgets::h2(ui, "Lyrics");
            let engine = &self.lyrics.engine;
            if engine.title().is_empty() {
                ui.add_space(4.0);
                widgets::hint(ui, "Pick a track → Lyrics.");
                return;
            }
            // Title line (old lyricTitleFor: "title — artist").
            let title = if engine.artist().is_empty() {
                engine.title().to_owned()
            } else {
                format!("{} — {}", engine.title(), engine.artist())
            };
            ui.label(
                RichText::new(title)
                    .font(fonts::strong_font(17.0))
                    .color(c::TEXT),
            );
            // Caption (old lyricCaption) — synced lines only.
            if matches!(state, LyricState::Lines) {
                let caption = engine.caption(live, focused);
                if !caption.is_empty() {
                    ui.add_space(4.0);
                    ui.label(RichText::new(caption).size(12.0).color(c::DIM));
                }
            }
            ui.add_space(8.0);
            match state {
                LyricState::Loading => {
                    widgets::hint(ui, "Loading…");
                }
                LyricState::Error(e) => {
                    ui.label(
                        RichText::new(format!("No lyrics available ({e})"))
                            .size(15.0)
                            .color(c::DIM),
                    );
                }
                LyricState::Text => {
                    for para in engine.text().split('\n') {
                        ui.label(
                            RichText::new(para)
                                .font(fonts::strong_font(24.0))
                                .color(c::WHITE),
                        );
                    }
                }
                LyricState::Lines => {
                    seeks = lyric_lines(ui, engine, focused, pos, col_w, scroll_to);
                }
            }
            ui.add_space(48.0);
        });
        // Click a line/word → jump to its timing (old lyrics click handler
        // called markSeek + player_seek per container).
        for ms in seeks {
            let ctx = ui.ctx().clone();
            self.on_seek(&ctx, ms);
        }
    }

    // -- album detail -------------------------------------------------------------

    fn render_album(&mut self, ui: &mut Ui) {
        if back_button(ui) {
            self.back();
            return;
        }
        ui.add_space(6.0);
        self.catalog.albums.poll();
        let id = self.catalog.album_id.clone();
        if id.is_empty() {
            widgets::hint(ui, "No album selected.");
            return;
        }
        if self.catalog.albums.value.is_none()
            && !self.catalog.albums.busy()
            && self.catalog.albums.error.is_none()
        {
            let state = self.backend.state.clone();
            let ctx = ui.ctx().clone();
            let rx =
                self.backend.call(
                    &ctx,
                    async move { shell::commands::get_album(&state, id).await },
                );
            self.catalog.albums.start(rx);
        }
        let err = self.catalog.albums.error.clone();
        if !slot_ready(
            ui,
            "album",
            self.catalog.albums.loading,
            err.as_deref(),
            || self.catalog.albums.clear(),
        ) {
            return;
        }
        let value = self.catalog.albums.value.take();
        if let Some(detail) = &value {
            self.render_album_inner(ui, detail);
        }
        self.catalog.albums.value = value;
    }

    fn render_album_inner(&mut self, ui: &mut Ui, detail: &AlbumDetail) {
        let album = &detail.album;
        ui.horizontal(|ui| {
            let (art, _) = ui.allocate_exact_size(vec2(180.0, 180.0), egui::Sense::hover());
            let cover = album
                .artwork
                .as_ref()
                .and_then(|a| self.covers.get(ui.ctx(), &self.backend, &a.url, 300));
            if let Some(tex) = &cover {
                egui::Image::new(egui::load::SizedTexture::new(tex.id(), vec2(180.0, 180.0)))
                    .corner_radius(12.0)
                    .paint_at(ui, art);
            } else {
                ui.painter()
                    .rect_filled(art, egui::CornerRadius::from(12.0), c::PANEL);
                let letter = album
                    .title
                    .trim()
                    .chars()
                    .find(|c| c.is_alphanumeric())
                    .unwrap_or('♪');
                icons::letter_tile(ui.painter(), art, [12.0, 12.0, 12.0, 12.0], letter);
            }

            ui.allocate_ui_with_layout(
                vec2(340.0, 180.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.add_space(28.0);
                    ui.label(
                        RichText::new(&album.title)
                            .font(fonts::strong_font(24.0))
                            .color(c::TEXT),
                    );
                    ui.label(
                        RichText::new(format!("by {}", album.artist))
                            .size(14.0)
                            .color(c::DIM),
                    );
                    // Old detail heads counted the fetched tracks (and
                    // rendered nothing for an empty list).
                    let mut bits: Vec<String> = Vec::new();
                    if !detail.tracks.is_empty() {
                        bits.push(format!("{} songs", detail.tracks.len()));
                    }
                    if let Some(year) = album.release_date.as_deref().and_then(|d| d.get(..4)) {
                        bits.push(year.to_owned());
                    }
                    let meta = bits.join(" · ");
                    ui.label(RichText::new(meta).size(13.0).color(c::DIM));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let play = egui::Button::new(
                            RichText::new("Play")
                                .color(c::WHITE)
                                .font(fonts::strong_font(14.0)),
                        )
                        .fill(c::ACCENT)
                        .corner_radius(egui::CornerRadius::from(10.0))
                        .min_size(vec2(96.0, 34.0));
                        if ui.add(play).clicked() {
                            let entries = self.track_entries(&detail.tracks);
                            self.play_queue(ui.ctx(), entries, 0);
                        }
                        if ui
                            .add_enabled(
                                !detail.tracks.is_empty(),
                                egui::Button::new("Add to queue"),
                            )
                            .clicked()
                        {
                            let entries = self.track_entries(&detail.tracks);
                            let ctx = ui.ctx().clone();
                            self.queue_append(&ctx, entries);
                            self.set_status("Added to queue.");
                        }
                    });
                },
            );
        });
        ui.add_space(12.0);
        self.track_list(ui, &detail.tracks, None);
    }

    // -- artist detail -------------------------------------------------------------

    fn render_artist(&mut self, ui: &mut Ui) {
        if back_button(ui) {
            self.back();
            return;
        }
        ui.add_space(6.0);
        self.catalog.artists.poll();
        let id = self.catalog.artist_id.clone();
        if id.is_empty() {
            widgets::hint(ui, "No artist selected.");
            return;
        }
        if self.catalog.artists.value.is_none()
            && !self.catalog.artists.busy()
            && self.catalog.artists.error.is_none()
        {
            let state = self.backend.state.clone();
            let ctx = ui.ctx().clone();
            let rx = self.backend.call(&ctx, async move {
                shell::commands::get_artist(&state, id).await
            });
            self.catalog.artists.start(rx);
        }
        let err = self.catalog.artists.error.clone();
        if !slot_ready(
            ui,
            "artist",
            self.catalog.artists.loading,
            err.as_deref(),
            || self.catalog.artists.clear(),
        ) {
            return;
        }
        let value = self.catalog.artists.value.take();
        if let Some(detail) = &value {
            ui.label(
                RichText::new(&detail.artist.name)
                    .font(fonts::strong_font(26.0))
                    .color(c::TEXT),
            );
            if !detail.artist.genres.is_empty() {
                ui.label(
                    RichText::new(detail.artist.genres.join(" · "))
                        .size(13.0)
                        .color(c::DIM),
                );
            }
            ui.add_space(14.0);
            if detail.albums.is_empty() {
                widgets::hint(ui, "No albums to show.");
            } else {
                widgets::h3(ui, "Albums");
                ui.add_space(6.0);
                let cards = self.album_cards(ui, &detail.albums, 300);
                if let Some(i) = card_grid(ui, &cards) {
                    let album_id = detail.albums[i].id.clone();
                    self.open_album(&album_id);
                }
            }
        }
        self.catalog.artists.value = value;
    }

    // -- playlist detail -------------------------------------------------------------

    fn render_playlist(&mut self, ui: &mut Ui) {
        if back_button(ui) {
            self.back();
            return;
        }
        ui.add_space(6.0);
        self.catalog.playlist.poll();
        let id = self.catalog.playlist_id.clone();
        if id.is_empty() {
            widgets::hint(ui, "No playlist selected.");
            return;
        }
        if self.catalog.playlist.value.is_none()
            && !self.catalog.playlist.busy()
            && self.catalog.playlist.error.is_none()
        {
            let state = self.backend.state.clone();
            let ctx = ui.ctx().clone();
            let rx = self.backend.call(&ctx, async move {
                shell::commands::get_playlist(&state, id).await
            });
            self.catalog.playlist.start(rx);
        }
        let err = self.catalog.playlist.error.clone();
        if !slot_ready(
            ui,
            "playlist",
            self.catalog.playlist.loading,
            err.as_deref(),
            || self.catalog.playlist.clear(),
        ) {
            return;
        }
        let value = self.catalog.playlist.value.take();
        if let Some(detail) = &value {
            self.render_playlist_inner(ui, detail);
        }
        self.catalog.playlist.value = value;
    }

    fn render_playlist_inner(&mut self, ui: &mut Ui, detail: &PlaylistDetail) {
        let playlist = &detail.playlist;
        // Old detail head: art left, title/meta/description right.
        ui.horizontal(|ui| {
            let (art, _) = ui.allocate_exact_size(vec2(180.0, 180.0), egui::Sense::hover());
            let cover = playlist
                .artwork
                .as_ref()
                .and_then(|a| self.covers.get(ui.ctx(), &self.backend, &a.url, 300));
            if let Some(tex) = &cover {
                egui::Image::new(egui::load::SizedTexture::new(tex.id(), vec2(180.0, 180.0)))
                    .corner_radius(12.0)
                    .paint_at(ui, art);
            } else {
                ui.painter()
                    .rect_filled(art, egui::CornerRadius::from(12.0), c::PANEL);
                let letter = playlist
                    .name
                    .trim()
                    .chars()
                    .find(|c| c.is_alphanumeric())
                    .unwrap_or('♪');
                icons::letter_tile(ui.painter(), art, [12.0; 4], letter);
            }

            let col_w = ui.available_width().max(240.0);
            ui.allocate_ui_with_layout(
                vec2(col_w, 180.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(&playlist.name)
                            .font(fonts::strong_font(26.0))
                            .color(c::TEXT),
                    );
                    // Old playlist detail heads counted `d.tracks` too.
                    let mut meta = if detail.tracks.is_empty() {
                        String::new()
                    } else {
                        format!("{} songs", detail.tracks.len())
                    };
                    if playlist.is_public == Some(true) {
                        if !meta.is_empty() {
                            meta.push_str(" · ");
                        }
                        meta.push_str("public");
                    }
                    ui.label(RichText::new(meta).size(13.0).color(c::DIM));
                    if let Some(desc) = playlist
                        .description
                        .as_ref()
                        .filter(|d| !d.trim().is_empty())
                    {
                        ui.label(RichText::new(desc).size(13.0).color(c::DIM));
                    }
                },
            );
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let play = egui::Button::new(
                RichText::new("Play")
                    .color(c::WHITE)
                    .font(fonts::strong_font(14.0)),
            )
            .fill(c::ACCENT)
            .corner_radius(egui::CornerRadius::from(10.0))
            .min_size(vec2(96.0, 34.0));
            if ui.add(play).clicked() {
                let entries = self.track_entries(&detail.tracks);
                self.play_queue(ui.ctx(), entries, 0);
            }
            if ui
                .add_enabled(!detail.tracks.is_empty(), egui::Button::new("Add to queue"))
                .clicked()
            {
                let entries = self.track_entries(&detail.tracks);
                let ctx = ui.ctx().clone();
                self.queue_append(&ctx, entries);
                self.set_status("Added to queue.");
            }

            // Management row — old `openPlaylistDetailMenu` gating: only
            // library playlists (`p.…`) are mutable; Apple-curated ones
            // (`can_edit === false`) are read-only; Share works everywhere.
            let is_library = playlist.id.starts_with("p.");
            let editable = is_library && playlist.can_edit != Some(false);
            ui.separator();
            if editable && ui.button("Rename…").clicked() {
                self.catalog.dialog = Some(Dialog::RenamePlaylist {
                    id: playlist.id.clone(),
                    name: playlist.name.clone(),
                });
            }
            if ui.button("Share…").clicked() {
                let ctx = ui.ctx().clone();
                let p = playlist.clone();
                self.share_playlist_now(&ctx, &p);
            }
            if is_library && playlist.is_public != Some(false) {
                let label = if playlist.is_public == Some(true) {
                    "Unshare ✓ public"
                } else {
                    "Unshare…"
                };
                if ui.button(label).clicked() {
                    let ctx = ui.ctx().clone();
                    let p = playlist.clone();
                    self.unshare_playlist_now(&ctx, &p);
                }
            }
            if !detail.tracks.is_empty()
                && (editable || !is_library)
                && ui
                    .button(if is_library {
                        "Duplicate…"
                    } else {
                        "Save as playlist…"
                    })
                    .clicked()
            {
                self.catalog.dialog = Some(Dialog::DuplicatePlaylist {
                    ids: detail.tracks.iter().map(|t| t.id.clone()).collect(),
                    source: playlist.name.clone(),
                    name: format!("{} (copy)", playlist.name),
                });
            }
            if editable
                && ui
                    .button(egui::RichText::new("Delete…").color(c::ACCENT2))
                    .clicked()
            {
                self.catalog.dialog = Some(Dialog::DeletePlaylist {
                    id: playlist.id.clone(),
                    name: playlist.name.clone(),
                });
            }
        });
        ui.add_space(10.0);

        // Row-level "Remove from playlist" only for mutable library lists
        // (the old per-row `opts.onRemove`).
        let editable = playlist.id.starts_with("p.") && playlist.can_edit != Some(false);
        self.track_list(
            ui,
            &detail.tracks,
            if editable {
                Some(playlist.id.as_str())
            } else {
                None
            },
        );
    }

    // -- dialogs -------------------------------------------------------------------

    /// Create / rename / delete / duplicate playlist modals (egui has no
    /// native prompts). Rendered at the end of every view.
    fn render_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.catalog.dialog.clone() else {
            return;
        };
        let mut open = true;
        let mut confirmed = false;
        let mut cancelled = false;

        match dialog {
            Dialog::NewPlaylist { mut name, add_song } => {
                egui::Window::new("New playlist")
                    .collapsible(false)
                    .resizable(false)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label("Name");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut name)
                                .hint_text("My playlist")
                                .desired_width(280.0),
                        );
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            confirmed = true;
                        }
                        if add_song.is_some() {
                            ui.label(
                                egui::RichText::new("The track is added after creating.")
                                    .size(12.0)
                                    .color(c::DIM),
                            );
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Create").clicked() {
                                confirmed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                if confirmed && !name.trim().is_empty() {
                    let state = self.backend.state.clone();
                    let state2 = self.backend.state.clone();
                    let draft = name.trim().to_owned();
                    let msg_name = draft.clone();
                    self.backend.fire_msg(ctx, "create playlist", async move {
                        let new_id = shell::commands::create_playlist(&state, draft).await?;
                        match add_song {
                            // Track-menu "+ New playlist…": create, then append.
                            Some(song) => {
                                shell::commands::add_to_playlist(&state2, new_id, vec![song]).await
                            }
                            None => Ok(format!("Playlist “{msg_name}” created.")),
                        }
                    });
                    self.catalog.playlists.clear();
                }
            }
            Dialog::RenamePlaylist { id, mut name } => {
                egui::Window::new("Rename playlist")
                    .collapsible(false)
                    .resizable(false)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label("Name");
                        let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(280.0));
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            confirmed = true;
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                confirmed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                if confirmed && !name.trim().is_empty() {
                    let state = self.backend.state.clone();
                    let new_name = name.trim().to_owned();
                    self.backend.fire_msg(ctx, "rename playlist", async move {
                        shell::commands::rename_playlist(&state, id, new_name).await
                    });
                    self.catalog.playlists.clear();
                    self.catalog.playlist.clear();
                }
            }
            Dialog::DeletePlaylist { id, name } => {
                egui::Window::new("Delete playlist")
                    .collapsible(false)
                    .resizable(false)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label(format!("Delete “{name}”? This cannot be undone."));
                        ui.horizontal(|ui| {
                            if ui
                                .button(egui::RichText::new("Delete").color(c::ACCENT2))
                                .clicked()
                            {
                                confirmed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                if confirmed {
                    let state = self.backend.state.clone();
                    let vid = id.clone();
                    self.backend.fire_msg(ctx, "delete playlist", async move {
                        shell::commands::delete_playlist(&state, vid).await
                    });
                    self.catalog.playlists.clear();
                    self.catalog.playlist.clear();
                    if self.view == View::Playlist(id) {
                        self.back();
                    }
                }
            }
            Dialog::DuplicatePlaylist {
                ids,
                source,
                mut name,
            } => {
                let n = ids.len();
                egui::Window::new("Duplicate playlist")
                    .collapsible(false)
                    .resizable(false)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label(format!("Duplicate “{source}” as:"));
                        let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(280.0));
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            confirmed = true;
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Duplicate").clicked() {
                                confirmed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                if confirmed && !name.trim().is_empty() {
                    let state = self.backend.state.clone();
                    let state2 = self.backend.state.clone();
                    let new_name = name.trim().to_owned();
                    self.backend
                        .fire_msg(ctx, "duplicate playlist", async move {
                            let new_id = shell::commands::create_playlist(&state, new_name).await?;
                            if ids.is_empty() {
                                return Ok("Playlist duplicated (empty).".to_string());
                            }
                            shell::commands::add_to_playlist(&state2, new_id, ids).await?;
                            Ok(format!("Playlist duplicated ({n} tracks)."))
                        });
                    self.catalog.playlists.clear();
                }
            }
        }

        if !open || cancelled || confirmed {
            self.catalog.dialog = None;
        }
    }

    // -- settings -------------------------------------------------------------------

    pub fn render_settings(&mut self, ui: &mut Ui) {
        widgets::h2(ui, "Settings");
        ui.add_space(8.0);

        widgets::h3(ui, "Playback");
        widgets::settings_card(ui, |ui| {
            ui.label("Playback is in-process (no browser engine).");
            ui.label(
                RichText::new(format!(
                    "Queue: {} tracks — loop and infinite modes are in the player bar.",
                    self.player.queue.len()
                ))
                .size(13.0)
                .color(c::DIM),
            );
        });

        ui.add_space(12.0);
        widgets::h3(ui, "Display");
        widgets::settings_card(ui, |ui| {
            ui.checkbox(&mut self.settings.fs_lyrics, "lyrics in fullscreen");
            ui.checkbox(&mut self.settings.lyrics_focus, "focused lyrics (±2)");
            combo(
                ui,
                "fs-layout",
                &mut self.settings.fs_layout,
                "Fullscreen layout",
                &[
                    (FsLayout::Vertical, "vertical"),
                    (FsLayout::Horizontal, "horizontal"),
                ],
            );
            ui.checkbox(&mut self.settings.native_fs, "native fullscreen")
                .on_hover_text(
                    "True fullscreen hides window decorations but aborts the app on some \
                 GPU/compositor combos — the overlay covers the screen either way",
                );
        });

        ui.add_space(12.0);
        widgets::h3(ui, "Immersive fullscreen");
        widgets::settings_card(ui, |ui| {
            ui.checkbox(
                &mut self.settings.immersive,
                "immersive fullscreen (default, uncheck for classic)",
            );
            combo(
                ui,
                "im-layout",
                &mut self.settings.im_layout,
                "Immersive layout",
                &[
                    (ImLayout::Split, "split (cover + lyrics)"),
                    (ImLayout::Overlay, "overlay (lyrics over background)"),
                ],
            );
            combo(
                ui,
                "im-bg",
                &mut self.settings.im_bg,
                "Background",
                &[
                    (ImBg::Flow, "immersive (old)"),
                    (ImBg::Static, "blurred still cover"),
                    (ImBg::Motion, "animated cover"),
                    (ImBg::Dynamic, "immersive"),
                    (ImBg::Gradient, "colors only"),
                ],
            );
            ui.checkbox(&mut self.settings.motion_covers, "animated covers (video)");
            combo(
                ui,
                "im-cover",
                &mut self.settings.im_cover,
                "Foreground cover",
                &[
                    (ImCover::Animated, "animated"),
                    (ImCover::Still, "still"),
                    (ImCover::Hidden, "hidden"),
                ],
            );
            ui.horizontal(|ui| {
                ui.label("Blur");
                ui.add(
                    egui::Slider::new(&mut self.settings.im_blur, 0.0..=40.0)
                        .fixed_decimals(0)
                        .show_value(false),
                );
                ui.label(
                    RichText::new(format!("{}", self.settings.im_blur.round() as i32))
                        .size(12.0)
                        .color(c::DIM),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Dim");
                ui.add(
                    egui::Slider::new(&mut self.settings.im_dim, 0.0..=90.0)
                        .fixed_decimals(0)
                        .show_value(false),
                );
                ui.label(
                    RichText::new(format!("{}", self.settings.im_dim.round() as i32))
                        .size(12.0)
                        .color(c::DIM),
                );
            });
        });

        ui.add_space(12.0);
        widgets::h3(ui, "Notifications");
        widgets::settings_card(ui, |ui| {
            ui.checkbox(
                &mut self.settings.notifications,
                "track-change notifications",
            );
            ui.label(
                RichText::new(
                    "Shows a desktop notification with cover art when the track changes \
                     (off by default).",
                )
                .size(13.0)
                .color(c::DIM),
            );
        });

        ui.add_space(12.0);
        widgets::h3(ui, "Discord");
        widgets::settings_card(ui, |ui| {
            ui.checkbox(&mut self.settings.discord, "show Discord status");
            ui.horizontal(|ui| {
                ui.label("App ID");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.discord_app_id)
                        .hint_text("built-in Sonora app")
                        .desired_width(240.0),
                )
                .on_hover_text(
                    "Optional: paste your own Discord application ID to use a personal app \
                     instead of the built-in one",
                );
            });
            ui.label(
                RichText::new(
                    "Shows the playing song on Discord while music plays (leave empty for \
                     the built-in app). Needs the Discord desktop app running.",
                )
                .size(13.0)
                .color(c::DIM),
            );
        });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            widgets::h3(ui, "Account");
            ui.label(
                RichText::new("(token source checked at sign-in)")
                    .size(12.0)
                    .color(c::DIM),
            );
        });
        let signed_in = self.auth.signed_in;
        let token_status = self.auth.token_status.clone();
        let signin_busy = self.auth.signin.busy();
        widgets::settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                let sign_in = ui
                    .add_enabled(!signin_busy, egui::Button::new("Sign in with Apple Music"))
                    .on_hover_text("Opens the browser for a one-shot localhost approval");
                if sign_in.clicked() {
                    let ctx = ui.ctx().clone();
                    self.auth_start_signin(&ctx);
                }
                let cancel = ui
                    .add_enabled(signin_busy, egui::Button::new("Cancel"))
                    .on_hover_text("Abort a pending browser sign-in");
                if cancel.clicked() {
                    let ctx = ui.ctx().clone();
                    self.auth_cancel_signin(&ctx);
                }
                let logout = ui
                    .add_enabled(signed_in == Some(true), egui::Button::new("Log out"))
                    .on_hover_text("Stops playback and clears the queue");
                if logout.clicked() {
                    let ctx = ui.ctx().clone();
                    self.auth_logout(&ctx);
                }
            });
            ui.add_space(4.0);
            let state_text = if signin_busy {
                "Waiting for browser sign-in…".to_string()
            } else {
                match (signed_in, token_status.as_deref()) {
                    (Some(true), Some(tok)) => format!("Signed in. Token: {tok}"),
                    (Some(true), None) => "Signed in.".to_string(),
                    (Some(false), _) => "Not signed in.".to_string(),
                    (None, _) => "…".to_string(),
                }
            };
            ui.label(RichText::new(state_text).size(13.0).color(c::DIM));
            ui.add_space(4.0);
            ui.checkbox(&mut self.settings.advanced_auth, "Advanced login")
                .on_hover_text("Show the manual token-paste fallback");

            if self.settings.advanced_auth {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Get authorize URL").clicked() {
                        let ctx = ui.ctx().clone();
                        self.auth_get_url(&ctx);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("MUT");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.mut_input)
                            .hint_text("paste Music-User-Token")
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button("Save token").clicked() {
                        let ctx = ui.ctx().clone();
                        self.auth_submit_token(&ctx);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Redirect URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.auth_url_input)
                            .hint_text("…or paste authorize redirect URL")
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button("Save from URL").clicked() {
                        let ctx = ui.ctx().clone();
                        self.auth_submit_url(&ctx);
                    }
                });
            }
        });
    }
}
