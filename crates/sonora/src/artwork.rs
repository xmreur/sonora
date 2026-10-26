//! Cover art: Apple artwork-template substitution, disk-cached sized
//! thumbnails fetched off-thread on the backend runtime, decoded into
//! egui textures and kept under a byte-capped LRU — the old UI's
//! `<img>` + thumbcache pipeline without the webview.
//!
//! Every fetch goes through `shell::thumb_cache` (same `~/.config/sonora/
//! thumbcache` files the old UI wrote), so upgrades reuse warm disks and
//! repeated views skip network entirely. Animated (HLS) covers are a
//! later phase; everything here is the static `{w}x{h}` artwork the
//! models already carry.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};

use egui::{ColorImage, TextureHandle, TextureOptions};

/// GPU-side cap for decoded cover textures (48 MB ≈ 500 covers at 150²,
/// or ~300 at 300²). Eviction is least-recently-used.
const TEX_CAP: usize = 48 << 20;

/// Sized fetch for the dynamic background's raw source pixels (old
/// `dynCoverBlob(art, 600)`). Deliberately the same 600² URL the
/// theming palette pulls, so both share one thumbcache file and decode.
pub const DYN_PIXEL_SIZE: u32 = 600;

/// Substitute Apple's artwork template placeholders — mirrors the old JS
/// `art()`: `{w}`/`{h}` become the requested square size, `{c}`/`{f}`/
/// `{q}` the Apple-web defaults (bb crop, jpg, q60). `None` when the
/// template carries no URL.
pub fn art_url(template: &str, size: u32) -> Option<String> {
    let template = template.trim();
    if template.is_empty() {
        return None;
    }
    let s = size.to_string();
    Some(
        template
            .replace("{w}", &s)
            .replace("{h}", &s)
            .replace("{c}", "bb")
            .replace("{f}", "jpg")
            .replace("{q}", "60"),
    )
}

/// One finished fetch: decoded pixels, or `None` on a failure (offline,
/// non-image response …) — memoized so a broken URL is not retried
/// every frame.
struct Loaded {
    key: String,
    /// Unresolved artwork template the sized URL came from — palette
    /// entries are shared across every size of one cover.
    template: String,
    image: Option<ColorImage>,
}

struct Entry {
    tex: TextureHandle,
    bytes: usize,
    stamp: u64,
}

/// Cover-derived theming (old `updateImmersivePalette` / `updateAmbient`),
/// extracted once per cover at decode time: up to six hue-diverse colors
/// for the immersive background, the deepened average for the classic
/// fullscreen ambient glow, and the light-background flag.
#[derive(Clone, Debug, Default)]
pub struct CoverPalette {
    /// Up to six vivid, hue-diverse colors (each `[r,g,b]` in 0..=255).
    pub colors: Vec<[f32; 3]>,
    /// Average cover color, 0..=1 (before the ambient darkening).
    pub avg_rgb: [f32; 3],
    /// Average relative luminance of the sampled cover, 0..=1.
    pub avg_lum: f32,
    /// Pale cover → brighter lyric text + a stronger scrim (readability).
    pub light: bool,
}

impl CoverPalette {
    /// Classic-fullscreen ambient glow: the average color deepened so the
    /// white overlay text stays readable (old `updateAmbient` ×0.55).
    pub fn ambient(&self) -> egui::Color32 {
        egui::Color32::from_rgb(
            (self.avg_rgb[0] * 0.55 * 255.0) as u8,
            (self.avg_rgb[1] * 0.55 * 255.0) as u8,
            (self.avg_rgb[2] * 0.55 * 255.0) as u8,
        )
    }
}

