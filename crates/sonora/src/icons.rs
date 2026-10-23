//! Painter-drawn glyphs and small graphics helpers.
//!
//! egui ships no icon set, and relying on symbol glyphs (`⏮`, `♪`, …)
//! being present in whatever system font happens to be loaded is fragile —
//! so every chrome/transport icon is drawn from geometric primitives.

use egui::epaint::{Mesh, PathShape, Vertex};
use egui::{
    pos2, vec2, Align2, Color32, CornerRadius, Painter, Pos2, Rect, Sense, Stroke, Ui, Vec2,
};

use crate::fonts;
use crate::theme::c;

/// A small clickable icon button with a circular hover background.
pub fn button(
    ui: &mut Ui,
    size: Vec2,
    draw: impl FnOnce(&Painter, Rect, Color32),
    tooltip: &str,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let interactive = resp.enabled();
    if interactive && resp.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::from(size.y.min(size.x) / 2.0),
            c::PANEL2,
        );
    }
    let color = if interactive { c::TEXT } else { c::DIM };
    draw(ui.painter(), rect, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tooltip)
}

// ---------------------------------------------------------------------------
// Transport & chrome glyphs
// ---------------------------------------------------------------------------

pub fn play(painter: &Painter, rect: Rect, color: Color32) {
    let cy = rect.center().y;
    let x0 = rect.left() + rect.width() * 0.32;
    let x1 = rect.right() - rect.width() * 0.26;
    let hh = rect.height() * 0.36;
    painter.add(PathShape::convex_polygon(
        vec![pos2(x0, cy - hh), pos2(x0, cy + hh), pos2(x1, cy)],
        color,
        Stroke::NONE,
    ));
}

pub fn pause(painter: &Painter, rect: Rect, color: Color32) {
    let w = rect.width();
    let h = rect.height();
    let cy = rect.center().y;
    let bw = w * 0.13;
    let gap = w * 0.11;
    let bh = h * 0.68;
    let x0 = rect.center().x - (bw * 2.0 + gap) / 2.0;
    let y0 = cy - bh / 2.0;
    painter.rect_filled(Rect::from_min_size(pos2(x0, y0), vec2(bw, bh)), 1.5, color);
    painter.rect_filled(
        Rect::from_min_size(pos2(x0 + bw + gap, y0), vec2(bw, bh)),
        1.5,
        color,
    );
}

pub fn previous(painter: &Painter, rect: Rect, color: Color32) {
    let w = rect.width();
    let h = rect.height();
    let cy = rect.center().y;
    let bh = h * 0.52;
    painter.rect_filled(
        Rect::from_min_size(
            pos2(rect.left() + w * 0.20, cy - bh / 2.0),
            vec2(w * 0.10, bh),
        ),
        1.5,
        color,
    );
    let hh = h * 0.34;
    painter.add(PathShape::convex_polygon(
        vec![
            pos2(rect.right() - w * 0.18, cy - hh),
            pos2(rect.right() - w * 0.18, cy + hh),
            pos2(rect.left() + w * 0.36, cy),
        ],
        color,
        Stroke::NONE,
    ));
}

pub fn next(painter: &Painter, rect: Rect, color: Color32) {
    let w = rect.width();
    let h = rect.height();
    let cy = rect.center().y;
    let bh = h * 0.52;
    painter.rect_filled(
        Rect::from_min_size(
            pos2(rect.right() - w * 0.30, cy - bh / 2.0),
            vec2(w * 0.10, bh),
        ),
        1.5,
        color,
    );
    let hh = h * 0.34;
    painter.add(PathShape::convex_polygon(
        vec![
            pos2(rect.left() + w * 0.18, cy - hh),
            pos2(rect.left() + w * 0.18, cy + hh),
            pos2(rect.left() + w * 0.54, cy),
        ],
        color,
        Stroke::NONE,
    ));
}

