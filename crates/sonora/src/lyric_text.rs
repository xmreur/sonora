//! Supersampled lyric rasterizer.
//!
//! egui's font atlas stores one coverage sample per device pixel, and the
//! dark-mode coverage curve sharpens that fringe into a hard step. Lyrics
//! are large and sit on a flat field, so those steps read as stair-steps.
//! Each run is outlined at [`SUPERSAMPLE`]× device resolution, box-filtered
//! back down, and drawn as a linear texture. One white coverage texture is
//! cached per glyph; color is a tint and the karaoke fill is a clip, so a
//! sweep does not allocate a new texture per step.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ab_glyph::{point, Font, FontVec, OutlinedGlyph, PxScale, ScaleFont};
use egui::epaint::{Color32, ColorImage};
use egui::{Painter, Pos2, Rect, TextureHandle, TextureOptions, Vec2};

/// Outline resolution relative to device pixels. 4× box-filter is the usual
/// "smooth but still a glyph" setting: each on-screen pixel averages 16
/// coverage samples.
const SUPERSAMPLE: u32 = 4;

struct FontSlot {
    font: Option<FontVec>,
}

static FONT: OnceLock<FontSlot> = OnceLock::new();

fn font() -> Option<&'static FontVec> {
    FONT.get_or_init(|| FontSlot {
        font: crate::fonts::lyric_bytes().and_then(|bytes| {
            let bytes = bytes.to_vec();
            FontVec::try_from_vec(bytes).ok()
        }),
    })
    .font
    .as_ref()
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    text: String,
    pt_q: u32,
    ppp_q: u32,
}

#[derive(Clone)]
struct Entry {
    tex: TextureHandle,
    tex_size: Vec2,
    offset: Vec2,
}

struct Slot {
    entry: Entry,
    stamp: u64,
}

struct LyricCache {
    map: HashMap<Key, Slot>,
    clock: u64,
}

/// Glyphs on screen at once, plus a few lines of scrollback. A karaoke
/// step used to be its own texture; the cap is on distinct text now.
const CACHE_CAP: usize = 64;

static CACHE: std::sync::LazyLock<Mutex<LyricCache>> = std::sync::LazyLock::new(|| {
    Mutex::new(LyricCache {
        map: HashMap::new(),
        clock: 0,
    })
});

fn cache() -> std::sync::MutexGuard<'static, LyricCache> {
    CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Layout size of `text` at `pt` points, matching [`paint`].
pub fn measure(painter: &Painter, text: &str, pt: f32) -> Vec2 {
    let ppp = painter.pixels_per_point().max(1.0);
    if text.is_empty() {
        return Vec2::new(0.0, pt);
    }
    if let Some(size) = layout_size(text, pt, ppp) {
        return size;
    }
    painter
        .layout_no_wrap(
            text.to_owned(),
            crate::fonts::lyric_font(pt),
            Color32::WHITE,
        )
        .size()
}

/// A glyph rasterized once at its slot size. Draw it scaled and tinted;
/// the size and color of a line can then ease without rasterizing again.
pub struct Glyph {
    tex: TextureHandle,
    tex_size: Vec2,
    offset: Vec2,
}

impl Glyph {
    pub fn draw(&self, painter: &Painter, pos: Pos2, scale: f32, tint: Color32) {
        painter.image(
            self.tex.id(),
            Rect::from_min_size(pos + self.offset * scale, self.tex_size * scale),
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            tint,
        );
    }

    /// Sung fraction of this glyph. The same coverage texture, clipped,
    /// with a few pixels of dimmer coverage past the cut so the edge is
    /// not a one-pixel scissor.
    pub fn draw_sung(&self, painter: &Painter, pos: Pos2, scale: f32, amount: f32, tint: Color32) {
        let amount = amount.clamp(0.0, 1.0);
        if amount <= 0.0 {
            return;
        }
        let origin = pos + self.offset * scale;
        let size = self.tex_size * scale;
        let width = size.x * amount;
        let feather = (size.y * 0.12).clamp(2.0, 8.0);
        let soft_w = (width + feather).min(size.x);
        if soft_w > width {
            let clip = Rect::from_min_size(origin, Vec2::new(soft_w, size.y.max(1.0)));
            self.draw(
                &painter.with_clip_rect(clip),
                pos,
                scale,
                tint.gamma_multiply(0.35),
            );
        }
        let clip = Rect::from_min_size(origin, Vec2::new(width, size.y.max(1.0)));
        self.draw(&painter.with_clip_rect(clip), pos, scale, tint);
    }
}