/// sRGB relative luminance (old `rgbRelativeLuminance`): the WCAG Y value
/// used to detect pale covers.
fn relative_luminance(r: u8, g: u8, b: u8) -> f32 {
    let lin = |c: u8| {
        let x = c as f32 / 255.0;
        if x <= 0.03928 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// Hue in degrees, or `-1` for near-gray (no meaningful hue). Forces hue
/// diversity so similar shades can't fill multiple palette slots.
fn rgb_hue(r: f32, g: f32, b: f32) -> f32 {
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let d = mx - mn;
    if d < 24.0 {
        return -1.0;
    }
    let mut h = if mx == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if mx == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    h *= 60.0;
    if h < 0.0 {
        h += 360.0;
    }
    h
}

/// L1 distance between two colors (old `colorDist`).
fn color_dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs()
}

/// Extract the theming palette from a decoded cover (port of
/// `updateImmersivePalette`): quantize to 4-bit buckets, rank by
/// popularity × saturation, then pick up to six hue-diverse colors — one
/// per 30° sector first, then distance-filtered, then best scorers. Also
/// accumulates the average color/luminance for the ambient glow and the
/// light-background check.
fn extract_palette(img: &ColorImage) -> CoverPalette {
    const S: usize = 32;
    let (w, h) = (img.size[0].max(1), img.size[1].max(1));

    // 4-bit color buckets over a 32×32 sample; skip transparent pixels.
    let mut buckets: std::collections::HashMap<u16, u32> = std::collections::HashMap::new();
    let mut n = 0u32;
    let mut lum_sum = 0.0f32;
    let mut r_sum = 0.0f32;
    let mut g_sum = 0.0f32;
    let mut b_sum = 0.0f32;
    for gy in 0..S {
        let y = (gy * h / S).min(h - 1);
        for gx in 0..S {
            let x = (gx * w / S).min(w - 1);
            let [r, g, b, a] = img.pixels[y * w + x].to_srgba_unmultiplied();
            if a < 128 {
                continue;
            }
            n += 1;
            lum_sum += relative_luminance(r, g, b);
            r_sum += r as f32;
            g_sum += g as f32;
            b_sum += b as f32;
            let key = (((r >> 4) as u16) << 8) | (((g >> 4) as u16) << 4) | (b >> 4) as u16;
            *buckets.entry(key).or_insert(0) += 1;
        }
    }
    if n == 0 {
        return CoverPalette::default();
    }

    // Rank buckets by sqrt(count) × (1 + sat/64): a 10× larger field must
    // not get 10× the score, or the dominant color crowds out accents.
    let mut ranked: Vec<([f32; 3], f32, f32)> = buckets
        .iter()
        .map(|(&key, &count)| {
            let r = (((key >> 8) & 15) as f32) * 16.0 + 8.0;
            let g = (((key >> 4) & 15) as f32) * 16.0 + 8.0;
            let b = ((key & 15) as f32) * 16.0 + 8.0;
            let sat = r.max(g).max(b) - r.min(g).min(b);
            let score = (count as f32).sqrt() * (1.0 + sat / 64.0);
            ([r, g, b], rgb_hue(r, g, b), score)
        })
        .collect();
    ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut picked: Vec<[f32; 3]> = Vec::with_capacity(6);
    // Pass 1 — hue diversity: one color per 30° sector (grays share a
    // sector), also distance-filtered so adjacent shades of the dominant
    // color can't take a second slot.
    let mut sectors: std::collections::HashSet<i32> = std::collections::HashSet::new();
    for (rgb, hue, _) in &ranked {
        if picked.len() >= 6 {
            break;
        }
        let sec = if *hue < 0.0 { -1 } else { (*hue / 30.0) as i32 };
        if !sectors.contains(&sec) && picked.iter().all(|p| color_dist(*p, *rgb) > 60.0) {
            sectors.insert(sec);
            picked.push(*rgb);
        }
    }
    // Pass 2 — fill leftovers by distance alone.
    for (rgb, _, _) in &ranked {
        if picked.len() >= 6 {
            break;
        }
        if picked.iter().all(|p| color_dist(*p, *rgb) > 80.0) {
            picked.push(*rgb);
        }
    }
    // Pass 3 — last resort: best scorers regardless (near-monochrome
    // covers legitimately have fewer than six distinct colors).
    for (rgb, _, _) in &ranked {
        if picked.len() >= 6 {
            break;
        }
        if !picked.contains(rgb) {
            picked.push(*rgb);
        }
    }

    // Pale-cover detection: the cover average catches pale art when the
    // hue-diverse palette still includes darker accent slots.
    let avg_lum = lum_sum / n as f32;
    let max_l = picked
        .iter()
        .map(|c| relative_luminance(c[0] as u8, c[1] as u8, c[2] as u8))
        .fold(0.0f32, f32::max);
    let avg_pl = if picked.is_empty() {
        0.0
    } else {
        picked
            .iter()
            .map(|c| relative_luminance(c[0] as u8, c[1] as u8, c[2] as u8))
            .sum::<f32>()
            / picked.len() as f32
    };
    let light = avg_lum > 0.42 || max_l > 0.52 || avg_pl > 0.48;

    // No darkening: blob colors go out at full extracted brightness so they
    // read against the base even for dark covers; readability comes from the
    // scrim, not from muting colors. Vivid clamp keeps whites/blacks usable.
    let vivid = |v: f32| v.clamp(24.0, 255.0);
    let colors = picked
        .iter()
        .map(|c| [vivid(c[0]), vivid(c[1]), vivid(c[2])])
        .collect();

    CoverPalette {
        colors,
        avg_rgb: [
            r_sum / n as f32 / 255.0,
            g_sum / n as f32 / 255.0,
            b_sum / n as f32 / 255.0,
        ],
        avg_lum,
        light,
    }
}

/// Decoded cover textures keyed by resolved URL, fetched through the
/// shell disk cache and evicted least-recently-used past [`TEX_CAP`].
pub struct Covers {
    entries: HashMap<String, Entry>,
    bytes: usize,
    clock: u64,
    in_flight: HashSet<String>,
    failed: HashSet<String>,
    /// Cover-derived theming palette, keyed by *unresolved template* so
    /// every size of one cover shares a single extraction (computed once,
    /// off the decode path, in `pump`).
    palettes: HashMap<String, CoverPalette>,
    /// Raw-pixel delivery for the Kawarp dynamic background, url-keyed
    /// (old `dynCoverBlob`): the url currently asked for, requests in
    /// flight, urls whose fetch failed, and at most one ready delivery
    /// awaiting the consumer.
    pixel_last_url: Option<String>,
    pixel_pending: HashSet<String>,
    pixel_failed: HashSet<String>,
    pixel_ready: VecDeque<(String, ColorImage)>,
    tx: Sender<Loaded>,
    rx: Receiver<Loaded>,
}

impl Default for Covers {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
            in_flight: HashSet::new(),
            failed: HashSet::new(),
            palettes: HashMap::new(),
            pixel_last_url: None,
            pixel_pending: HashSet::new(),
            pixel_failed: HashSet::new(),
            pixel_ready: VecDeque::new(),
            tx,
            rx,
        }
    }
}