/// Playback-mode loop arrows with an optional `1` / `∞` badge in the middle.
pub fn loop_icon(painter: &Painter, rect: Rect, color: Color32, badge: Option<&str>) {
    let w = rect.width();
    let h = rect.height();
    let l = rect.left() + w * 0.15;
    let r = rect.right() - w * 0.15;
    let ty = rect.top() + h * 0.30;
    let by = rect.bottom() - h * 0.30;
    let stroke = Stroke::new(1.7, color);
    painter.line_segment([pos2(l + 5.0, ty), pos2(r - 3.0, ty)], stroke);
    painter.line_segment([pos2(r - 5.0, by), pos2(l + 3.0, by)], stroke);
    let ah = vec2(4.5, 3.4);
    painter.add(PathShape::convex_polygon(
        vec![
            pos2(r, ty),
            pos2(r - ah.x, ty - ah.y),
            pos2(r - ah.x, ty + ah.y),
        ],
        color,
        Stroke::NONE,
    ));
    painter.add(PathShape::convex_polygon(
        vec![
            pos2(l, by),
            pos2(l + ah.x, by - ah.y),
            pos2(l + ah.x, by + ah.y),
        ],
        color,
        Stroke::NONE,
    ));
    if let Some(b) = badge {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            b,
            fonts::strong_font((h * 0.34).max(9.0)),
            color,
        );
    }
}

/// Queue list with a play triangle (was the SVG in the player bar).
pub fn queue_list(painter: &Painter, rect: Rect, color: Color32) {
    let w = rect.width();
    let h = rect.height();
    let l = rect.left() + w * 0.14;
    let line_r = rect.left() + w * 0.62;
    let cy = rect.center().y;
    let stroke = Stroke::new(1.7, color);
    let dy = h * 0.20;
    for y in [cy - dy, cy] {
        painter.line_segment([pos2(l, y), pos2(line_r, y)], stroke);
    }
    painter.line_segment([pos2(l, cy + dy), pos2(l + w * 0.30, cy + dy)], stroke);
    let ah = h * 0.26;
    painter.add(PathShape::convex_polygon(
        vec![
            pos2(rect.right() - w * 0.24, cy - ah),
            pos2(rect.right() - w * 0.24, cy + ah),
            pos2(rect.right() - w * 0.06, cy),
        ],
        color,
        Stroke::NONE,
    ));
}

/// Four corner brackets (⛶).
pub fn fullscreen(painter: &Painter, rect: Rect, color: Color32) {
    let rect = rect.shrink(3.0);
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let s = rect.width() * 0.34;
    let stroke = Stroke::new(1.8, color);
    painter.line_segment([pos2(l, t), pos2(l + s, t)], stroke);
    painter.line_segment([pos2(l, t), pos2(l, t + s)], stroke);
    painter.line_segment([pos2(r - s, t), pos2(r, t)], stroke);
    painter.line_segment([pos2(r, t), pos2(r, t + s)], stroke);
    painter.line_segment([pos2(l, b - s), pos2(l, b)], stroke);
    painter.line_segment([pos2(l, b), pos2(l + s, b)], stroke);
    painter.line_segment([pos2(r - s, b), pos2(r, b)], stroke);
    painter.line_segment([pos2(r, b - s), pos2(r, b)], stroke);
}

pub fn volume(painter: &Painter, rect: Rect, color: Color32) {
    let (l, t) = (rect.left(), rect.top());
    let (w, h) = (rect.width(), rect.height());
    let p = |x: f32, y: f32| pos2(l + w * x, t + h * y);
    painter.add(PathShape::convex_polygon(
        vec![
            p(0.16, 0.42),
            p(0.34, 0.42),
            p(0.52, 0.26),
            p(0.52, 0.74),
            p(0.34, 0.58),
            p(0.16, 0.58),
        ],
        color,
        Stroke::NONE,
    ));
    // one sound arc
    let center = p(0.52, 0.50);
    let radius = w * 0.24;
    let mut pts = Vec::with_capacity(9);
    for i in 0..=8 {
        let a = -1.05 + (2.1 * i as f32 / 8.0); // radians, y-down
        pts.push(pos2(
            center.x + radius * a.cos(),
            center.y + radius * a.sin(),
        ));
    }
    painter.add(PathShape::line(pts, Stroke::new(1.7, color)));
}

pub fn search(painter: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(3.0);
    let center = pos2(r.left() + r.width() * 0.40, r.top() + r.height() * 0.40);
    painter.circle_stroke(center, r.width() * 0.28, Stroke::new(1.8, color));
    painter.line_segment(
        [
            pos2(center.x + r.width() * 0.22, center.y + r.height() * 0.22),
            pos2(r.right(), r.bottom()),
        ],
        Stroke::new(2.0, color),
    );
}

pub fn close(painter: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(4.0);
    let stroke = Stroke::new(2.0, color);
    painter.line_segment([r.left_top(), r.right_bottom()], stroke);
    painter.line_segment([r.right_top(), r.left_bottom()], stroke);
}

