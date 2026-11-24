//! Fullscreen + immersive overlays (old `#fsOverlay` / `#imOverlay`).
//!
//! Classic fullscreen is a cover + title + lyrics + transport over an
//! ambient radial glow sampled from the cover. Immersive swaps that glow
//! for a lava-lamp background driven by the cover palette: a base gradient
//! through six extracted colors, optional wandering blobs, a still
//! blurred cover, or the Kawarp dynamic warp (`Dynamic`) drawn through an
//! `egui_glow` paint callback over the gradient. Both share the transport
//! bar and the karaoke lyrics engine.
//!
//! Motion covers are the remaining later phase (P3d): `Motion` still
//! renders its `static` degraded look, matching the old app's own
//! fallback path.

use std::sync::Arc;
use std::time::Instant;

use egui::{
    pos2, vec2, Color32, ColorImage, CornerRadius, Pos2, Rect, RichText, Sense, Shape, Stroke,
    TextureHandle, UiBuilder,
};

use crate::app::{FsLayout, ImBg, ImCover, ImLayout, SonoraApp};
use crate::artwork::CoverPalette;
use crate::kawarp;
use crate::lyrics::LyricState;
use crate::motion::Slot;
use crate::theme::c;
use crate::{fonts, icons, widgets};

/// Palette morph duration (old `IM_PAL_TWEEN_MS`): colors ease in over
/// ~1.2s instead of snapping on a track change.
const PAL_TWEEN_MS: f32 = 1200.0;

/// A palette tween: (from-colors, to-colors, start instant).
type PalTween = ([[f32; 3]; 6], [[f32; 3]; 6], Instant);

/// Default immersive palette (old `--im1..6`), used before a cover's
/// extraction lands.
const DEFAULT_PALETTE: [[f32; 3]; 6] = [
    [58.0, 22.0, 32.0],
    [20.0, 20.0, 25.0],
    [36.0, 20.0, 40.0],
    [20.0, 32.0, 58.0],
    [58.0, 42.0, 20.0],
    [20.0, 58.0, 42.0],
];

/// Default classic-fullscreen ambient glow (old `--ambient`).
const DEFAULT_AMBIENT: Color32 = Color32::from_rgb(0x2a, 0x0f, 0x16);

/// One lava-lamp blob (old `flowBlobs` entry): steers toward a random
/// target at its own eased speed, with scale/opacity drifting on random
/// phases. `x`/`y` are offsets from a seeded base position.
struct Blob {
    base: Pos2,
    size: f32,
    x: f32,
    y: f32,
    tx: f32,
    ty: f32,
    s: f32,
    ts: f32,
    o: f32,
    to: f32,
    speed: f32,
    w: f32,
    phase: f32,
    color: usize,
}

/// Egui-side state for the fullscreen/immersive overlays: the cover
/// palette (tweened across track changes), the ambient glow color, and the
/// flow blobs. Seeded lazily from the decoded cover; degrades to the
/// built-in defaults until a palette is available.
pub struct ImmersiveUi {
    /// Currently rendered palette (tween source of truth).
    render: [[f32; 3]; 6],
    /// In-flight tween: (from, to, start).
    tween: Option<PalTween>,
    /// Classic-fullscreen ambient glow (deepened cover average).
    ambient: Color32,
    /// Pale-cover flag (brighter lyrics + stronger scrim).
    light: bool,
    /// Artwork template the current palette/ambient came from.
    last_art: Option<String>,
    /// Flow blobs + the track they were seeded for.
    blobs: Vec<Blob>,
    flow_track: Option<String>,
    flow_last_t: Option<Instant>,
    /// Crossfade weight of the Motion layer over the still cover in
    /// dynamic-blended mode (0 = still only, 1 = video).
    blend_mix: f32,
    /// Last video frame, kept so the layer can fade out when Motion artwork
    /// disappears instead of vanishing on the next frame.
    blend_last: Option<Arc<ColorImage>>,
    /// Previous track's frame, held while the next one animates in.
    blend_prev: Option<Arc<ColorImage>>,
    /// Feathered frame uploads, rebuilt only when their frame/geometry moves.
    mask_cur: MaskCache,
    mask_prev: MaskCache,
    mask_still: MaskCache,
    blend_still: Option<Arc<ColorImage>>,
    /// 0 → previous frame, 1 → current frame.
    blend_swap: f32,
    /// EMA of fullscreen render time and of the real frame rate, shown with
    /// SONORA_PERF=1. Comparing them separates CPU cost from GPU/present cost.
    perf_ms: f32,
    fps: f32,
    pub(crate) motion_pull_ms: f32,
    pub(crate) motion_copy_ms: f32,
    pub(crate) motion_upload_ms: f32,
    /// EMA of the CPU time spent building feathered frame images.
    mask_ms: f32,
    blend_track: Option<String>,
}

impl Default for ImmersiveUi {
    fn default() -> Self {
        Self {
            render: DEFAULT_PALETTE,
            tween: None,
            ambient: DEFAULT_AMBIENT,
            light: false,
            last_art: None,
            blobs: Vec::new(),
            flow_track: None,
            flow_last_t: None,
            blend_mix: 0.0,
            blend_last: None,
            blend_prev: None,
            mask_cur: MaskCache::new("blend-mask-cur"),
            mask_prev: MaskCache::new("blend-mask-prev"),
            mask_still: MaskCache::new("blend-mask-still"),
            blend_still: None,
            blend_swap: 1.0,
            perf_ms: 0.0,
            fps: 0.0,
            motion_pull_ms: 0.0,
            motion_copy_ms: 0.0,
            motion_upload_ms: 0.0,
            mask_ms: 0.0,
            blend_track: None,
        }
    }
}

impl ImmersiveUi {
    /// Adopt a newly-extracted palette for `art` (old
    /// `updateImmersivePalette` + `updateAmbient`): start a 1.2s tween from
    /// the currently-rendered colors, and set the ambient glow + light-bg
    /// flag immediately (readability first). No-op until `pal` is available
    /// or if this art was already applied.
    pub fn note_art(&mut self, art: Option<&str>, pal: Option<&CoverPalette>) {
        let (Some(art), Some(pal)) = (art, pal) else {
            return;
        };
        if pal.colors.is_empty() || self.last_art.as_deref() == Some(art) {
            return;
        }
        self.last_art = Some(art.to_owned());

        // Pad to six slots by cycling (old `writeImPaletteVars`).
        let mut to = [[0.0f32; 3]; 6];
        for (i, slot) in to.iter_mut().enumerate() {
            *slot = pal.colors[i % pal.colors.len()];
        }
        self.tween = Some((self.render, to, Instant::now()));
        self.ambient = pal.ambient();
        self.light = pal.light;
    }

    /// Step the palette tween (old `stepImPaletteTween`) — easeInOutQuad
    /// over [`PAL_TWEEN_MS`]. True while still animating.
    pub fn tick(&mut self) -> bool {
        let Some((from, to, t0)) = self.tween else {
            return false;
        };
        // `elapsed` is seconds; PAL_TWEEN_MS is milliseconds.
        let k = (t0.elapsed().as_secs_f32() * 1000.0 / PAL_TWEEN_MS).min(1.0);
        let e = ease_in_out_quad(k);
        for (i, slot) in self.render.iter_mut().enumerate() {
            let t = to[i];
            for ch in 0..3 {
                slot[ch] = from[i][ch] + (t[ch] - from[i][ch]) * e;
            }
        }
        if k >= 1.0 {
            self.render = to;
            self.tween = None;
            false
        } else {
            true
        }
    }

    fn palette_color(&self, i: usize) -> Color32 {
        let col = self.render[i % self.render.len()];
        Color32::from_rgb(col[0] as u8, col[1] as u8, col[2] as u8)
    }

    /// Effective scrim strength (old `applyImmersiveLightBgClass`): a pale
    /// cover boosts the dim for readability, but only when a palette wash
    /// is actually painted (not in dynamic mode).
    fn dim(&self, base: f32, dynamic: bool) -> f32 {
        if self.light && !dynamic {
            (base + 0.22).min(0.92)
        } else {
            base
        }
    }