/// White coverage for `text` at `pt` points. Tint it when drawing.
pub fn coverage(painter: &Painter, text: &str, pt: f32) -> Option<Glyph> {
    cached(painter, text, pt)
}

fn cached(painter: &Painter, text: &str, pt: f32) -> Option<Glyph> {
    if text.is_empty() || pt <= 0.0 {
        return None;
    }
    let ppp = painter.pixels_per_point().max(1.0);
    let key = Key {
        text: text.to_owned(),
        pt_q: (pt * 10.0).round() as u32,
        ppp_q: (ppp * 100.0).round() as u32,
    };
    {
        let mut guard = cache();
        guard.clock = guard.clock.wrapping_add(1);
        let stamp = guard.clock;
        let hit = guard.map.get_mut(&key).map(|slot| {
            slot.stamp = stamp;
            glyph_of(slot.entry.clone())
        });
        if let Some(glyph) = hit {
            return Some(glyph);
        }
    }
    let run = raster(text, pt, ppp, Color32::WHITE, None)?;
    let tex = painter
        .ctx()
        .load_texture("lyric", run.image, TextureOptions::LINEAR);
    let entry = Entry {
        tex,
        tex_size: run.tex_size,
        offset: run.offset,
    };
    let mut guard = cache();
    guard.clock += 1;
    let stamp = guard.clock;
    if guard.map.len() >= CACHE_CAP {
        if let Some(old) = guard
            .map
            .iter()
            .min_by_key(|(_, slot)| slot.stamp)
            .map(|(k, _)| k.clone())
        {
            guard.map.remove(&old);
        }
    }
    guard.map.insert(
        key,
        Slot {
            entry: entry.clone(),
            stamp,
        },
    );
    Some(glyph_of(entry))
}

fn glyph_of(entry: Entry) -> Glyph {
    Glyph {
        tex: entry.tex,
        tex_size: entry.tex_size,
        offset: entry.offset,
    }
}

pub struct Raster {
    pub image: ColorImage,
    pub tex_size: Vec2,
    pub offset: Vec2,
}

/// Raster `text` at `pt` points for a context whose `pixels_per_point` is
/// `ppp`. `fill` paints that fraction of the advance in `fill.1` and the
/// remainder in `base`.
fn layout_size(text: &str, pt: f32, ppp: f32) -> Option<Vec2> {
    let font = font()?;
    if text.is_empty() || pt <= 0.0 {
        return None;
    }
    let ppp = ppp.max(1.0);
    let scale = em_scale(font, pt * ppp * SUPERSAMPLE as f32);
    let (advance, line_h) = line_metrics(font, text, scale);
    let unit = ppp * SUPERSAMPLE as f32;
    Some(Vec2::new(advance / unit, line_h / unit))
}

/// `PxScale` is the ascender-to-descender box, not the em. egui sizes are
/// em sizes, so a raw `PxScale::from(pt)` draws a smaller glyph than the
/// same point size in the rest of the UI.
fn em_scale(font: &FontVec, px_per_em: f32) -> PxScale {
    let upem = font.units_per_em().unwrap_or(1000.0).max(1.0);
    let height = font.height_unscaled().max(1.0);
    PxScale::from(px_per_em * height / upem)
}

fn line_metrics(font: &FontVec, text: &str, scale: PxScale) -> (f32, f32) {
    let scaled = font.as_scaled(scale);
    let mut pen = 0.0f32;
    let mut prev = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(prev) = prev {
            pen += scaled.kern(prev, id);
        }
        pen += scaled.h_advance(id);
        prev = Some(id);
    }
    (pen.max(1.0), scaled.height().max(1.0))
}

