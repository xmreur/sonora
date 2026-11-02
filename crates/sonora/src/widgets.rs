//! Shared building blocks: headings, track rows, cards, truncation.

use egui::epaint::{Mesh, Vertex};
use egui::text::LayoutJob;
use egui::{
    pos2, vec2, Align2, Color32, CornerRadius, FontId, Margin, Painter, Pos2, Rect, RichText,
    Sense, Stroke, TextureHandle, Ui,
};

use crate::fonts;
use crate::icons;
use crate::theme::c;

/// `m:ss` time formatting (matches the old `fmtTime`).
pub fn fmt_time(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Section heading (old `.view h2`).
pub fn h2(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(fonts::strong_font(20.0))
            .color(c::TEXT),
    );
}

/// Sub-section heading (old `.view h3`: uppercase, dim, small).
pub fn h3(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .font(fonts::strong_font(13.0))
            .color(c::DIM),
    );
}

/// Dim secondary text.
pub fn hint(ui: &mut Ui, text: &str) -> egui::Response {
    ui.label(RichText::new(text).size(13.0).color(c::DIM))
}

/// Layout one line of text, elided with `…` when wider than `max_width`.
pub fn paint_truncated(
    painter: &egui::Painter,
    text: &str,
    font_id: FontId,
    pos: Pos2,
    max_width: f32,
    color: Color32,
) {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id,
            color,
            ..Default::default()
        },
    );
    truncate_job(&mut job, max_width);
    let galley = painter.layout_job(job);
    painter.galley(pos, galley, color);
}

/// Restrict a job to a single elided row.
pub fn truncate_job(job: &mut LayoutJob, max_width: f32) {
    job.wrap.max_width = max_width.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
}

/// Column geometry of the old `.track` grid:
/// `40px num | 44px art | 1fr title | auto duration`, gap 10, padding 7/8.
const NUM_COL: f32 = 40.0;
const ART: f32 = 44.0;
const ROW_GAP: f32 = 10.0;
const ROW_H: f32 = 58.0;

/// One row of a track list (browse, search, album, playlist, queue).
#[derive(Clone, Default)]
pub struct TrackRow {
    pub num: Option<u32>,
    pub letter: Option<char>,
    /// Cover texture once `Covers` resolved it; `None` keeps the tile.
    pub cover: Option<TextureHandle>,
    pub title: String,
    /// Combined subtitle when artist and album are not separate links.
    pub sub: String,
    /// Clickable artist. Empty keeps [`Self::sub`].
    pub artist: String,
    /// Clickable album. Empty keeps [`Self::sub`] when artist is empty too.
    pub album: String,
    pub duration: Option<String>,
    pub unavailable: bool,
    pub current: bool,
    pub saved: bool,
    pub pending: bool,
    pub failed: bool,
    pub radio: bool,
}

impl TrackRow {
    /// Simple numbered row: `1 · Title — Artist`, `3:41`.
    pub fn simple(num: u32, title: &str, sub: &str, secs: u64) -> Self {
        Self {
            num: Some(num),
            letter: title.trim().chars().next(),
            title: title.to_owned(),
            sub: sub.to_owned(),
            duration: Some(fmt_time(secs * 1000)),
            ..Default::default()
        }
    }
}

/// Which part of a track row was clicked. The row itself still plays;
/// the artist and album names open their pages and do not.
pub struct TrackRowHit {
    pub response: egui::Response,
    pub artist_clicked: bool,
    pub album_clicked: bool,
}

