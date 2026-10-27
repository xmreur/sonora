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

use std::time::Instant;

use egui::{
    load::SizedTexture, pos2, vec2, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Shape,
    TextureHandle, UiBuilder,
};

use crate::app::{FsLayout, ImBg, ImCover, ImLayout, SonoraApp};
use crate::artwork::CoverPalette;
use crate::kawarp;
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

/// Stretch a small cover texture across `rect` — a soft "blur" for the
/// static background via bilinear upscaling.
fn paint_cover_fill(painter: &egui::Painter, rect: Rect, tex: &TextureHandle) {
    painter.image(
        tex.id(),
        rect,
        Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

/// `object-fit: cover` for a sharp texture: UVs are cropped so the image
/// fills `rect` without distortion (old `#imBgVideo` over the full bleed).
fn paint_cover_crop(painter: &egui::Painter, rect: Rect, tex: &TextureHandle) {
    let [tw, th] = tex.size();
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
    painter.image(tex.id(), rect, uv, Color32::WHITE);
}

// ---------------------------------------------------------------------------
// Overlay rendering
// ---------------------------------------------------------------------------

impl SonoraApp {
    /// Render the fullscreen/immersive overlay in place of the normal
    /// chrome (the old overlay covers the whole viewport). Dispatches to
    /// the classic or immersive variant per the `immersive` setting.
    pub fn render_fullscreen(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let tweening = self.fs.tick();
        if self.settings.immersive {
            let flowing = self.settings.im_bg == ImBg::Flow
                && self.fs.tick_flow(self.now_playing_id().as_deref(), full);
            self.render_immersive(ui, full, tweening || flowing);
        } else {
            self.render_classic(ui, full, tweening);
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
        if !self.settings.motion_covers {
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
        let show_lyrics = self.settings.fs_lyrics && self.lyrics.engine.line_count() > 0;

        self.fs_close(ui, full);

        // Animated cover (old `#fsCoverVideo`): square-first rendition,
        // following playback; the still shows until the first frame lands.
        let cover = self.motion_cover(Slot::Fs).or_else(|| {
            art.as_ref().and_then(|a| {
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
            // Row: [cover + titles] [lyrics].
            let lyrics_w = if show_lyrics { 520.0f32 } else { 0.0 };
            let head_w = (content.width() - lyrics_w - 48.0).max(300.0);
            let head = Rect::from_x_y_ranges(
                content.left()..=(content.left() + head_w),
                content.top()..=content.bottom(),
            );
            let lyrics_rect = Rect::from_x_y_ranges(
                (head.right() + 48.0)..=content.right(),
                content.top()..=content.bottom(),
            );
            self.fs_head(ui, head, &title, &artist, &album, cover.as_ref(), true);
            if show_lyrics {
                self.fs_lyrics(ui, lyrics_rect, pos_ms, false);
            }
        } else {
            let col_w = content.width().min(920.0);
            let cx = full.center().x;
            let col = Rect::from_x_y_ranges(
                (cx - col_w / 2.0)..=(cx + col_w / 2.0),
                content.top()..=content.bottom(),
            );
            self.fs_head(ui, col, &title, &artist, &album, cover.as_ref(), false);
            if show_lyrics {
                let lh = content.height() * 0.44;
                let lyrics_rect = Rect::from_x_y_ranges(
                    col.left()..=col.right(),
                    (content.bottom() - lh - 16.0)..=(content.bottom() - 16.0),
                );
                self.fs_lyrics(ui, lyrics_rect, pos_ms, true);
            }
        }

        self.fs_bar(ui, full, pos_ms, dur_ms, false);
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
    /// Eligibility mirrors the old `maybeApplyDynamicBg` guards (mode on,
    /// `motion_covers` honored, no permanent failure). With no cover art
    /// the running instance keeps painting its last image — the old
    /// `applyDynamicBg` early-return left it alone too — which means
    /// gradient until the first cover ever arrives.
    fn tick_dynamic_bg(&mut self, ui: &egui::Ui, art: Option<&str>) -> bool {
        if self.settings.im_bg != ImBg::Dynamic || !self.settings.motion_covers || kawarp::failed()
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
        kawarp::update(a, pixels, energy, playing || self.player.play.loading);
        true
    }

    fn render_immersive(&mut self, ui: &mut egui::Ui, full: Rect, animating: bool) {
        let art = self.fs_art();
        self.fs_note_palette(ui, art.as_deref(), 600);

        let bg = self.settings.im_bg;
        let dynamic = bg == ImBg::Dynamic;
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
            if let Some(tex) = motion_bg {
                paint_cover_crop(ui.painter(), full, &tex);
            } else if let Some(a) = &art {
                let ctx = ui.ctx().clone();
                if let Some(tex) = self.covers.get(&ctx, &self.backend, a, 48) {
                    paint_cover_fill(ui.painter(), full, &tex);
                }
            }
        }

        // Scrim: vertical black readability gradient + (non-dynamic) wash.
        self.fs_scrim(ui, full, dim, dynamic);

        let (title, artist, album) = self.fs_meta();
        let (pos_ms, dur_ms) = self.fs_pos_dur();
        let cover_shown = self.settings.im_cover != ImCover::Hidden;
        let show_lyrics = self.settings.fs_lyrics && self.lyrics.engine.line_count() > 0;
        let cover = if cover_shown {
            // Foreground motion only in the `Animated` cover mode (old
            // `im-cover-motion`); anything else tears the slot down.
            let motion = (self.settings.im_cover == ImCover::Animated)
                .then(|| self.motion_cover(Slot::Im))
                .flatten();
            motion.or_else(|| {
                art.as_ref().and_then(|a| {
                    let ctx = ui.ctx().clone();
                    self.covers.get(&ctx, &self.backend, a, 800)
                })
            })
        } else {
            None
        };

        self.fs_close(ui, full);

        let split = self.settings.im_layout == ImLayout::Split;
        let mini = !split && cover_shown && cover.is_some();
        let pad_x = 48.0;
        let content = Rect::from_x_y_ranges(
            (full.left() + pad_x)..=(full.right() - pad_x),
            (full.top() + 48.0)..=(full.bottom() - 104.0),
        );
        let max_w = if split { 1200.0 } else { 860.0 };
        let stage_w = content.width().min(max_w);
        let cx = full.center().x;
        let stage = Rect::from_x_y_ranges(
            (cx - stage_w / 2.0)..=(cx + stage_w / 2.0),
            content.top()..=content.bottom(),
        );

        if split {
            let lyrics_w = if show_lyrics { 640.0f32 } else { 0.0 };
            let head_w = (stage.width() - lyrics_w - 56.0).clamp(320.0, 400.0);
            let head = Rect::from_x_y_ranges(
                stage.left()..=(stage.left() + head_w),
                stage.top()..=stage.bottom(),
            );
            let lyrics_rect = Rect::from_x_y_ranges(
                (head.right() + 56.0)..=stage.right(),
                stage.top()..=stage.bottom(),
            );
            self.im_head(
                ui,
                head,
                &title,
                &artist,
                &album,
                cover.as_ref(),
                mini,
                false,
            );
            if show_lyrics {
                self.fs_lyrics(ui, lyrics_rect, pos_ms, false);
            }
        } else {
            let head_h = if mini { 96.0 } else { 160.0 };
            let head = Rect::from_x_y_ranges(
                stage.left()..=stage.right(),
                stage.top()..=(stage.top() + head_h),
            );
            self.im_head(
                ui,
                head,
                &title,
                &artist,
                &album,
                cover.as_ref(),
                mini,
                mini,
            );
            if show_lyrics {
                let lh = stage.height().min(full.height() * 0.52);
                let top = stage.top() + head_h + 12.0;
                let lyrics_rect =
                    Rect::from_x_y_ranges(stage.left()..=stage.right(), top..=(top + lh));
                self.fs_lyrics(ui, lyrics_rect, pos_ms, true);
            }
        }

        self.fs_bar(ui, full, pos_ms, dur_ms, true);
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

        if let Some(tex) = cover {
            let base: f32 = if horizontal { 360.0 } else { 300.0 };
            let side = base.min(rect.height() * 0.34).max(120.0);
            let (r, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
            egui::Image::new(SizedTexture::new(tex.id(), vec2(side, side)))
                .corner_radius(18.0)
                .paint_at(ui, r);
            ui.add_space(22.0);
        }

        ui.add(egui::Label::new(
            RichText::new(title)
                .font(fonts::strong_font(30.0))
                .color(c::TEXT),
        ));
        ui.add_space(4.0);
        if !artist.is_empty() {
            let resp = ui.add(
                egui::Label::new(RichText::new(artist).size(17.0).color(c::ACCENT2))
                    .sense(Sense::click()),
            );
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.open_artist_by_name(&ctx, artist);
            }
            ui.add_space(8.0);
        }
        if !album.is_empty() {
            let resp = ui.add(
                egui::Label::new(RichText::new(album).size(14.0).color(c::DIM))
                    .sense(Sense::click()),
            );
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.open_album_by_name(&ctx, artist, album);
            }
        }
    }

    /// Cover + titles head for the immersive overlay (mini row or full column).
    #[allow(clippy::too_many_arguments)]
    fn im_head(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        title: &str,
        artist: &str,
        album: &str,
        cover: Option<&TextureHandle>,
        mini: bool,
        _row: bool,
    ) {
        let layout = if mini {
            egui::Layout::left_to_right(egui::Align::Center)
        } else {
            egui::Layout::top_down(egui::Align::Center)
        };
        let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(layout));
        let ui = &mut child;

        if let Some(tex) = cover {
            let side = if mini {
                72.0
            } else {
                380.0f32.min(rect.height() * 0.42).max(160.0)
            };
            let (r, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
            egui::Image::new(SizedTexture::new(tex.id(), vec2(side, side)))
                .corner_radius(if mini { 12.0 } else { 20.0 })
                .paint_at(ui, r);
            ui.add_space(if mini { 14.0 } else { 22.0 });
        }

        let title_size = if mini { 22.0 } else { 36.0 };
        ui.add(egui::Label::new(
            RichText::new(title)
                .font(fonts::strong_font(title_size))
                .color(c::WHITE),
        ));
        ui.add_space(4.0);
        if !artist.is_empty() {
            let resp = ui.add(
                egui::Label::new(
                    RichText::new(artist)
                        .size(if mini { 15.0 } else { 18.0 })
                        .color(Color32::from_white_alpha(217)),
                )
                .sense(Sense::click()),
            );
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.open_artist_by_name(&ctx, artist);
            }
        }
        if !album.is_empty() && !mini {
            ui.add_space(6.0);
            let resp = ui.add(
                egui::Label::new(
                    RichText::new(album)
                        .size(14.0)
                        .color(Color32::from_white_alpha(153)),
                )
                .sense(Sense::click()),
            );
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.open_album_by_name(&ctx, artist, album);
            }
        }
    }

    /// Karaoke lyrics pane for the overlay (centered or left-aligned).
    fn fs_lyrics(&mut self, ui: &mut egui::Ui, rect: Rect, pos_ms: u64, center: bool) {
        let align = if center {
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
        let col_w = rect.width();
        let seeks = {
            let engine = &self.lyrics.engine;
            crate::views::lyric_lines(ui, engine, true, pos_ms, col_w, None)
        };
        let ctx = ui.ctx().clone();
        let dur = self.fs_pos_dur().1;
        for ms in seeks {
            self.on_seek(&ctx, ms.min(dur));
        }
    }

    /// Overlay transport bar: prev / play-pause / next / mode / seek / vol.
    fn fs_bar(&mut self, ui: &mut egui::Ui, full: Rect, pos_ms: u64, dur_ms: u64, glass: bool) {
        let bar_w = full.width().min(if glass { 900.0 } else { 860.0 }) - 48.0;
        let cx = full.center().x;
        let rect = Rect::from_x_y_ranges(
            (cx - bar_w / 2.0)..=(cx + bar_w / 2.0),
            (full.bottom() - 76.0)..=(full.bottom() - 16.0),
        );
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
            // Playback mode (off → loop-one → infinite).
            let (mrect, mresp) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::click());
            if mresp.hovered() {
                ui.painter()
                    .rect_filled(mrect, CornerRadius::from(18.0), c::PANEL2);
            }
            let mode_color = if badge.is_some() { c::ACCENT2 } else { c::TEXT };
            icons::loop_icon(ui.painter(), mrect, mode_color, badge);
            let tip = if self.settings.loop_one {
                "Playback mode: loop one (click for infinite queue)"
            } else if self.settings.infinite {
                "Playback mode: infinite queue (click for off)"
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
                .color(c::DIM),
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
