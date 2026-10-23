//! Shared building blocks: headings, track rows, cards, truncation.

use egui::text::LayoutJob;
use egui::{
    pos2, vec2, Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, RichText, Sense, Stroke,
    TextureHandle, Ui,
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
    pub sub: String,
    pub duration: Option<String>,
    pub unavailable: bool,
    pub current: bool,
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

pub fn track_row(ui: &mut Ui, row: &TrackRow) -> egui::Response {
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
        egui::Image::new(egui::load::SizedTexture::new(tex.id(), vec2(ART, ART)))
            .corner_radius(6.0)
            .paint_at(ui, art_rect);
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
    let text_w = (right_x - dur_w - title_x - 8.0).max(20.0);

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

    paint_truncated(
        painter,
        &row.sub,
        fonts::body_font(12.0),
        pos2(title_x, cy + 3.0),
        text_w,
        sub_c,
    );

    if let Some(d) = &row.duration {
        painter.text(
            pos2(right_x, cy),
            Align2::RIGHT_CENTER,
            d,
            fonts::body_font(12.0),
            c::DIM,
        );
    }

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
        egui::Image::new(egui::load::SizedTexture::new(tex.id(), art_rect.size()))
            .corner_radius(CornerRadius {
                nw: 12,
                ne: 12,
                sw: 0,
                se: 0,
            })
            .paint_at(ui, art_rect);
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