pub fn track_row(ui: &mut Ui, row: &TrackRow) -> TrackRowHit {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width().max(1.0), ROW_H), Sense::click());
    let painter = ui.painter();
    let cy = rect.center().y;

    if resp.hovered() || row.current {
        painter.rect_filled(
            Rect::from_min_size(
                pos2(rect.left(), rect.top() + 2.0),
                vec2(rect.width(), ROW_H - 4.0),
            ),
            CornerRadius::from(8.0),
            c::PANEL2,
        );
    }
    if row.current {
        painter.rect_filled(
            Rect::from_min_size(pos2(rect.left(), rect.top() + 8.0), vec2(3.0, ROW_H - 16.0)),
            2.0,
            c::ACCENT2,
        );
    }

    let (title_c, sub_c) = if row.unavailable {
        (c::DIM, c::DIM)
    } else {
        (c::TEXT, c::DIM)
    };

    // Number, or a play glyph for the current row.
    if row.current {
        icons::play(
            painter,
            Rect::from_center_size(pos2(rect.left() + 16.0, cy), vec2(20.0, 20.0)),
            c::ACCENT2,
        );
    } else if let Some(n) = row.num {
        painter.text(
            pos2(rect.left() + NUM_COL - 6.0, cy),
            Align2::RIGHT_CENTER,
            n.to_string(),
            fonts::body_font(13.0),
            c::DIM,
        );
    }

    // Artwork: cover texture when it landed, letter tile until then.
    let art_x = rect.left() + NUM_COL + ROW_GAP;
    let art_rect = Rect::from_min_size(pos2(art_x, cy - ART / 2.0), vec2(ART, ART));
    if let Some(tex) = &row.cover {
        paint_cover(painter, art_rect, tex, 6.0);
    } else if let Some(letter) = row.letter {
        icons::letter_tile(painter, art_rect, [6.0; 4], letter);
    } else {
        painter.rect_filled(art_rect, 6.0, c::PANEL2);
    }
    if row.unavailable {
        painter.rect_filled(
            art_rect,
            6.0,
            Color32::from_rgba_premultiplied(0x14, 0x14, 0x19, 150),
        );
    }

    // Title (+ optional " · radio" suffix) and sub line.
    let title_x = art_rect.right() + ROW_GAP;
    let right_x = rect.right() - 8.0;
    let dur_w = if row.duration.is_some() { 46.0 } else { 0.0 };
    let mark = if row.saved {
        Some("↓")
    } else if row.pending {
        Some("…")
    } else if row.failed {
        Some("×")
    } else {
        None
    };
    let saved_w = if mark.is_some() { 18.0 } else { 0.0 };
    let text_w = (right_x - dur_w - saved_w - title_x - 8.0).max(20.0);

    let mut job = LayoutJob::default();
    job.append(
        &row.title,
        0.0,
        egui::TextFormat {
            font_id: fonts::body_font(14.0),
            color: title_c,
            ..Default::default()
        },
    );
    if row.radio {
        job.append(
            " · radio",
            0.0,
            egui::TextFormat {
                font_id: fonts::body_font(11.0),
                color: c::DIM,
                ..Default::default()
            },
        );
    }
    truncate_job(&mut job, text_w);
    let galley = painter.layout_job(job);
    painter.galley(pos2(title_x, cy - 15.0), galley, title_c);

    let (artist_clicked, album_clicked) =
        paint_sub_line(ui, resp.id, pos2(title_x, cy + 3.0), text_w, row, sub_c);

    if let Some(mark) = mark {
        painter.text(
            pos2(right_x - dur_w - 4.0, cy),
            Align2::RIGHT_CENTER,
            mark,
            fonts::body_font(13.0),
            c::ACCENT2,
        );
    }
    if let Some(d) = &row.duration {
        painter.text(
            pos2(right_x, cy),
            Align2::RIGHT_CENTER,
            d,
            fonts::body_font(12.0),
            c::DIM,
        );
    }

    TrackRowHit {
        response: resp.on_hover_cursor(egui::CursorIcon::PointingHand),
        artist_clicked,
        album_clicked,
    }
}

/// Share `max_width` between an artist and an album, keeping the separator.
fn sub_widths(artist: f32, sep: f32, album: f32, max_width: f32) -> (f32, f32) {
    let max_width = max_width.max(0.0);
    if artist <= 0.0 {
        return (0.0, album.min(max_width));
    }
    if album <= 0.0 {
        return (artist.min(max_width), 0.0);
    }
    let budget = (max_width - sep.min(max_width)).max(0.0);
    if artist + album <= budget {
        return (artist, album);
    }
    let half = budget / 2.0;
    if artist <= half {
        return (artist, (budget - artist).max(0.0));
    }
    if album <= half {
        return ((budget - album).max(0.0), album);
    }
    (half, (budget - half).max(0.0))
}