/// Music note `♪` for the idle player-bar cover placeholder.
pub fn music_note(painter: &Painter, rect: Rect, color: Color32) {
    let (l, t) = (rect.left(), rect.top());
    let (w, h) = (rect.width(), rect.height());
    let head = pos2(l + w * 0.36, t + h * 0.70);
    let head_r = w * 0.15;
    painter.circle_filled(head, head_r, color);
    let stem_x = head.x + head_r - 1.0;
    let stroke = Stroke::new(2.2, color);
    painter.line_segment([pos2(stem_x, head.y), pos2(stem_x, t + h * 0.20)], stroke);
    painter.line_segment(
        [
            pos2(stem_x, t + h * 0.20),
            pos2(stem_x + w * 0.24, t + h * 0.33),
        ],
        stroke,
    );
}

// ---------------------------------------------------------------------------
// Gradients & tiles
// ---------------------------------------------------------------------------

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        mix(a.a(), b.a()),
    )
}

/// A rounded rect filled with a top-left → bottom-right gradient
/// (matches the CSS `linear-gradient(135deg, …)` fallback tiles).
///
/// Per-corner radii `[tl, tr, br, bl]` let the art area of a card sit flush
/// against the card body while staying rounded on top.
pub fn gradient_rounded_rect(
    painter: &Painter,
    rect: Rect,
    radii: [f32; 4],
    from: Color32,
    to: Color32,
) {
    const SEG: usize = 6;
    let (w, h) = (rect.width(), rect.height());
    if w <= 0.5 || h <= 0.5 {
        return;
    }
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let max_r = (w / 2.0).min(h / 2.0);
    let radii: [f32; 4] = radii.map(|x| x.min(max_r));

    // Corner arc centers, in ring order (TL, TR, BR, BL), clockwise on
    // screen. Angles are sampled in screen space (y grows downwards).
    let centers = [
        pos2(l + radii[0], t + radii[0]),
        pos2(r - radii[1], t + radii[1]),
        pos2(r - radii[2], b - radii[2]),
        pos2(l + radii[3], b - radii[3]),
    ];
    let arcs = [
        (std::f32::consts::PI, std::f32::consts::FRAC_PI_2),
        (
            std::f32::consts::FRAC_PI_2 * 3.0,
            std::f32::consts::FRAC_PI_2,
        ),
        (0.0, std::f32::consts::FRAC_PI_2),
        (std::f32::consts::FRAC_PI_2, std::f32::consts::PI),
    ];

    let mut ring: Vec<(Pos2, Color32)> = Vec::with_capacity(4 * (SEG + 1));
    let diag = |p: Pos2| (((p.x - l) / w) + ((p.y - t) / h)) / 2.0;
    for (i, center) in centers.iter().enumerate() {
        let radius = radii[i];
        if radius < 0.5 {
            // Degenerate corner: push the two tangent points so the ring
            // still traces the rectangle.
            let corners = [pos2(l, t), pos2(r, t), pos2(r, b), pos2(l, b)];
            ring.push((corners[i], lerp(from, to, diag(corners[i]))));
            continue;
        }
        let (a0, sweep) = arcs[i];
        for s in 0..=SEG {
            let a = a0 + sweep * s as f32 / SEG as f32;
            let p = pos2(center.x + radius * a.cos(), center.y + radius * a.sin());
            ring.push((p, lerp(from, to, diag(p))));
        }
    }

    let center_pos = rect.center();
    let mut mesh = Mesh::default();
    mesh.vertices.push(Vertex {
        pos: center_pos,
        uv: pos2(0.5, 0.5),
        color: lerp(from, to, diag(center_pos)),
    });
    for (p, color) in &ring {
        mesh.vertices.push(Vertex {
            pos: *p,
            uv: pos2(0.0, 0.0),
            color: *color,
        });
    }
    let n = ring.len() as u32;
    for i in 0..n {
        let a = 1 + i;
        let bidx = 1 + ((i + 1) % n);
        mesh.indices.extend_from_slice(&[0, a, bidx]);
    }
    painter.add(mesh);
}

/// Gradient fallback tile with a big initial (album/playlist art placeholder).
pub fn letter_tile(painter: &Painter, rect: Rect, radii: [f32; 4], letter: char) {
    gradient_rounded_rect(painter, rect, radii, c::ACCENT, c::ACCENT2);
    let size = (rect.height() * 0.40).clamp(8.0, 400.0);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        letter.to_ascii_uppercase().to_string(),
        fonts::strong_font(size),
        c::WHITE,
    );
}
