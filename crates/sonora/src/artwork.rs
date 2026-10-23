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

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};

use egui::{ColorImage, TextureHandle, TextureOptions};

/// GPU-side cap for decoded cover textures (48 MB ≈ 500 covers at 150²,
/// or ~300 at 300²). Eviction is least-recently-used.
const TEX_CAP: usize = 48 << 20;

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
    image: Option<ColorImage>,
}

struct Entry {
    tex: TextureHandle,
    bytes: usize,
    stamp: u64,
}

/// Decoded cover textures keyed by resolved URL, fetched through the
/// shell disk cache and evicted least-recently-used past [`TEX_CAP`].
pub struct Covers {
    entries: HashMap<String, Entry>,
    bytes: usize,
    clock: u64,
    in_flight: HashSet<String>,
    failed: HashSet<String>,
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
        backend.spawn(async move {
            let image = fetch_decode(&url, size).await.ok();
            let _ = tx.send(Loaded { key: url, image });
            ctx.request_repaint();
        });
        None
    }

    /// Move finished fetches into textures, then evict past the byte cap.
    /// Call once per frame, before the views render.
    pub fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.rx.try_recv() {
            self.in_flight.remove(&done.key);
            match done.image {
                Some(image) => {
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