fn text_width(painter: &Painter, text: &str, font: FontId) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    painter
        .layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER)
        .size()
        .x
}

/// Artist and album as separate links (`Artist · Album`). A row that only
/// has `sub` stays one plain line.
fn paint_sub_line(
    ui: &Ui,
    row_id: egui::Id,
    pos: Pos2,
    max_width: f32,
    row: &TrackRow,
    color: Color32,
) -> (bool, bool) {
    let artist = row.artist.trim();
    let album = row.album.trim();
    let font = fonts::body_font(12.0);
    if artist.is_empty() && album.is_empty() {
        paint_truncated(ui.painter(), &row.sub, font, pos, max_width, color);
        return (false, false);
    }
    let painter = ui.painter();
    let sep = if artist.is_empty() || album.is_empty() {
        ""
    } else {
        " · "
    };
    let sep_w = text_width(painter, sep, font.clone());
    let (artist_w, album_w) = sub_widths(
        text_width(painter, artist, font.clone()),
        sep_w,
        text_width(painter, album, font.clone()),
        max_width,
    );
    let h = 16.0;
    let mut x = pos.x;
    let artist_clicked = if artist_w > 0.0 {
        let rect = Rect::from_min_size(pos2(x, pos.y), vec2(artist_w, h));
        x += artist_w;
        text_link(
            ui,
            row_id.with("artist"),
            rect,
            artist,
            font.clone(),
            color,
            "Open artist",
        )
    } else {
        false
    };
    if !sep.is_empty() && artist_w > 0.0 && album_w > 0.0 {
        painter.text(pos2(x, pos.y), Align2::LEFT_TOP, sep, font.clone(), color);
        x += sep_w;
    }
    let album_clicked = if album_w > 0.0 {
        let rect = Rect::from_min_size(pos2(x, pos.y), vec2(album_w, h));
        text_link(
            ui,
            row_id.with("album"),
            rect,
            album,
            font,
            color,
            "Open album",
        )
    } else {
        false
    };
    (artist_clicked, album_clicked)
}

fn text_link(
    ui: &Ui,
    id: egui::Id,
    rect: Rect,
    text: &str,
    font: FontId,
    color: Color32,
    tip: &str,
) -> bool {
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip);
    let color = if resp.hovered() { c::ACCENT2 } else { color };
    paint_truncated(
        ui.painter(),
        text,
        font,
        rect.left_top(),
        rect.width(),
        color,
    );
    if resp.hovered() {
        ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0, color),
        );
    }
    resp.clicked()
}

/// A short name that opens its page. Hover turns accent and underlines.
pub fn name_link(ui: &mut Ui, text: &str, font: FontId, color: Color32, tip: &str) -> bool {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), color);
    let (rect, _) = ui.allocate_exact_size(galley.size(), Sense::hover());
    text_link(
        ui,
        ui.scope_id().with((tip, text)),
        rect,
        text,
        font,
        color,
        tip,
    )
}

/// A card in one of the grids (album/playlist/chart tiles).
#[derive(Clone)]
pub struct Card {
    pub title: String,
    pub sub: String,
    pub letter: char,
    /// Cover texture once `Covers` resolved it; `None` keeps the tile.
    pub cover: Option<TextureHandle>,
}

impl Card {
    pub fn new(title: &str, sub: &str) -> Self {
        let letter = title
            .trim()
            .chars()
            .find(|ch| ch.is_alphanumeric())
            .unwrap_or('♪');
        Self {
            title: title.to_owned(),
            sub: sub.to_owned(),
            letter,
            cover: None,
        }
    }
}