    /// (Re)seed the flow blobs for a track (old `flowSeed`): 8–12 blobs at
    /// random positions/sizes/colors, dealt round-robin from a shuffled
    /// palette so every color appears evenly. Deterministic per track id.
    fn seed_flow(&mut self, track: Option<&str>, rect: Rect) {
        let mut rng = Rng::for_track(track);

        let mut ord = [1usize, 2, 3, 4, 5, 6];
        for i in (1..6).rev() {
            let j = (rng.next() * (i + 1) as f32) as usize;
            ord.swap(i, j.min(i));
        }

        let vmax = rect.width().max(rect.height());
        let n = 8 + (rng.next() * 5.0) as usize;
        self.blobs.clear();
        for i in 0..n {
            let size = (30.0 + rng.next() * 35.0) / 100.0 * vmax;
            let mut b = Blob {
                base: pos2(rng.next() * rect.width(), rng.next() * rect.height()),
                size,
                x: 0.0,
                y: 0.0,
                tx: 0.0,
                ty: 0.0,
                s: 0.8 + rng.next() * 0.5,
                ts: 1.0,
                o: 0.85,
                to: 0.85,
                speed: 60.0,
                w: (2.0 * std::f32::consts::PI) / (6.0 + rng.next() * 7.0),
                phase: rng.next() * std::f32::consts::TAU,
                color: ord[i % 6],
            };
            retarget(&mut b, rect, &mut rng);
            self.blobs.push(b);
        }
        self.flow_track = track.map(str::to_owned);
        self.flow_last_t = None;
    }

    /// Advance the blob wander (old `tickFlow`). True while animating.
    pub fn tick_flow(&mut self, track: Option<&str>, rect: Rect) -> bool {
        if self.flow_track != track.map(str::to_owned) || self.blobs.is_empty() {
            self.seed_flow(track, rect);
        }
        let now = Instant::now();
        let dt = match self.flow_last_t {
            Some(t) => (now - t).as_secs_f32().clamp(0.001, 0.1),
            None => 0.016,
        };
        self.flow_last_t = Some(now);
        let t_s = now.elapsed().as_secs_f32();
        let mut rng = Rng::for_track(track);
        rng.advance(self.blobs.len() as u32 * 7);

        for b in &mut self.blobs {
            let dx = b.tx - b.x;
            let dy = b.ty - b.y;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist < 6.0 {
                retarget(b, rect, &mut rng);
            } else {
                let step = dist.min(b.speed * dt);
                b.x += dx / dist * step;
                b.y += dy / dist * step;
            }
            // Ease scale/opacity toward their own targets.
            let ease = (dt * 0.8).min(1.0);
            b.s += (b.ts - b.s) * ease;
            b.o += (b.to - b.o) * ease;
            if (b.ts - b.s).abs() < 0.02 && (b.to - b.o).abs() < 0.02 {
                retarget(b, rect, &mut rng);
            }
            // Sine wobble on the scale (organic, never repeats).
            b.s *= 1.0 + 0.06 * (t_s * b.w + b.phase).sin();
        }
        true
    }

    fn blobs(&self) -> &[Blob] {
        &self.blobs
    }
}

/// Tiny xorshift PRNG (no external dep) seeded from a track id, so each
/// track gets a stable, varied composition.
struct Rng(u32);

impl Rng {
    fn for_track(track: Option<&str>) -> Self {
        let seed = track
            .map(|t| {
                t.bytes()
                    .fold(0x9e37_79b9u32, |a, b| a ^ b as u32)
                    .wrapping_mul(0x85eb_ca6b)
            })
            .unwrap_or(0x1234_5678)
            | 1;
        Self(seed)
    }