impl Covers {
    /// Texture for an artwork template at `size`, starting the
    /// disk/network fetch on first request. `None` while loading (or
    /// after a failure) — callers keep the letter tile meanwhile. The
    /// handle is cheap to clone (shared `Arc`), so widgets store their
    /// own copy.
    pub fn get(
        &mut self,
        ctx: &egui::Context,
        backend: &crate::backend::Backend,
        template: &str,
        size: u32,
    ) -> Option<TextureHandle> {
        let url = art_url(template, size)?;
        if let Some(entry) = self.entries.get_mut(&url) {
            self.clock += 1;
            entry.stamp = self.clock;
            return Some(entry.tex.clone());
        }
        if self.failed.contains(&url) || !self.in_flight.insert(url.clone()) {
            return None;
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let tmpl = template.to_owned();
        backend.spawn(async move {
            let image = fetch_decode(&url, size).await.ok();
            let _ = tx.send(Loaded {
                key: url,
                template: tmpl,
                image,
            });
            ctx.request_repaint();
        });
        None
    }

    /// Theming palette for an artwork template (old
    /// `updateImmersivePalette` / `updateAmbient` extraction). Starts the
    /// sized fetch on first request and answers from the decoded pixels
    /// once they land — `None` while loading or after a failure, so the
    /// caller falls back to the built-in defaults.
    pub fn palette(
        &mut self,
        ctx: &egui::Context,
        backend: &crate::backend::Backend,
        template: &str,
        size: u32,
    ) -> Option<&CoverPalette> {
        // Any size decodes the same cover; a small sample is enough and
        // cheap to decode. Use an already-cached texture when present.
        if self.palettes.contains_key(template) {
            return self.palettes.get(template);
        }
        // Reuse `get`'s fetch path (it dedupes + memoizes failures), then
        // extract from the decoded image on arrival in `pump`.
        let _ = self.get(ctx, backend, template, size);
        None
    }

    /// Raw pixels for the dynamic background (old `dynCoverBlob`): one
    /// delivery per cover change for the Kawarp port. Answers `None`
    /// while loading, after a fetch failure, and once delivered — the
    /// caller is the consumer, so this never re-delivers the same cover
    /// while the track plays. Bookkeeping is url-keyed so a request can
    /// piggyback on an identical in-flight fetch (the palette path pulls
    /// the same 600² URL).
    pub fn dyn_pixels(
        &mut self,
        ctx: &egui::Context,
        backend: &crate::backend::Backend,
        template: &str,
    ) -> Option<ColorImage> {
        let url = art_url(template, DYN_PIXEL_SIZE)?;
        // Leaving a cover forgets its attempt state, so returning to a
        // track re-requests (its file is disk-cached — the retry is one
        // read). Delivery for other urls is dropped as stale.
        if self.pixel_last_url.as_deref() != Some(url.as_str()) {
            self.pixel_last_url = Some(url.clone());
            self.pixel_pending.retain(|u| u == &url);
            self.pixel_failed.retain(|u| u == &url);
            self.pixel_ready.retain(|(u, _)| u == &url);
        }
        if let Some(i) = self.pixel_ready.iter().position(|(u, _)| u == &url) {
            return self.pixel_ready.remove(i).map(|(_, image)| image);
        }
        if self.pixel_pending.contains(&url) || self.pixel_failed.contains(&url) {
            return None;
        }
        self.pixel_pending.insert(url.clone());
        // Piggyback when the identical fetch is already running: `pump`
        // routes any landing through `pixel_pending` by url.
        if !self.in_flight.contains(&url) {
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            let tmpl = template.to_owned();
            backend.spawn(async move {
                let image = fetch_decode(&url, DYN_PIXEL_SIZE).await.ok();
                let _ = tx.send(Loaded {
                    key: url,
                    template: tmpl,
                    image,
                });
                ctx.request_repaint();
            });
        }
        None
    }

    /// Move finished fetches into textures (and their theming palettes),
    /// then evict past the byte cap. Call once per frame, before the views
    /// render.
    pub fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.rx.try_recv() {
            self.in_flight.remove(&done.key);
            match done.image {
                Some(image) => {
                    // Theming palette comes off the decoded pixels once
                    // per cover (shared across every size).
                    if !done.template.is_empty() && !self.palettes.contains_key(&done.template) {
                        let pal = extract_palette(&image);
                        if !pal.colors.is_empty() {
                            self.palettes.insert(done.template.clone(), pal);
                        }
                    }
                    // Kawarp (dynamic bg) raw-pixel delivery: one shot per
                    // cover, dropped when the cover changed meanwhile.
                    if self.pixel_pending.remove(&done.key)
                        && self.pixel_last_url.as_deref() == Some(done.key.as_str())
                    {
                        self.pixel_ready
                            .push_back((done.key.clone(), image.clone()));
                    }
                    let bytes = image.width() * image.height() * 4;
                    let tex = ctx.load_texture("cover", image, TextureOptions::default());
                    self.clock += 1;
                    self.entries.insert(
                        done.key,
                        Entry {
                            tex,
                            bytes,
                            stamp: self.clock,
                        },
                    );
                    self.bytes += bytes;
                }
                None => {
                    // A failed pixel request is not retried while this
                    // cover is current (the retry clears on cover change).
                    if self.pixel_pending.remove(&done.key) {
                        self.pixel_failed.insert(done.key.clone());
                    }
                    self.failed.insert(done.key);
                }
            }
        }
        while self.bytes > TEX_CAP {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.stamp)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            // Dropping the last handle frees the GPU texture.
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
    }
}