pub fn card(ui: &mut Ui, width: f32, card: &Card) -> egui::Response {
    let height = width + 56.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let painter = ui.painter();

    painter.rect_filled(rect, CornerRadius::from(12.0), c::PANEL);
    if resp.hovered() {
        painter.rect_stroke(
            rect,
            CornerRadius::from(12.0),
            Stroke::new(1.0, c::LINE),
            egui::StrokeKind::Outside,
        );
    }

    let art_rect = Rect::from_min_size(rect.min, vec2(width, width));
    if let Some(tex) = &card.cover {
        paint_cover(painter, art_rect, tex, 12.0);
    } else {
        icons::letter_tile(painter, art_rect, [12.0, 12.0, 0.0, 0.0], card.letter);
    }

    let inner = width - 20.0;
    paint_truncated(
        painter,
        &card.title,
        fonts::strong_font(13.0),
        pos2(rect.left() + 10.0, art_rect.bottom() + 8.0),
        inner,
        c::TEXT,
    );
    paint_truncated(
        painter,
        &card.sub,
        fonts::body_font(12.0),
        pos2(rect.left() + 10.0, art_rect.bottom() + 26.0),
        inner,
        c::DIM,
    );

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// How many `min_card`-wide cards fit per row and how wide they end up
/// (the CSS `grid-template-columns: repeat(auto-fill, minmax(150px, 1fr))`).
/// Object-fit cover into `rect`, cropped to the destination aspect and
/// clipped to a rounded rect. Stretching a portrait frame into a square
/// is what made stills and motion covers look wrong.
pub fn paint_cover(painter: &Painter, rect: Rect, tex: &TextureHandle, radius: f32) {
    let [tw, th] = tex.size();
    if tw == 0 || th == 0 || rect.width() <= 1.0 || rect.height() <= 1.0 {
        return;
    }
    let ta = tw as f32 / th as f32;
    let ra = rect.width() / rect.height();
    let uv = if ta > ra {
        let w = ra / ta;
        Rect::from_x_y_ranges((0.5 - w * 0.5)..=(0.5 + w * 0.5), 0.0..=1.0)
    } else {
        let h = ta / ra;
        Rect::from_x_y_ranges(0.0..=1.0, (0.5 - h * 0.5)..=(0.5 + h * 0.5))
    };
    let mut mesh = Mesh::with_texture(tex.id());
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let max_r = (rect.width() / 2.0).min(rect.height() / 2.0);
    let rad = radius.min(max_r);
    let uv_at = |p: Pos2| {
        let x = ((p.x - l) / rect.width()).clamp(0.0, 1.0);
        let y = ((p.y - t) / rect.height()).clamp(0.0, 1.0);
        pos2(uv.left() + x * uv.width(), uv.top() + y * uv.height())
    };
    if rad < 0.5 {
        for p in [
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ] {
            mesh.vertices.push(Vertex {
                pos: p,
                uv: uv_at(p),
                color: Color32::WHITE,
            });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(mesh);
        return;
    }
    const SEG: usize = 6;
    let centers = [
        pos2(l + rad, t + rad),
        pos2(r - rad, t + rad),
        pos2(r - rad, b - rad),
        pos2(l + rad, b - rad),
    ];
    let arcs = [
        (std::f32::consts::PI, std::f32::consts::FRAC_PI_2),
        (
            std::f32::consts::FRAC_PI_2 * 3.0,
            std::f32::consts::FRAC_PI_2,
        ),
        (0.0, std::f32::consts::FRAC_PI_2),
        (std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2),
    ];
    let center = rect.center();
    mesh.vertices.push(Vertex {
        pos: center,
        uv: uv_at(center),
        color: Color32::WHITE,
    });
    for (i, c) in centers.iter().enumerate() {
        let (a0, sweep) = arcs[i];
        for s in 0..=SEG {
            let a = a0 + sweep * s as f32 / SEG as f32;
            let p = pos2(c.x + rad * a.cos(), c.y + rad * a.sin());
            mesh.vertices.push(Vertex {
                pos: p,
                uv: uv_at(p),
                color: Color32::WHITE,
            });
        }
    }
    let n = (mesh.vertices.len() - 1) as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + ((i + 1) % n));
    }
    painter.add(mesh);
}

pub fn cards_layout(ui: &Ui, min_card: f32) -> (usize, f32) {
    let avail = ui.available_width();
    let gap = 16.0;
    let n = (((avail + gap) / (min_card + gap)).floor() as usize).max(1);
    let w = (avail - gap * (n as f32 - 1.0)) / n as f32;
    (n, w.max(40.0))
}