pub fn raster(
    text: &str,
    pt: f32,
    ppp: f32,
    base: Color32,
    fill: Option<(f32, Color32)>,
) -> Option<Raster> {
    let font = font()?;
    if text.is_empty() || pt <= 0.0 {
        return None;
    }
    let ppp = ppp.max(1.0);
    let scale = em_scale(font, pt * ppp * SUPERSAMPLE as f32);
    let scaled = font.as_scaled(scale);
    let (advance, line_h) = line_metrics(font, text, scale);
    let mut pen = 0.0f32;
    let mut prev = None;
    let mut outlined: Vec<OutlinedGlyph> = Vec::new();
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(prev) = prev {
            pen += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, point(pen, scaled.ascent()));
        if let Some(outline) = font.outline_glyph(glyph) {
            outlined.push(outline);
        }
        pen += scaled.h_advance(id);
        prev = Some(id);
    }

    let mut min_x = 0.0f32;
    let mut min_y = 0.0f32;
    let mut max_x = advance;
    let mut max_y = line_h;
    for outline in &outlined {
        let bounds = outline.px_bounds();
        min_x = min_x.min(bounds.min.x);
        min_y = min_y.min(bounds.min.y);
        max_x = max_x.max(bounds.max.x);
        max_y = max_y.max(bounds.max.y);
    }
    let pad = SUPERSAMPLE as f32;
    let origin_x = -min_x.floor() + pad;
    let origin_y = -min_y.floor() + pad;
    let hi_w = ((max_x - min_x).ceil() as u32)
        .saturating_add(pad as u32 * 2)
        .saturating_add(SUPERSAMPLE)
        .next_multiple_of(SUPERSAMPLE)
        .max(SUPERSAMPLE);
    let hi_h = ((max_y - min_y).ceil() as u32)
        .saturating_add(pad as u32 * 2)
        .saturating_add(SUPERSAMPLE)
        .next_multiple_of(SUPERSAMPLE)
        .max(SUPERSAMPLE);
    let mut coverage = vec![0.0f32; (hi_w as usize) * (hi_h as usize)];
    for outline in &outlined {
        let bounds = outline.px_bounds();
        let ox = (bounds.min.x + origin_x).floor() as i32;
        let oy = (bounds.min.y + origin_y).floor() as i32;
        outline.draw(|x, y, c| {
            let px = ox + x as i32;
            let py = oy + y as i32;
            if px >= 0 && py >= 0 && (px as u32) < hi_w && (py as u32) < hi_h {
                let i = (py as u32 * hi_w + px as u32) as usize;
                if c > coverage[i] {
                    coverage[i] = c;
                }
            }
        });
    }

    let out_w = hi_w / SUPERSAMPLE;
    let out_h = hi_h / SUPERSAMPLE;
    let mut pixels = Vec::with_capacity((out_w as usize) * (out_h as usize) * 4);
    let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;
    let edge = fill.map(|(f, _)| f.clamp(0.0, 1.0));
    let fill_color = fill.map(|(_, c)| c).unwrap_or(base);
    // One em in the high-res buffer. The sweep fades across a fraction of
    // that, mostly into the unsung side, instead of a one-pixel cut.
    let em = pt * ppp * SUPERSAMPLE as f32;
    for oy in 0..out_h {
        for ox in 0..out_w {
            let mut cov_sum = 0.0f32;
            let mut acc = [0.0f32; 4];
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let hx = ox * SUPERSAMPLE + sx;
                    let hy = oy * SUPERSAMPLE + sy;
                    let cov = coverage[(hy * hi_w + hx) as usize];
                    if cov <= 0.0 {
                        continue;
                    }
                    let sample_x = hx as f32 + 0.5;
                    let along = sample_x - origin_x;
                    let color = match edge {
                        Some(edge) => {
                            let t = sweep_mix(along, edge * advance, em);
                            lerp_straight(fill_color, base, t)
                        }
                        None => straight_rgba(base),
                    };
                    cov_sum += cov;
                    acc[0] += cov * color[0];
                    acc[1] += cov * color[1];
                    acc[2] += cov * color[2];
                    acc[3] += cov * color[3];
                }
            }
            if cov_sum <= 0.0 {
                pixels.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let r = (acc[0] / cov_sum).round() as u8;
                let g = (acc[1] / cov_sum).round() as u8;
                let b = (acc[2] / cov_sum).round() as u8;
                // Keep the color's own alpha (dimmed lyric colors are not
                // opaque) and multiply by the glyph coverage.
                let alpha = ((cov_sum / samples) * (acc[3] / cov_sum) / 255.0).clamp(0.0, 1.0);
                pixels.extend_from_slice(&[r, g, b, (alpha * 255.0).round() as u8]);
            }
        }
    }
    let unit = ppp * SUPERSAMPLE as f32;
    Some(Raster {
        image: ColorImage::from_rgba_unmultiplied([out_w as usize, out_h as usize], &pixels),
        tex_size: Vec2::new(hi_w as f32 / unit, hi_h as f32 / unit),
        offset: Vec2::new(-origin_x / unit, -origin_y / unit),
    })
}