    fn advance(&mut self, n: u32) {
        for _ in 0..n {
            self.next();
        }
    }

    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// Pick a new wander target for a blob (old `flowRetarget`).
fn retarget(b: &mut Blob, rect: Rect, rng: &mut Rng) {
    b.tx = (rng.next() * 2.0 - 1.0) * rect.width() * 0.13;
    b.ty = (rng.next() * 2.0 - 1.0) * rect.height() * 0.11;
    b.ts = 0.7 + rng.next() * 0.7;
    b.to = 0.6 + rng.next() * 0.4;
    b.speed = 40.0 + rng.next() * 50.0;
}

/// easeInOutQuad over a 0..1 linear t (old tween easing).
fn ease_in_out_quad(k: f32) -> f32 {
    let k = k.clamp(0.0, 1.0);
    if k < 0.5 {
        2.0 * k * k
    } else {
        1.0 - (-2.0 * k + 2.0).powi(2) / 2.0
    }
}

// ---------------------------------------------------------------------------
// Painting helpers
// ---------------------------------------------------------------------------

/// Sample a multi-stop ramp at `t` (0..=1), lerping in gamma space.
fn sample_ramp(stops: &[(f32, Color32)], t: f32) -> Color32 {
    match stops {
        [] => Color32::TRANSPARENT,
        [only] => only.1,
        _ => {
            let t = t.clamp(0.0, 1.0);
            for w in stops.windows(2) {
                let (t0, c0) = w[0];
                let (t1, c1) = w[1];
                if t <= t1 {
                    let span = (t1 - t0).max(1e-4);
                    return c0.lerp_to_gamma(c1, ((t - t0) / span).clamp(0.0, 1.0));
                }
            }
            stops[stops.len() - 1].1
        }
    }
}

/// Exact linear gradient across `rect` at `angle` radians through `stops`.
/// A linear color field interpolates perfectly across two triangles, so a
/// 4-corner quad with per-corner ramp colors renders seamlessly.
fn linear_gradient(painter: &egui::Painter, rect: Rect, angle: f32, stops: &[(f32, Color32)]) {
    let axis = vec2(angle.cos(), angle.sin());
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    let proj: Vec<f32> = corners
        .iter()
        .map(|p| p.x * axis.x + p.y * axis.y)
        .collect();
    let mn = proj.iter().cloned().fold(f32::INFINITY, f32::min);
    let mx = proj.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let span = (mx - mn).max(1e-4);

    let mut mesh = egui::epaint::Mesh::default();
    for (i, p) in corners.iter().enumerate() {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: *p,
            uv: Pos2::ZERO,
            color: sample_ramp(stops, (proj[i] - mn) / span),
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(Shape::Mesh(std::sync::Arc::new(mesh)));
}

/// Soft radial blob from concentric circles (epaint has no feather): drawn
/// outermost-transparent → innermost-opaque so the edge fades out.
fn soft_blob(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, opacity: f32) {
    const STEPS: usize = 14;
    for k in 0..STEPS {
        let f = k as f32 / (STEPS - 1) as f32; // 0 outer → 1 inner
        let a = (f * f * opacity).clamp(0.0, 1.0);
        if a <= 0.004 {
            continue;
        }
        painter.circle_filled(center, radius * (1.0 - f), color.gamma_multiply(a));
    }
}

/// Stretch a small cover texture across `rect`. `blur` (the settings
/// slider, 0..=40) stacks offset copies so the 48px still is softer than
/// a single bilinear stretch.
fn paint_cover_fill(painter: &egui::Painter, rect: Rect, tex: &TextureHandle, blur: f32) {
    let uv = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
    if blur < 0.5 {
        painter.image(tex.id(), rect, uv, Color32::WHITE);
        return;
    }
    let radius = blur.clamp(0.0, 40.0);
    const STEPS: usize = 8;
    let alpha = 1.0 / (STEPS as f32 + 1.0);
    painter.image(tex.id(), rect, uv, Color32::WHITE.gamma_multiply(alpha));
    for i in 0..STEPS {
        let a = i as f32 / STEPS as f32 * std::f32::consts::TAU;
        let offset = vec2(a.cos(), a.sin()) * radius;
        painter.image(
            tex.id(),
            rect.translate(offset),
            uv,
            Color32::WHITE.gamma_multiply(alpha),
        );
    }
}

/// `object-fit: cover` for a sharp texture: UVs are cropped so the image
/// fills `rect` without distortion (old `#imBgVideo` over the full bleed).
fn paint_cover_crop(painter: &egui::Painter, rect: Rect, tex: &TextureHandle, blur: f32) {
    let [tw, th] = tex.size();
    if tw == 0 || th == 0 || rect.height() <= 0.0 || rect.width() <= 0.0 {
        return;
    }
    let ta = tw as f32 / th as f32;
    let ra = rect.width() / rect.height();
    let uv = if ta > ra {
        // Wider than the rect: crop the sides.
        let w = ra / ta;
        Rect::from_x_y_ranges((0.5 - w * 0.5)..=(0.5 + w * 0.5), 0.0..=1.0)
    } else {
        // Taller: crop top/bottom.
        let h = ta / ra;
        Rect::from_x_y_ranges(0.0..=1.0, (0.5 - h * 0.5)..=(0.5 + h * 0.5))
    };
    let draw = |rect: Rect, tint: Color32| painter.image(tex.id(), rect, uv, tint);
    if blur < 0.5 {
        draw(rect, Color32::WHITE);
        return;
    }
    let radius = blur.clamp(0.0, 40.0);
    const STEPS: usize = 8;
    let alpha = 1.0 / (STEPS as f32 + 1.0);
    draw(rect, Color32::WHITE.gamma_multiply(alpha));
    for i in 0..STEPS {
        let a = i as f32 / STEPS as f32 * std::f32::consts::TAU;
        let offset = vec2(a.cos(), a.sin()) * radius;
        draw(rect.translate(offset), Color32::WHITE.gamma_multiply(alpha));
    }
}

/// Geometry for the "dynamic blended" fullscreen mode: a full-height video
/// stage pinned to the left, an eased dissolve on its right edge, a lyrics
/// column on the right and a small metadata row at the bottom-left.
struct BlendLayout {
    stage: Rect,
    feather_w: f32,
    lyrics: Rect,
    meta: Rect,
    bar: Rect,
}

fn blend_layout(full: Rect) -> BlendLayout {
    let pad = (full.width() * 0.04).clamp(24.0, 64.0);
    let stage_w = full.width() * 0.54;
    let stage = Rect::from_min_max(full.left_top(), pos2(full.left() + stage_w, full.bottom()));
    let feather_w = stage_w * 0.45;
    let lyrics = Rect::from_x_y_ranges(
        (stage.right() - feather_w * 0.1)..=(full.right() - pad),
        (full.top() + 80.0)..=(full.bottom() - 150.0),
    );
    let meta = Rect::from_min_size(
        pos2(full.left() + pad, full.bottom() - 190.0),
        vec2(360.0, 96.0),
    );
    let bar = Rect::from_min_size(
        pos2(full.left() + pad, full.bottom() - 84.0),
        vec2((full.width() * 0.42).clamp(360.0, 620.0), 60.0),
    );
    BlendLayout {
        stage,
        feather_w,
        lyrics,
        meta,
        bar,
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    Rect::from_min_max(
        pos2(lerp(a.left(), b.left(), t), lerp(a.top(), b.top(), t)),
        pos2(
            lerp(a.right(), b.right(), t),
            lerp(a.bottom(), b.bottom(), t),
        ),
    )
}

struct ImmersiveRects {
    cover: Rect,
    meta: Rect,
    lyrics: Rect,
    bar: Rect,
}

/// Exact-enough rects of the regular immersive layout. The blended mode
/// animates from these to `blend_layout` so the cover and text actually move
/// between the two layouts instead of two layouts fading over each other.
fn immersive_rects(
    full: Rect,
    split: bool,
    vertical: bool,
    show_lyrics: bool,
    cover_shown: bool,
) -> ImmersiveRects {
    let pad = 48.0;
    let content = Rect::from_x_y_ranges(
        (full.left() + pad)..=(full.right() - pad),
        (full.top() + 48.0)..=(full.bottom() - 104.0),
    );
    let stage_w = content.width().min(if split { 1200.0 } else { 860.0 });
    let cx = full.center().x;
    let stage = Rect::from_x_y_ranges(
        (cx - stage_w / 2.0)..=(cx + stage_w / 2.0),
        content.top()..=content.bottom(),
    );

    let (head, lyrics) = if split && vertical {
        let col_w = stage.width().min(680.0);
        let cx = stage.center().x;
        let col = Rect::from_x_y_ranges(
            (cx - col_w / 2.0)..=(cx + col_w / 2.0),
            stage.top()..=stage.bottom(),
        );
        let lyrics_h = if show_lyrics {
            (stage.height() * 0.42).clamp(140.0, stage.height() * 0.55)
        } else {
            0.0
        };
        (
            Rect::from_x_y_ranges(
                col.left()..=col.right(),
                col.top()..=(col.bottom() - lyrics_h),
            ),
            Rect::from_x_y_ranges(
                col.left()..=col.right(),
                (col.bottom() - lyrics_h)..=col.bottom(),
            ),
        )
    } else if split {
        let lyrics_w = if show_lyrics {
            (stage.width() * 0.46).clamp(200.0, 640.0)
        } else {
            0.0
        };
        let gap = if show_lyrics { 40.0 } else { 0.0 };
        let head_w = (stage.width() - lyrics_w - gap).clamp(200.0, stage.width().max(200.0));
        let group_w = head_w + gap + lyrics_w;
        let left = stage.center().x - group_w / 2.0;
        (
            Rect::from_x_y_ranges(left..=(left + head_w), stage.top()..=stage.bottom()),
            Rect::from_x_y_ranges(
                (left + head_w + gap)..=(left + group_w),
                stage.top()..=stage.bottom(),
            ),
        )
    } else {
        let head_h = 160.0;
        (
            Rect::from_x_y_ranges(
                stage.left()..=stage.right(),
                stage.top()..=(stage.top() + head_h),
            ),
            Rect::from_x_y_ranges(
                stage.left()..=stage.right(),
                (stage.top() + head_h + 12.0)..=stage.bottom(),
            ),
        )
    };

    let side = if cover_shown {
        (head.height() - 140.0)
            .min(head.width() * 0.86)
            .clamp(96.0, 400.0)
    } else {
        0.0
    };
    let top_space = ((head.height() - (side + 130.0)) * 0.5).max(0.0);
    let cover = if side > 0.0 {
        Rect::from_center_size(
            pos2(head.center().x, head.top() + top_space + side * 0.5),
            vec2(side, side),
        )
    } else {
        Rect::from_min_size(head.center(), vec2(0.0, 0.0))
    };
    let meta = Rect::from_x_y_ranges(
        head.left()..=head.right(),
        (cover.bottom() + 12.0).min(head.bottom())..=head.bottom(),
    );
    let bar_w = full.width().min(860.0) - 48.0;
    let bar_cx = full.center().x;
    let bar = Rect::from_x_y_ranges(
        (bar_cx - bar_w / 2.0)..=(bar_cx + bar_w / 2.0),
        (full.bottom() - 76.0)..=(full.bottom() - 16.0),
    );
    ImmersiveRects {
        cover,
        meta,
        lyrics,
        bar,
    }
}

/// Feathered copy of a video frame, cached so the UI thread only rebuilds it
/// when the frame or the feather geometry actually changes.
///
/// The old path rebuilt a premultiplied 512² image on *every* repaint: ~16 ms
/// per image in a debug build, so prev + current cost ~65 ms/frame while a
/// video played. Frames arrive at 10 fps and the feather is constant outside
/// the layout animation, so caching makes the steady state free.
///
/// Only the *spatial* alpha is baked in; the overall opacity rides on the
/// quad's tint, so the between-song crossfade changes no pixels at all.
struct MaskCache {
    name: &'static str,
    src: Option<Arc<ColorImage>>,
    feather_from: f32,
    vfeather: f32,
    tex: Option<TextureHandle>,
}

impl MaskCache {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            src: None,
            feather_from: f32::NAN,
            vfeather: f32::NAN,
            tex: None,
        }
    }

    /// Draw `src` into `rect`, with its right edge dissolving over
    /// `feather_from..1` and its top/bottom edges over `vfeather`, scaled by
    /// `alpha`.
    fn paint(
        &mut self,
        ui: &egui::Ui,
        src: &Arc<ColorImage>,
        feather_from: f32,
        vfeather: f32,
        rect: Rect,
        alpha: f32,
    ) -> f32 {
        if alpha <= 0.001 || rect.width() <= 0.0 || rect.height() <= 0.0 {
            return 0.0;
        }
        let mut built_ms = 0.0;
        if !self.fresh(src, feather_from, vfeather) {
            let started = Instant::now();
            let img = feather_image(src, feather_from, vfeather);
            built_ms = started.elapsed().as_secs_f32() * 1000.0;
            match self.tex.as_mut() {
                Some(tex) => tex.set(img, egui::TextureOptions::default()),
                None => {
                    self.tex = Some(ui.ctx().load_texture(
                        self.name,
                        img,
                        egui::TextureOptions::default(),
                    ))
                }
            }
            self.src = Some(src.clone());
            self.feather_from = feather_from;
            self.vfeather = vfeather;
        }
        if let Some(tex) = &self.tex {
            ui.painter().image(
                tex.id(),
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::from_white_alpha((alpha.clamp(0.0, 1.0) * 255.0) as u8),
            );
        }
        built_ms
    }

    /// Whether the cached texture already matches this frame and feather.
    fn fresh(&self, src: &Arc<ColorImage>, feather_from: f32, vfeather: f32) -> bool {
        self.src.as_ref().is_some_and(|old| Arc::ptr_eq(old, src))
            && self.feather_from == feather_from
            && self.vfeather == vfeather
    }
}

/// Bake the feather into a frame's alpha (0..255 per edge) and premultiply.
///
/// The ramp is separable, so it is two 1D tables and one integer multiply per
/// pixel; the old loop recomputed a smoothstep and did a gamma-aware float
/// unpremultiply/premultiply for every pixel, which is what made a debug build
/// spend 16 ms here.
fn feather_image(src: &ColorImage, feather_from: f32, vfeather: f32) -> Arc<ColorImage> {
    let w = src.size[0];
    let h = src.size[1];
    if w == 0 || h == 0 {
        return Arc::new(src.clone());
    }
    let cols: Vec<u32> = (0..w)
        .map(|x| {
            let t = if w > 1 {
                x as f32 / (w - 1) as f32
            } else {
                0.0
            };
            if t <= feather_from {
                255
            } else {
                let k = ((t - feather_from) / (1.0 - feather_from).max(1e-4)).clamp(0.0, 1.0);
                ((1.0 - k * k * (3.0 - 2.0 * k)) * 255.0) as u32
            }
        })
        .collect();
    let rows: Vec<u32> = (0..h)
        .map(|y| {
            let vy = if h > 1 {
                y as f32 / (h - 1) as f32
            } else {
                0.0
            };
            if vfeather <= 0.0 {
                255
            } else if vy < vfeather {
                let k = (vy / vfeather).clamp(0.0, 1.0);
                (k * k * (3.0 - 2.0 * k) * 255.0) as u32
            } else if vy > 1.0 - vfeather {
                let k = ((1.0 - vy) / vfeather).clamp(0.0, 1.0);
                (k * k * (3.0 - 2.0 * k) * 255.0) as u32
            } else {
                255
            }
        })
        .collect();
    let mut img = src.clone();
    for (y, rf) in rows.iter().enumerate() {
        let base = y * w;
        let row = &mut img.pixels[base..base + w];
        for (px, hf) in row.iter_mut().zip(cols.iter()) {
            let a = (hf * rf + 127) / 255;
            let r = (px.r() as u32 * a + 127) / 255;
            let g = (px.g() as u32 * a + 127) / 255;
            let b = (px.b() as u32 * a + 127) / 255;
            *px = Color32::from_rgba_premultiplied(r as u8, g as u8, b as u8, a as u8);
        }
    }
    Arc::new(img)
}

fn paint_stage_mesh(
    painter: &egui::Painter,
    stage: Rect,
    feather_w: f32,
    alpha: f32,
    dim: f32,
    tex: &TextureHandle,
) {
    if alpha <= 0.001 {
        return;
    }
    if feather_w <= 0.5 {
        // Plain cover in the regular immersive layout: no dissolve at all.
        painter.image(
            tex.id(),
            stage,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::from_white_alpha((alpha * dim * 255.0) as u8),
        );
        return;
    }
    let [tw, th] = tex.size();
    if tw == 0 || th == 0 || stage.width() <= 0.0 || stage.height() <= 0.0 {
        return;
    }
    // Same object-fit crop as `paint_cover_crop`.
    let ta = tw as f32 / th as f32;
    let ra = stage.width() / stage.height();
    let uv = if ta > ra {
        let w = ra / ta;
        Rect::from_x_y_ranges((0.5 - w * 0.5)..=(0.5 + w * 0.5), 0.0..=1.0)
    } else {
        let h = ta / ra;
        Rect::from_x_y_ranges(0.0..=1.0, (0.5 - h * 0.5)..=(0.5 + h * 0.5))
    };

    const COLS: usize = 96;
    let feather_from = 1.0 - (feather_w / stage.width()).clamp(0.05, 1.0);
    let mut mesh = egui::Mesh::with_texture(tex.id());
    for i in 0..=COLS {
        let t = i as f32 / COLS as f32;
        let fade = if t <= feather_from {
            1.0
        } else {
            let x = ((t - feather_from) / (1.0 - feather_from)).clamp(0.0, 1.0);
            1.0 - x * x * (3.0 - 2.0 * x)
        };
        // 0.78 dims the video so white lyrics over the right stay legible.
        let color = Color32::from_white_alpha((fade * alpha * dim * 255.0) as u8);
        let x = stage.left() + stage.width() * t;
        let u = uv.left() + uv.width() * t;
        mesh.vertices.push(egui::epaint::Vertex {
            pos: pos2(x, stage.top()),
            uv: pos2(u, uv.top()),
            color,
        });
        mesh.vertices.push(egui::epaint::Vertex {
            pos: pos2(x, stage.bottom()),
            uv: pos2(u, uv.bottom()),
            color,
        });
    }
    for i in 0..COLS {
        let a = (i * 2) as u32;
        mesh.indices
            .extend_from_slice(&[a, a + 1, a + 2, a + 2, a + 1, a + 3]);
    }
    painter.add(egui::Shape::Mesh(std::sync::Arc::new(mesh)));
}

// ---------------------------------------------------------------------------
// Overlay rendering
// ---------------------------------------------------------------------------

impl SonoraApp {
    /// Render the fullscreen/immersive overlay in place of the normal
    /// chrome (the old overlay covers the whole viewport). Dispatches to
    /// the classic or immersive variant per the `immersive` setting.
    pub fn render_fullscreen(&mut self, ui: &mut egui::Ui) {
        let started = Instant::now();
        let full = ui.max_rect();
        let tweening = self.fs.tick();
        if self.settings.immersive {
            let flowing = self.settings.im_bg == ImBg::Flow
                && self.fs.tick_flow(self.now_playing_id().as_deref(), full);
            self.render_immersive(ui, full, tweening || flowing);
        } else {
            self.render_classic(ui, full, tweening);
        }
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        self.fs.perf_ms = lerp(self.fs.perf_ms, ms, 0.1);
        let dt = ui.ctx().input(|i| i.stable_dt).max(1e-4);
        self.fs.fps = lerp(self.fs.fps, 1.0 / dt, 0.1);
        if std::env::var_os("SONORA_PERF").is_some() {
            ui.painter().text(
                pos2(full.left() + 8.0, full.top() + 8.0),
                egui::Align2::LEFT_TOP,
                format!(
                    "{:.1} ms · {:.0} fps · blend {:.2}{}\nmotion pull {:.1} copy {:.1} upload {:.1} · mask {:.1}",
                    self.fs.perf_ms,
                    self.fs.fps,
                    self.fs.blend_mix,
                    if cfg!(debug_assertions) { " · debug" } else { "" },
                    self.fs.motion_pull_ms,
                    self.fs.motion_copy_ms,
                    self.fs.motion_upload_ms,
                    self.fs.mask_ms
                ),
                egui::FontId::proportional(14.0),
                c::TEXT,
            );
        }
    }