/// A bordered settings card (old `.settings-grid`), capped at 520 px.
pub fn settings_card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let width = 520.0_f32.min(ui.available_width());
    ui.allocate_ui_with_layout(
        vec2(width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::new()
                .fill(c::PANEL)
                .stroke(Stroke::new(1.0, c::LINE))
                .corner_radius(CornerRadius::from(12.0))
                .inner_margin(Margin::symmetric(16, 14))
                .show(ui, add);
        },
    );
}

/// Which screen is waiting on a catalog fetch.
#[derive(Clone, Copy)]
pub enum PageSkeleton {
    Browse,
    Search,
    Playlists,
    Album,
    Artist,
    Playlist,
    Lyrics,
}

/// Placeholder layout for a page that is still fetching. The blocks pulse
/// until the caller stops showing this.
pub fn page_skeleton(ui: &mut Ui, kind: PageSkeleton) {
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(32));
    let color = skeleton_color(ui.input(|i| i.time));
    match kind {
        PageSkeleton::Browse => {
            bone_bar(ui, 150.0, 20.0, color);
            ui.add_space(8.0);
            bone_rows(ui, 6, color);
            ui.add_space(18.0);
            bone_bar(ui, 130.0, 20.0, color);
            ui.add_space(8.0);
            bone_cards(ui, 1, color);
        }
        PageSkeleton::Search => {
            bone_bar(ui, 70.0, 14.0, color);
            ui.add_space(8.0);
            bone_rows(ui, 5, color);
            ui.add_space(16.0);
            bone_bar(ui, 80.0, 14.0, color);
            ui.add_space(8.0);
            bone_cards(ui, 1, color);
        }
        PageSkeleton::Playlists => bone_cards(ui, 2, color),
        PageSkeleton::Album | PageSkeleton::Playlist => {
            bone_detail(ui, color);
            ui.add_space(12.0);
            bone_rows(ui, 8, color);
        }
        PageSkeleton::Artist => {
            bone_bar(ui, 220.0, 26.0, color);
            ui.add_space(8.0);
            bone_bar(ui, 140.0, 13.0, color);
            ui.add_space(16.0);
            bone_bar(ui, 72.0, 13.0, color);
            ui.add_space(8.0);
            bone_cards(ui, 2, color);
        }
        PageSkeleton::Lyrics => {
            ui.vertical_centered(|ui| {
                bone_bar(ui, 200.0, 18.0, color);
                ui.add_space(8.0);
                bone_bar(ui, 120.0, 14.0, color);
                ui.add_space(22.0);
                for width in [280.0, 220.0, 250.0, 160.0, 200.0] {
                    bone_bar(ui, width, 16.0, color);
                    ui.add_space(12.0);
                }
            });
        }
    }
}

/// Line placeholders for a lyrics column. Bars follow the caller's layout,
/// so a centered pane centers them and a left pane keeps them on the left.
pub fn lyric_line_skeleton(ui: &mut Ui) {
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(32));
    let color = skeleton_color(ui.input(|i| i.time));
    // Widths are fractions of the column. A fixed pixel width becomes a
    // full-bleed block in the narrow overlay pane, unlike a lyric line.
    for frac in [0.62, 0.44, 0.72, 0.36, 0.54] {
        let width = (ui.available_width() * frac).max(40.0);
        bone_bar(ui, width, 18.0, color);
        ui.add_space(20.0);
    }
}

fn skeleton_color(time: f64) -> Color32 {
    let wave = (time * 2.4).sin() as f32 * 0.5 + 0.5;
    lerp_color(c::LINE, Color32::from_rgb(0x3e, 0x3e, 0x4a), wave)
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let [ar, ag, ab, aa] = a.to_srgba_unmultiplied();
    let [br, bg, bb, ba] = b.to_srgba_unmultiplied();
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t) as u8;
    Color32::from_rgba_unmultiplied(mix(ar, br), mix(ag, bg), mix(ab, bb), mix(aa, ba))
}