/// Disk-cached fetch (shell) + decode (image crate) of one sized cover.
async fn fetch_decode(url: &str, size: u32) -> Result<ColorImage, String> {
    let path = shell::thumb_cache::thumb_cache(url.to_owned(), size).await?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    decode_image(&bytes)
}

/// JPEG/PNG bytes → unpremultiplied RGBA [`ColorImage`].
fn decode_image(bytes: &[u8]) -> Result<ColorImage, String> {
    let dyn_img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        rgba.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use egui::Color32;

    use super::*;

    #[test]
    fn template_substitution_matches_old_js() {
        assert_eq!(
            art_url("https://x/{w}x{h}bb.jpg", 300).as_deref(),
            Some("https://x/300x300bb.jpg")
        );
        // Generic form: crop/format/quality placeholders must also go,
        // or the URL 404s (uploads + newer assets).
        assert_eq!(
            art_url("https://x/{w}x{h}{c}.{f}?f={q}", 100).as_deref(),
            Some("https://x/100x100bb.jpg?f=60")
        );
    }

    #[test]
    fn empty_template_is_none_and_plain_urls_pass() {
        assert!(art_url("", 300).is_none());
        assert!(art_url("   ", 300).is_none());
        assert_eq!(
            art_url("https://x/plain.jpg", 300).as_deref(),
            Some("https://x/plain.jpg")
        );
    }

    #[test]
    fn decodes_png_bytes() {
        // 1×1 red pixel through the same path covers take.
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode");
        let ci = decode_image(&png).expect("decode");
        assert_eq!(ci.size, [1, 1]);
        assert_eq!(ci.pixels[0], Color32::from_rgb(255, 0, 0));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_image(b"not an image").is_err());
        assert!(decode_image(b"").is_err());
    }
}