    /// Current track's artwork template: the bar's by-track map, else the
    /// queue mirror entry (a restored session only has art there).
    fn fs_art(&self) -> Option<String> {
        self.current_art()
    }

    /// Demand the current track's motion rendition for a surface and hand
    /// back its frame once decoded — callers paint it instead of the
    /// still, which stays underneath as the fallback (`motionFailed` in
    /// the old app). Returns `None` with no demand while animated covers
    /// are off or nothing is playing, so the sweep tears the slot down.
    fn motion_cover(&mut self, slot: Slot) -> Option<TextureHandle> {
        self.motion_cover_inner(slot, self.settings.motion_covers)
    }

    /// Demand a Motion slot and return its decoded pixels for masked blending.
    fn motion_frame_forced(&mut self, slot: Slot, key: &str) -> Option<Arc<ColorImage>> {
        let url = self.motion.url(key, slot);
        self.motion.demand(slot, key, url.as_deref());
        self.motion.frame(slot, key)
    }

    fn motion_cover_inner(&mut self, slot: Slot, enabled: bool) -> Option<TextureHandle> {
        if !enabled {
            return None;
        }
        let tid = self.now_playing_id()?;
        let key = format!("s:{tid}");
        let url = self.motion.url(&key, slot);
        self.motion.demand(slot, &key, url.as_deref());
        self.motion.tex(slot, &key)
    }

    fn fs_meta(&self) -> (String, String, String) {
        self.now_meta()
    }