fn bone_bar(ui: &mut Ui, width: f32, height: f32, color: Color32) {
    let width = width.min(ui.available_width()).max(1.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::from(6.0), color);
}

fn bone_rows(ui: &mut Ui, count: usize, color: Color32) {
    for i in 0..count {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
        let painter = ui.painter();
        let cy = rect.center().y;
        let art = Rect::from_min_size(pos2(rect.left() + 50.0, cy - ART / 2.0), vec2(ART, ART));
        painter.rect_filled(art, CornerRadius::from(6.0), color);
        let title_w = (rect.width() * (0.22 + (i % 3) as f32 * 0.07)).min(280.0);
        painter.rect_filled(
            Rect::from_min_size(pos2(art.right() + 12.0, cy - 14.0), vec2(title_w, 12.0)),
            CornerRadius::from(4.0),
            color,
        );
        let sub_w = (rect.width() * (0.12 + (i % 2) as f32 * 0.05)).min(180.0);
        painter.rect_filled(
            Rect::from_min_size(pos2(art.right() + 12.0, cy + 4.0), vec2(sub_w, 10.0)),
            CornerRadius::from(4.0),
            color,
        );
        painter.rect_filled(
            Rect::from_min_size(pos2(rect.right() - 44.0, cy - 5.0), vec2(32.0, 10.0)),
            CornerRadius::from(4.0),
            color,
        );
    }
}

fn bone_cards(ui: &mut Ui, rows: usize, color: Color32) {
    let (per_row, width) = cards_layout(ui, 150.0);
    let count = per_row.saturating_mul(rows).max(per_row);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(16.0, 16.0);
        ui.horizontal_wrapped(|ui| {
            for i in 0..count {
                let (rect, _) = ui.allocate_exact_size(vec2(width, width + 56.0), Sense::hover());
                let art = Rect::from_min_size(rect.min, vec2(width, width));
                ui.painter()
                    .rect_filled(art, CornerRadius::from(12.0), color);
                let title_w = (width - 20.0) * (0.55 + (i % 3) as f32 * 0.12);
                ui.painter().rect_filled(
                    Rect::from_min_size(
                        pos2(rect.left() + 10.0, art.bottom() + 10.0),
                        vec2(title_w, 12.0),
                    ),
                    CornerRadius::from(4.0),
                    color,
                );
                ui.painter().rect_filled(
                    Rect::from_min_size(
                        pos2(rect.left() + 10.0, art.bottom() + 28.0),
                        vec2(title_w * 0.6, 10.0),
                    ),
                    CornerRadius::from(4.0),
                    color,
                );
            }
        });
    });
}

fn bone_detail(ui: &mut Ui, color: Color32) {
    ui.horizontal(|ui| {
        let side = (ui.available_width() * 0.28).clamp(112.0, 220.0);
        let (art, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
        ui.painter()
            .rect_filled(art, CornerRadius::from(12.0), color);
        let col_w = ui.available_width().max(80.0);
        ui.allocate_ui_with_layout(
            vec2(col_w, side),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.add_space(24.0);
                bone_bar(ui, col_w * 0.62, 24.0, color);
                ui.add_space(10.0);
                bone_bar(ui, col_w * 0.36, 14.0, color);
                ui.add_space(8.0);
                bone_bar(ui, col_w * 0.28, 12.0, color);
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::sub_widths;

    #[test]
    fn sub_widths_keep_both_names_when_they_fit() {
        assert_eq!(sub_widths(40.0, 10.0, 30.0, 100.0), (40.0, 30.0));
    }

    #[test]
    fn sub_widths_split_the_remainder_when_both_are_long() {
        assert_eq!(sub_widths(80.0, 10.0, 80.0, 50.0), (20.0, 20.0));
    }

    #[test]
    fn sub_widths_give_the_short_name_its_full_width() {
        assert_eq!(sub_widths(10.0, 10.0, 80.0, 40.0), (10.0, 20.0));
        assert_eq!(sub_widths(0.0, 10.0, 80.0, 40.0), (0.0, 40.0));
    }
}