/// 0 = sung color, 1 = idle color.
///
/// The playhead is a short blur, then a weaker wash that brightens the
/// unsung letters just past it and dies out within an em.
fn sweep_mix(along: f32, edge_px: f32, em: f32) -> f32 {
    let d = (along - edge_px) / em.max(1.0);
    let core = smoothstep(-0.2, 0.35, d);
    let wash = (1.0 - smoothstep(0.15, 0.9, d)) * 0.18;
    (core - wash).clamp(0.0, 1.0)
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Straight sRGBA. `Color32::r()` is premultiplied, and storing that as
/// straight RGB paints the unsung half of a word darker than a tinted glyph.
fn straight_rgba(color: Color32) -> [f32; 4] {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    [f32::from(r), f32::from(g), f32::from(b), f32::from(a)]
}

fn lerp_straight(a: Color32, b: Color32, t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    let a = straight_rgba(a);
    let b = straight_rgba(b);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_fill_is_brighter_on_the_left() {
        crate::fonts::install(&egui::Context::default());
        let run = raster(
            "Than say, \"I do, I do, I do\"?",
            42.0,
            1.0,
            Color32::from_gray(150),
            Some((0.45, Color32::WHITE)),
        )
        .expect("a lyric font is installed");
        let w = run.image.size[0];
        let h = run.image.size[1];
        let mut edge = 0u32;
        let mut solid = 0u32;
        for y in 0..h {
            for x in 0..w {
                let a = run.image.pixels[y * w + x].a();
                if a == 0 {
                    continue;
                }
                if a == 255 {
                    solid += 1;
                } else {
                    edge += 1;
                }
            }
        }
        assert!(
            edge > 0 && edge * 30 > solid,
            "glyph edges need a real coverage fringe, got {edge} partial vs {solid} solid"
        );

        // Compare the left and right thirds of the ink, not the bitmap
        // midpoint: padding around the glyphs can put the playhead off center,
        // and a stem edge can drop more than the sweep does.
        let cols: Vec<Option<f32>> = (0..w)
            .map(|x| {
                let mut lum = 0.0f32;
                let mut n = 0u32;
                for y in 0..h {
                    let p = run.image.pixels[y * w + x];
                    if p.a() > 40 {
                        let [r, g, b, _] = p.to_srgba_unmultiplied();
                        lum += f32::from(r) + f32::from(g) + f32::from(b);
                        n += 1;
                    }
                }
                (n > 0).then_some(lum / n as f32)
            })
            .collect();
        let ink: Vec<f32> = cols.into_iter().flatten().collect();
        assert!(ink.len() > 6, "expected a run of ink columns");
        let third = ink.len() / 3;
        let left_ink = ink[..third].iter().sum::<f32>() / third as f32;
        let right_ink = ink[ink.len() - third..].iter().sum::<f32>() / third as f32;
        assert!(
            left_ink > right_ink,
            "the sung third ({left_ink}) should be brighter than the unsung third ({right_ink})"
        );
    }

    #[test]
    fn point_size_is_the_em_not_the_line_box() {
        crate::fonts::install(&egui::Context::default());
        let pt = 48.0;
        let run = raster("H", pt, 1.0, Color32::WHITE, None).expect("lyric font");
        let w = run.image.size[0];
        let h = run.image.size[1];
        let ink = (0..h)
            .filter(|y| (0..w).any(|x| run.image.pixels[y * w + x].a() > 20))
            .count();
        assert!(
            ink as f32 > pt * 0.6,
            "cap height {ink}px is too small for a {pt}pt em"
        );
    }
}