    fn fs_pos_dur(&self) -> (u64, u64) {
        match &self.player.report {
            Some(r) if r.track_id.is_some() => (self.player.clock.est(), r.duration_ms),
            _ => self.player.restored.unwrap_or((0, 0)),
        }
    }

    /// Fetch the cover palette for `art` (kicking the sized fetch on first
    /// sight) and adopt it into the overlay state.
    fn fs_note_palette(&mut self, ui: &egui::Ui, art: Option<&str>, size: u32) {
        let pal = art.and_then(|a| {
            let ctx = ui.ctx().clone();
            self.covers.palette(&ctx, &self.backend, a, size).cloned()
        });
        self.fs.note_art(art, pal.as_ref());
    }

    /// Stable per-track base-gradient angle (old `--im-angle`).
    fn fs_angle(&self) -> f32 {
        let seed = self.now_playing_id().map(|t| {
            t.bytes()
                .fold(2166136261u32, |a, b| (a ^ b as u32).wrapping_mul(16777619))
        });
        (seed.unwrap_or(7) % 360) as f32 / 360.0 * std::f32::consts::TAU
    }

    // -- classic fullscreen --------------------------------------------------

    fn render_classic(&mut self, ui: &mut egui::Ui, full: Rect, tweening: bool) {
        let art = self.fs_art();
        self.fs_note_palette(ui, art.as_deref(), 600);

        // Ambient radial glow (old `radial-gradient(120% 100% at 50% 0%)`,
        // approximated as a vertical ambient→bg ramp).
        let ambient = self.fs.ambient;
        linear_gradient(
            ui.painter(),
            full,
            std::f32::consts::FRAC_PI_2,
            &[(0.0, ambient), (0.62, c::BG), (1.0, c::BG)],
        );

        let (title, artist, album) = self.fs_meta();
        let (pos_ms, dur_ms) = self.fs_pos_dur();
        let horizontal = self.settings.fs_layout == FsLayout::Horizontal;
        let show_lyrics = self.fs_lyrics_open();

        self.fs_close(ui, full);

        // Animated cover (old `#fsCoverVideo`): square-first rendition,
        // following playback; the still shows until the first frame lands.
        let cover = self.motion_cover(Slot::Fs).or_else(|| {
            art.as_ref().and_then(|a| {
                self.covers.pin(a, 600);
                let ctx = ui.ctx().clone();
                self.covers.get(&ctx, &self.backend, a, 600)
            })
        });

        // Content column/row, centered between the top pad and the bar.
        let pad_x = 48.0;
        let content = Rect::from_x_y_ranges(
            (full.left() + pad_x)..=(full.right() - pad_x),
            (full.top() + 48.0)..=(full.bottom() - 96.0),
        );

        if horizontal {
            // Centered row: [cover + titles] [lyrics], scaled to the window.
            let lyrics_w = if show_lyrics {
                (content.width() * 0.40).clamp(200.0, 520.0)
            } else {
                0.0
            };
            let gap = if show_lyrics { 40.0 } else { 0.0 };
            let head_w = (content.width() - lyrics_w - gap).clamp(220.0, 460.0);
            let group_w = head_w + gap + lyrics_w;
            let left = content.center().x - group_w / 2.0;
            let head =
                Rect::from_x_y_ranges(left..=(left + head_w), content.top()..=content.bottom());
            let lyrics_rect = Rect::from_x_y_ranges(
                (head.right() + gap)..=(head.right() + gap + lyrics_w),
                content.top()..=content.bottom(),
            );
            self.fs_head(ui, head, &title, &artist, &album, cover.as_ref(), true);
            if show_lyrics {
                self.fs_lyrics(ui, lyrics_rect, pos_ms, false);
            }
        } else {
            let col_w = content.width().min(760.0);
            let cx = full.center().x;
            let col = Rect::from_x_y_ranges(
                (cx - col_w / 2.0)..=(cx + col_w / 2.0),
                content.top()..=content.bottom(),
            );
            let lyrics_h = if show_lyrics {
                (content.height() * 0.36).clamp(120.0, 360.0)
            } else {
                0.0
            };
            let head = Rect::from_x_y_ranges(
                col.left()..=col.right(),
                col.top()..=(col.bottom() - lyrics_h),
            );
            self.fs_head(ui, head, &title, &artist, &album, cover.as_ref(), false);
            if show_lyrics {
                let lyrics_rect = Rect::from_x_y_ranges(
                    col.left()..=col.right(),
                    (col.bottom() - lyrics_h)..=col.bottom(),
                );
                self.fs_lyrics(ui, lyrics_rect, pos_ms, true);
            }
        }

        let bar_w = full.width().min(860.0) - 48.0;
        let cx = full.center().x;
        let bar_rect = Rect::from_x_y_ranges(
            (cx - bar_w / 2.0)..=(cx + bar_w / 2.0),
            (full.bottom() - 76.0)..=(full.bottom() - 16.0),
        );
        self.fs_bar(ui, bar_rect, pos_ms, dur_ms, false);
        if tweening {
            ui.ctx().request_repaint();
        }
    }

    // -- immersive -----------------------------------------------------------

    /// Drive the Kawarp dynamic background for one frame: hand it the
    /// current cover's raw pixels (one delivery per track through the
    /// shared 600² thumbcache path) plus the energy-driven speed
    /// inputs, and report whether its paint callback should be queued
    /// over the gradient.
    ///
    /// The warp is a still cover, not a video. `motion_covers` only gates
    /// the HLS slots; tying Kawarp to that checkbox made "animated covers
    /// off" drop the immersive background back to the old flow.
    fn tick_dynamic_bg(&mut self, ui: &egui::Ui, art: Option<&str>) -> bool {
        if !matches!(self.settings.im_bg, ImBg::Dynamic | ImBg::DynamicBlended) || kawarp::failed()
        {
            kawarp::reset();
            return false;
        }
        let Some(a) = art else {
            return kawarp::has_image();
        };
        let pixels = {
            let ctx = ui.ctx().clone();
            self.covers.dyn_pixels(&ctx, &self.backend, a)
        };
        let (playing, energy) = self
            .player
            .report
            .as_ref()
            .map_or((false, 0.0), |r| (r.playing, r.energy));
        // Wanted while playing or still loading the track: loading keeps
        // the OLD cover drifting so the wall-clock crossfade lands as a
        // morph rather than a cut once the new pixels show up; paused
        // freezes the frame (old `dynamicRenderWanted`).
        let still = pixels.clone().map(Arc::new);
        kawarp::update(a, pixels, energy, playing || self.player.play.loading);
        if let Some(still) = still {
            self.fs.blend_still = Some(still);
        }
        true
    }

    fn render_immersive(&mut self, ui: &mut egui::Ui, full: Rect, animating: bool) {
        let art = self.fs_art();
        self.fs_note_palette(ui, art.as_deref(), 600);

        let bg = self.settings.im_bg;
        let blended = bg == ImBg::DynamicBlended;
        let dynamic = matches!(bg, ImBg::Dynamic | ImBg::DynamicBlended);
        let dim = self
            .fs
            .dim(self.settings.im_dim.clamp(0.0, 90.0) / 100.0, dynamic);

        // Base gradient through the six palette colors at a per-track angle.
        let angle = self.fs_angle();
        linear_gradient(
            ui.painter(),
            full,
            angle,
            &[
                (0.0, self.fs.palette_color(0)),
                (0.32, self.fs.palette_color(2)),
                (0.62, self.fs.palette_color(4)),
                (0.88, self.fs.palette_color(1)),
            ],
        );

        // Dynamic paints through Kawarp over the gradient — the gradient
        // stays underneath as the fallback (no cover pixels yet, paused
        // before the first frame, or a permanent GL failure).
        if self.tick_dynamic_bg(ui, art.as_deref()) {
            ui.painter().add(kawarp::paint_callback(full));
        }

        // Background layer by mode. Motion still degrades to static blur
        // until P3d; Dynamic is the Kawarp callback queued above.
        if bg == ImBg::Flow {
            let painter = ui.painter().clone();
            for b in self.fs.blobs() {
                let center = b.base + vec2(b.x, b.y);
                let radius = b.size * 0.5 * b.s;
                soft_blob(
                    &painter,
                    center,
                    radius,
                    self.fs.palette_color(b.color),
                    b.o,
                );
            }
        }
        if matches!(bg, ImBg::Static | ImBg::Motion) {
            // Motion mode draws the decoded loop cover-filled across the
            // bleed (old sharp `#imBgVideo`); the 48px still underneath is
            // the fallback until frames land and after any failure.
            let motion_bg = (bg == ImBg::Motion)
                .then(|| self.motion_cover(Slot::ImBg))
                .flatten();
            let blur = self.settings.im_blur;
            if let Some(tex) = motion_bg {
                paint_cover_crop(ui.painter(), full, &tex, blur);
            } else if let Some(a) = &art {
                let ctx = ui.ctx().clone();
                if let Some(tex) = self.covers.get(&ctx, &self.backend, a, 48) {
                    paint_cover_fill(ui.painter(), full, &tex, blur);
                }
            }
        }

        // Scrim: vertical black readability gradient + (non-dynamic) wash.
        self.fs_scrim(ui, full, dim, dynamic);

        let (title, artist, album) = self.fs_meta();
        let (pos_ms, dur_ms) = self.fs_pos_dur();
        let show_lyrics = self.fs_lyrics_open();
        let cover_shown = self.settings.im_cover != ImCover::Hidden;
        let split = self.settings.im_layout == ImLayout::Split;
        let vertical = self.settings.fs_layout == crate::app::FsLayout::Vertical;

        // Demand Motion for the blended mode and drive the single geometry
        // animation. The previous frame is held across track changes.
        let dt = ui.ctx().input(|i| i.stable_dt);
        let track = self.now_playing_id();
        let motion_key = track.as_ref().map(|id| format!("s:{id}"));
        let blend_frame = if blended {
            motion_key
                .as_deref()
                .and_then(|key| self.motion_frame_forced(Slot::ImBg, key))
        } else {
            None
        };
        if blended && self.fs.blend_track != track {
            self.fs.blend_track = track;
            if self.fs.blend_mix > 0.01 {
                self.fs.blend_prev = self.fs.blend_last.take();
                self.fs.blend_swap = 0.0;
            } else {
                self.fs.blend_prev = None;
                self.fs.blend_swap = 1.0;
            }
        }
        if let Some(tex) = &blend_frame {
            self.fs.blend_last = Some(tex.clone());
        }
        let target = if blend_frame.is_some() || self.fs.blend_prev.is_some() {
            1.0
        } else {
            0.0
        };
        let step = dt / 0.45;
        self.fs.blend_mix = if self.fs.blend_mix < target {
            (self.fs.blend_mix + step).min(target)
        } else {
            (self.fs.blend_mix - step).max(target)
        };
        if (self.fs.blend_mix - target).abs() > 0.001 {
            ui.ctx().request_repaint();
        }
        if self.fs.blend_prev.is_some() {
            self.fs.blend_swap = (self.fs.blend_swap + dt / 0.5).min(1.0);
            if self.fs.blend_swap >= 1.0 {
                self.fs.blend_prev = None;
            } else {
                ui.ctx().request_repaint();
            }
        }
        if self.fs.blend_mix <= 0.001 {
            self.fs.blend_last = None;
            self.fs.blend_prev = None;
        }
        let mix = self.fs.blend_mix;

        let normal = immersive_rects(full, split, vertical, show_lyrics, cover_shown);
        let blend = blend_layout(full);
        let cover_rect = lerp_rect(normal.cover, blend.stage, mix);
        let meta_rect = lerp_rect(normal.meta, blend.meta, mix);
        let lyrics_rect = lerp_rect(normal.lyrics, blend.lyrics, mix);
        let bar_rect = lerp_rect(normal.bar, blend.bar, mix);
        let feather_w = blend.feather_w * mix;
        let vfeather = 0.18 * mix;
        let cover_dim = lerp(1.0, 0.78, mix);

        let feather_from = if cover_rect.width() > 0.0 {
            1.0 - (feather_w / cover_rect.width()).clamp(0.05, 1.0)
        } else {
            1.0
        };
        let motion_front = if !blended && self.settings.im_cover == ImCover::Animated {
            self.motion_cover(Slot::Im)
        } else {
            None
        };
        let still_tex = if let Some(a) = &art {
            self.covers.pin(a, 800);
            let ctx = ui.ctx().clone();
            self.covers.get(&ctx, &self.backend, a, 800)
        } else {
            None
        };
        let mut mask_ms = 0.0f32;
        if cover_shown {
            if let Some(tex) = motion_front {
                crate::widgets::paint_cover(ui.painter(), cover_rect, &tex, 20.0);
            } else if mix <= 0.001 {
                if let Some(tex) = still_tex {
                    crate::widgets::paint_cover(ui.painter(), cover_rect, &tex, 20.0);
                }
            } else if self.fs.blend_last.is_none() && self.fs.blend_prev.is_none() {
                // A Motion frame covers the still completely; only build and
                // upload a feathered still when there is no video at all.
                if let Some(pixels) = self.fs.blend_still.clone() {
                    mask_ms += self.fs.mask_still.paint(
                        ui,
                        &pixels,
                        feather_from,
                        vfeather,
                        cover_rect,
                        mix * cover_dim,
                    );
                } else if let Some(tex) = still_tex {
                    paint_stage_mesh(ui.painter(), cover_rect, feather_w, mix, cover_dim, &tex);
                }
            }
        }
        let prev = self.fs.blend_prev.clone();
        let current = self.fs.blend_last.clone();
        if let Some(pixels) = prev {
            let old =
                cover_rect.translate(vec2(-cover_rect.width() * 0.08 * self.fs.blend_swap, 0.0));
            let alpha = mix * (1.0 - self.fs.blend_swap) * cover_dim;
            mask_ms += self
                .fs
                .mask_prev
                .paint(ui, &pixels, feather_from, vfeather, old, alpha);
        }
        if let Some(pixels) = current {
            let incoming = if self.fs.blend_prev.is_some() {
                cover_rect.translate(vec2(
                    cover_rect.width() * 0.08 * (1.0 - self.fs.blend_swap),
                    0.0,
                ))
            } else {
                cover_rect
            };
            let alpha = mix
                * if self.fs.blend_prev.is_some() {
                    self.fs.blend_swap
                } else {
                    1.0
                };
            mask_ms += self.fs.mask_cur.paint(
                ui,
                &pixels,
                feather_from,
                vfeather,
                incoming,
                alpha * cover_dim,
            );
        }
        self.fs.mask_ms = lerp(self.fs.mask_ms, mask_ms, 0.1);

        let scrim_alpha = (lerp(0.0, if self.fs.light { 225.0 } else { 190.0 }, mix)) as u8;
        let scrim = Rect::from_min_max(
            pos2(full.left(), full.bottom() - 240.0),
            pos2(
                full.left() + full.width() * lerp(0.0, 0.68, mix),
                full.bottom(),
            ),
        );
        linear_gradient(
            ui.painter(),
            scrim,
            std::f32::consts::FRAC_PI_2,
            &[
                (0.0, Color32::from_black_alpha(0)),
                (1.0, Color32::from_black_alpha(scrim_alpha)),
            ],
        );

        self.render_im_meta(ui, meta_rect, mix, &title, &artist, &album);
        if show_lyrics {
            self.fs_lyrics(ui, lyrics_rect, pos_ms, mix < 0.5 && (!split || vertical));
        }
        self.fs_bar(ui, bar_rect, pos_ms, dur_ms, mix < 0.5);
        self.fs_close(ui, full);
        if animating {
            ui.ctx().request_repaint();
        }
    }

    // -- shared pieces -------------------------------------------------------

    fn fs_close(&mut self, ui: &mut egui::Ui, full: Rect) {
        let rect = Rect::from_min_size(
            pos2(full.right() - 62.0, full.top() + 18.0),
            vec2(40.0, 40.0),
        );
        let resp = ui.interact(rect, ui.make_persistent_id("fs_close"), Sense::click());
        ui.painter()
            .rect_filled(rect, CornerRadius::from(20.0), c::PANEL2);
        icons::close(ui.painter(), rect, c::TEXT);
        let resp = resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Exit fullscreen (Esc)");
        if resp.clicked() {
            let ctx = ui.ctx().clone();
            self.exit_fullscreen(&ctx);
        }
    }

    /// Cover + title/artist/album head for the classic overlay.
    #[allow(clippy::too_many_arguments)]
    fn fs_head(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        title: &str,
        artist: &str,
        album: &str,
        cover: Option<&TextureHandle>,
        horizontal: bool,
    ) {
        let align = if horizontal {
            egui::Align::Min
        } else {
            egui::Align::Center
        };
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(align)),
        );
        let ui = &mut child;

        let side = if cover.is_some() {
            let cap = if horizontal { 360.0 } else { 340.0 };
            (rect.height() - 120.0)
                .min(rect.width() * 0.92)
                .clamp(72.0, cap)
        } else {
            0.0
        };
        let stack = side + if side > 0.0 { 118.0 } else { 90.0 };
        ui.add_space(((rect.height() - stack) * 0.5).max(0.0));
        if let Some(tex) = cover {
            let (r, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
            crate::widgets::paint_cover(ui.painter(), r, tex, 18.0);
            ui.add_space(22.0);
        }

        ui.add(egui::Label::new(
            RichText::new(title)
                .font(fonts::strong_font(30.0))
                .color(c::TEXT),
        ));
        ui.add_space(4.0);
        if !artist.is_empty() {
            let resp = ui
                .add(
                    egui::Label::new(RichText::new(artist).size(17.0).color(c::ACCENT2))
                        .sense(Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Open artist");
            if resp.hovered() {
                ui.painter().line_segment(
                    [resp.rect.left_bottom(), resp.rect.right_bottom()],
                    Stroke::new(1.0, c::ACCENT2),
                );
            }
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.open_artist_by_name(&ctx, artist);
            }
            ui.add_space(8.0);
        }
        if !album.is_empty() {
            let resp = ui
                .add(
                    egui::Label::new(RichText::new(album).size(14.0).color(c::DIM))
                        .sense(Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Open album");
            if resp.hovered() {
                ui.painter().line_segment(
                    [resp.rect.left_bottom(), resp.rect.right_bottom()],
                    Stroke::new(1.0, c::ACCENT2),
                );
            }
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.open_album_by_name(&ctx, artist, album);
            }
        }
    }

    /// The overlay lyrics pane is open for timed lines, plain text, a
    /// loading fetch, or a session error. A normal miss stays closed so
    /// the cover and titles recenter. An idle engine with no target
    /// stays hidden.
    fn fs_lyrics_open(&self) -> bool {
        if !self.settings.fs_lyrics {
            return false;
        }
        match self.lyrics.state() {
            LyricState::Lines | LyricState::Text => true,
            LyricState::Error(ref e) => {
                crate::views::lyric_notice(e) == crate::views::LyricNotice::Session
            }
            LyricState::Loading => self.lyrics.engine.track_id().is_some(),
        }
    }

    /// Karaoke lyrics pane for the overlay (centered or left-aligned).
    fn fs_lyrics(&mut self, ui: &mut egui::Ui, rect: Rect, pos_ms: u64, center: bool) {
        let align = if center {
            egui::Align::Center
        } else {
            egui::Align::Min
        };
        let state = self.lyrics.state();
        let focused = self.settings.lyrics_focus;
        let scroll_to = self.lyrics.take_pending_scroll();
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(align)),
        );
        let col_w = rect.width();
        let pad = rect.height() * 0.42;
        let mut lyrics_scroll = egui::ScrollArea::vertical()
            .id_salt("fs-lyrics")
            .max_height(rect.height());
        if focused {
            lyrics_scroll = lyrics_scroll
                .scroll_source(egui::scroll_area::ScrollSource::NONE)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
        }
        let seeks = lyrics_scroll
            .show(&mut child, |ui| match &state {
                LyricState::Loading => {
                    ui.add_space(pad);
                    crate::widgets::lyric_line_skeleton(ui);
                    Vec::new()
                }
                LyricState::Error(e) => {
                    crate::views::show_lyric_notice(ui, e);
                    Vec::new()
                }
                LyricState::Text => {
                    for para in self.lyrics.engine.text().split('\n') {
                        ui.label(
                            RichText::new(para)
                                .font(crate::fonts::strong_font(24.0))
                                .color(Color32::WHITE),
                        );
                    }
                    Vec::new()
                }
                LyricState::Lines => {
                    ui.add_space(pad);
                    let seeks = crate::views::lyric_lines(
                        ui,
                        &self.lyrics.engine,
                        focused,
                        pos_ms,
                        col_w,
                        scroll_to,
                        true,
                    );
                    ui.add_space(pad);
                    seeks
                }
            })
            .inner;
        let ctx = child.ctx().clone();
        let dur = self.fs_pos_dur().1;
        for ms in seeks {
            self.on_seek(&ctx, ms.min(dur));
        }
    }

    /// Metadata renderer shared by both layouts. Font sizes and alignment
    /// ease between the regular head and the blended bottom-left column.
    fn render_im_meta(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        mix: f32,
        title: &str,
        artist: &str,
        album: &str,
    ) {
        let align = if mix < 0.5 {
            egui::Align::Center
        } else {
            egui::Align::Min
        };
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(align)),
        );
        let ui = &mut child;
        ui.add(egui::Label::new(
            RichText::new(title)
                .font(fonts::strong_font(lerp(36.0, 22.0, mix)))
                .color(c::TEXT),
        ));
        if !artist.is_empty() {
            let resp = ui.add(
                egui::Label::new(
                    RichText::new(artist)
                        .size(lerp(18.0, 14.0, mix))
                        .color(Color32::from_white_alpha(220)),
                )
                .sense(Sense::click()),
            );
            if resp.hovered() {
                ui.painter().line_segment(
                    [resp.rect.left_bottom(), resp.rect.right_bottom()],
                    Stroke::new(1.0, Color32::from_white_alpha(220)),
                );
            }
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.open_artist_by_name(&ctx, artist);
            }
        }
        if !album.is_empty() {
            let resp = ui.add(
                egui::Label::new(
                    RichText::new(album)
                        .size(lerp(14.0, 13.0, mix))
                        .color(Color32::from_white_alpha(if mix > 0.5 { 190 } else { 153 })),
                )
                .sense(Sense::click()),
            );
            if resp.hovered() {
                ui.painter().line_segment(
                    [resp.rect.left_bottom(), resp.rect.right_bottom()],
                    Stroke::new(1.0, Color32::from_white_alpha(190)),
                );
            }
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.open_album_by_name(&ctx, artist, album);
            }
        }
    }

    /// Overlay transport bar: prev / play-pause / next / mode / seek / vol.
    fn fs_bar(&mut self, ui: &mut egui::Ui, rect: Rect, pos_ms: u64, dur_ms: u64, glass: bool) {
        if glass {
            ui.painter().rect_filled(
                rect,
                CornerRadius::from(16.0),
                Color32::from_rgba_premultiplied(18, 18, 24, 140),
            );
            ui.painter().rect_stroke(
                rect,
                CornerRadius::from(16.0),
                egui::Stroke::new(1.0, Color32::from_white_alpha(26)),
                egui::StrokeKind::Inside,
            );
        }
        let inner = rect.shrink2(vec2(18.0, 8.0));
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let ui = &mut child;

        let have = self.player.report.is_some() || !self.player.queue.is_empty();
        let playing = self
            .player
            .report
            .as_ref()
            .map(|r| r.playing)
            .unwrap_or(false);
        let badge = if self.settings.loop_one {
            Some("1")
        } else if self.settings.infinite {
            Some("∞")
        } else {
            None
        };
        let mode_on = badge.is_some() || self.settings.shuffle;

        ui.add_enabled_ui(have, |ui| {
            if icons::button(ui, vec2(40.0, 40.0), icons::previous, "Previous").clicked() {
                let ctx = ui.ctx().clone();
                self.on_prev(&ctx);
            }
            let play = if playing { icons::pause } else { icons::play };
            let tip = if playing { "Pause" } else { "Play" };
            if icons::button(ui, vec2(46.0, 46.0), play, tip).clicked() {
                let ctx = ui.ctx().clone();
                self.on_play_pause(&ctx);
            }
            if icons::button(ui, vec2(40.0, 40.0), icons::next, "Next").clicked() {
                let ctx = ui.ctx().clone();
                self.on_next(&ctx);
            }
            // Playback mode (off → loop-one → infinite → shuffle).
            let (mrect, mresp) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::click());
            if mresp.hovered() {
                ui.painter()
                    .rect_filled(mrect, CornerRadius::from(18.0), c::PANEL2);
            }
            let mode_color = if mode_on { c::ACCENT2 } else { c::TEXT };
            if self.settings.shuffle {
                icons::shuffle(ui.painter(), mrect, mode_color);
            } else {
                icons::loop_icon(ui.painter(), mrect, mode_color, badge);
            }
            let tip = if self.settings.loop_one {
                "Playback mode: loop one (click for infinite queue)"
            } else if self.settings.infinite {
                "Playback mode: infinite queue (click for shuffle)"
            } else if self.settings.shuffle {
                "Playback mode: shuffle (click for off)"
            } else {
                "Playback mode: off (click for loop)"
            };
            if mresp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(tip)
                .clicked()
            {
                self.cycle_mode();
            }
        });

        // Position / seek / duration.
        ui.add_space(12.0);
        ui.label(
            RichText::new(widgets::fmt_time(pos_ms))
                .size(12.0)
                .color(c::DIM)
                .monospace(),
        );
        let seek_w = (ui.available_width() - 200.0).max(120.0);
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
            // Named for screen readers (see the player bar's seek slider).
            let resp = resp.on_hover_text("Seek");
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
                .color(c::DIM)
                .monospace(),
        );

        // Volume.
        ui.add_space(6.0);
        ui.scope(|ui| {
            ui.spacing_mut().slider_width = 90.0;
            ui.add(egui::Slider::new(&mut self.settings.volume, 0.0..=100.0).show_value(false))
                .on_hover_text("Volume");
        });
    }

    /// Immersive scrim: vertical black readability gradient + (non-dynamic)
    /// a diagonal palette wash through im1/im2 (old `#imScrim`).
    fn fs_scrim(&self, ui: &egui::Ui, full: Rect, dim: f32, dynamic: bool) {
        let top = Color32::from_black_alpha((dim * 0.55 * 255.0) as u8);
        let mid = Color32::from_black_alpha((dim * 0.25 * 255.0) as u8);
        let bot = Color32::from_black_alpha((dim * 0.72 * 255.0) as u8);
        linear_gradient(
            ui.painter(),
            full,
            std::f32::consts::FRAC_PI_2,
            &[(0.0, top), (0.35, mid), (1.0, bot)],
        );
        if dynamic {
            // Dynamic mode paints no palette wash (the canvas is the art).
            return;
        }
        let w1 = self.fs.palette_color(0).gamma_multiply(0.55);
        let w2 = self.fs.palette_color(1).gamma_multiply(0.65);
        linear_gradient(
            ui.painter(),
            full,
            -std::f32::consts::FRAC_PI_4,
            &[(0.0, w1), (1.0, w2)],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feather_image_fades_right_and_edges() {
        let src = ColorImage {
            size: [4, 4],
            source_size: vec2(4.0, 4.0),
            pixels: vec![Color32::from_rgb(200, 100, 50); 16],
        };
        let out = feather_image(&src, 0.5, 0.25);
        let alpha = |x: usize, y: usize| out.pixels[y * 4 + x].a();
        // Left half is fully opaque, the right edge dissolved away.
        assert_eq!(alpha(0, 2), 255);
        assert_eq!(alpha(1, 2), 255);
        assert_eq!(alpha(3, 2), 0);
        // Top and bottom rows feather out, the middle stays solid.
        assert_eq!(alpha(1, 0), 0);
        assert_eq!(alpha(1, 3), 0);
        // Premultiplied: no channel may exceed alpha.
        for px in &out.pixels {
            assert!(px.r() <= px.a() && px.g() <= px.a() && px.b() <= px.a());
        }
    }

    #[test]
    fn mask_cache_reuses_untouched_frames() {
        let mut cache = MaskCache::new("test");
        let src = Arc::new(ColorImage {
            size: [8, 8],
            source_size: vec2(8.0, 8.0),
            pixels: vec![Color32::from_rgb(1, 2, 3); 64],
        });
        assert!(!cache.fresh(&src, 0.5, 0.1), "empty cache never matches");
        cache.src = Some(Arc::clone(&src));
        cache.feather_from = 0.5;
        cache.vfeather = 0.1;
        assert!(cache.fresh(&src, 0.5, 0.1));
        // A new decode is a different allocation even with equal pixels.
        let other = Arc::new((*src).clone());
        assert!(!cache.fresh(&other, 0.5, 0.1));
        // Moving the feather invalidates too.
        assert!(!cache.fresh(&src, 0.51, 0.1));
    }

    #[test]
    fn blend_layout_pins_stage_left_and_keeps_lyrics_right() {
        let full = Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 900.0));
        let layout = blend_layout(full);
        assert_eq!(layout.stage.left(), full.left());
        assert_eq!(layout.stage.top(), full.top());
        assert_eq!(layout.stage.bottom(), full.bottom());
        assert!((layout.stage.width() - 864.0).abs() < 0.01);
        assert!((layout.feather_w - 388.8).abs() < 0.01);
        assert!(layout.lyrics.left() > layout.stage.left());
        assert!(layout.lyrics.right() <= full.right());
        assert!(layout.meta.bottom() <= full.bottom());
        assert!(layout.meta.left() >= full.left());
    }

    #[test]
    fn blend_layout_clamps_padding_on_narrow_windows() {
        let full = Rect::from_min_size(Pos2::ZERO, vec2(620.0, 480.0));
        let layout = blend_layout(full);
        assert!(layout.stage.width() > 0.0);
        assert!(layout.lyrics.right() <= full.right());
        assert!(layout.meta.right() <= full.right());
    }

    #[test]
    fn ease_in_out_quad_matches_old_tween() {
        assert!((ease_in_out_quad(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_in_out_quad(0.5) - 0.5).abs() < 1e-6);
        assert!((ease_in_out_quad(1.0) - 1.0).abs() < 1e-6);
        // Symmetric + monotonic ramp.
        assert!(ease_in_out_quad(0.25) < 0.5);
        assert!(ease_in_out_quad(0.75) > 0.5);
        assert!((ease_in_out_quad(0.25) + ease_in_out_quad(0.75) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sample_ramp_interpolates_between_stops() {
        let stops = [(0.0, Color32::BLACK), (1.0, Color32::WHITE)];
        assert_eq!(sample_ramp(&stops, 0.0), Color32::BLACK);
        assert_eq!(sample_ramp(&stops, 1.0), Color32::WHITE);
        let mid = sample_ramp(&stops, 0.5);
        let [r, g, b, _] = mid.to_array();
        assert!((r as i32 - 128).abs() < 3, "mid r={r}");
        assert_eq!(r, g);
        assert_eq!(g, b);
    }

    #[test]
    fn palette_tween_reaches_target_and_stops() {
        let mut fs = ImmersiveUi::default();
        let pal = CoverPalette {
            colors: vec![[10.0, 20.0, 30.0]],
            avg_rgb: [0.1, 0.1, 0.1],
            avg_lum: 0.1,
            light: false,
        };
        fs.note_art(Some("art://a"), Some(&pal));
        assert!(fs.tween.is_some());
        // The tween is wall-clock driven; backdate the start so a single
        // tick lands past the duration and snaps to the target.
        if let Some(t) = fs.tween.as_mut() {
            t.2 = Instant::now() - std::time::Duration::from_millis(2000);
        }
        assert!(!fs.tick());
        // Every slot cycled to the single extracted color.
        for slot in &fs.render {
            assert_eq!(*slot, [10.0, 20.0, 30.0]);
        }
        // Once done, the tween is cleared and further ticks are no-ops.
        assert!(fs.tween.is_none());
        assert!(!fs.tick());
        // Re-note of the same art is a no-op.
        fs.note_art(Some("art://a"), Some(&pal));
        assert!(fs.tween.is_none());
    }

    #[test]
    fn light_bg_boosts_dim_except_dynamic() {
        let mut fs = ImmersiveUi::default();
        let pal = CoverPalette {
            colors: vec![[240.0, 240.0, 240.0]],
            avg_rgb: [0.9, 0.9, 0.9],
            avg_lum: 0.8,
            light: true,
        };
        fs.note_art(Some("art://pale"), Some(&pal));
        assert!((fs.dim(0.55, false) - 0.77).abs() < 1e-4);
        assert!((fs.dim(0.55, true) - 0.55).abs() < 1e-4);
    }

    #[test]
    fn flow_seeds_blobs_per_track() {
        let mut fs = ImmersiveUi::default();
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0));
        fs.seed_flow(Some("track-1"), rect);
        let n = fs.blobs().len();
        assert!((8..=12).contains(&n), "blob count {n}");
        // Same track keeps the same composition (base positions stable).
        let first_base = fs.blobs()[0].base;
        fs.tick_flow(Some("track-1"), rect);
        assert_eq!(fs.blobs()[0].base, first_base);
        // A new track reseeds (count varies 8–12 but stays in range).
        fs.tick_flow(Some("track-2"), rect);
        let n2 = fs.blobs().len();
        assert!((8..=12).contains(&n2), "reseeded blob count {n2}");
    }
}
