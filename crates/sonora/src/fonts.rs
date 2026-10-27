//! System font discovery and font-family setup.
//!
//! egui has no notion of font weight (`FontId` is size + family only), so
//! "strong" text — headings, titles, card names — uses a dedicated
//! [`FontFamily::Name`] family backed by a real bold font file when one can
//! be found on disk. When none can, the family still exists and maps to the
//! regular fonts, so [`strong_font`] is always safe to use.

use std::sync::{Arc, OnceLock};

use egui::{FontData, FontDefinitions, FontFamily, FontId};

/// Family name used for bold/strong text (see [`strong_font`]).
pub const STRONG_FAMILY: &str = "strong";

/// Unhinted face for lyrics. Hinting snaps outlines to the pixel grid,
/// which reads as jagged steps at lyric sizes.
pub const LYRIC_FAMILY: &str = "lyrics";

/// Font Awesome Free solid (SIL OFL). Transport and chrome glyphs.
pub const ICON_FAMILY: &str = "icons";

const FA_SOLID: &[u8] = include_bytes!("../assets/fa-solid-900.otf");

/// Primary UI font candidates, best first. Inter reads close to Apple's SF.
const PRIMARY: &[&str] = &[
    // Arch
    "/usr/share/fonts/inter/InterVariable.ttf",
    "/usr/share/fonts/inter/Inter-Regular.ttf",
    // Debian/Ubuntu
    "/usr/share/fonts/truetype/inter/InterVariable.ttf",
    "/usr/share/fonts/truetype/inter/Inter-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    // macOS system UI font
    "/System/Library/Fonts/SFNS.ttf",
    // Windows
    r"C:\Windows\Fonts\segoeui.ttf",
];

/// Bold companions for headings. Only .ttf/.otf — ab_glyph cannot read .ttc
/// collections, so macOS/Windows candidates are separate files.
const STRONG: &[&str] = &[
    "/usr/share/fonts/inter/Inter-Bold.ttf",
    "/usr/share/fonts/truetype/inter/Inter-Bold.ttf",
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/ubuntu/Ubuntu-B.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    r"C:\Windows\Fonts\arialbd.ttf",
];

const MONO: &[&str] = &[
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/System/Library/Fonts/Supplemental/Courier New.ttf",
    r"C:\Windows\Fonts\consola.ttf",
];

static LYRIC_BYTES: OnceLock<Vec<u8>> = OnceLock::new();

/// Bold (or regular) font bytes used to rasterize lyrics.
pub fn lyric_bytes() -> Option<&'static [u8]> {
    LYRIC_BYTES.get().map(Vec::as_slice)
}

fn first_file(candidates: &'static [&'static str]) -> Option<(&'static str, Vec<u8>)> {
    candidates.iter().find_map(|path| {
        let bytes = std::fs::read(path).ok()?;
        if bytes.is_empty() {
            None
        } else {
            Some((*path, bytes))
        }
    })
}

/// Discover system fonts and install them on the egui context.
pub fn install(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    let primary = first_file(PRIMARY);
    let strong = first_file(STRONG);
    if LYRIC_BYTES.get().is_none() {
        let bytes = strong
            .as_ref()
            .map(|(_, bytes)| bytes.clone())
            .or_else(|| primary.as_ref().map(|(_, bytes)| bytes.clone()));
        if let Some(bytes) = bytes {
            let _ = LYRIC_BYTES.set(bytes);
        }
    }

    if let Some((path, bytes)) = primary {
        log::debug!("ui font: {path}");
        defs.font_data
            .insert("ui".into(), Arc::new(FontData::from_owned(bytes)));
        if let Some(chain) = defs.families.get_mut(&FontFamily::Proportional) {
            chain.insert(0, "ui".into());
        }
    }

    let proportional: Vec<String> = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let strong_chain: Vec<String> = match strong {
        Some((path, bytes)) => {
            log::debug!("strong font: {path}");
            let data = FontData::from_owned(bytes);
            let mut smooth = data.clone();
            smooth.tweak.hinting = Some(false);
            defs.font_data.insert("strong".into(), Arc::new(data));
            defs.font_data.insert("lyrics".into(), Arc::new(smooth));
            let mut chain = vec!["strong".to_string()];
            chain.extend(proportional);
            chain
        }
        None => {
            log::debug!("no bold font found on disk; strong text uses the regular font");
            proportional
        }
    };
    defs.families
        .insert(FontFamily::Name(STRONG_FAMILY.into()), strong_chain.clone());
    let lyric_chain = if defs.font_data.contains_key("lyrics") {
        let mut chain = vec!["lyrics".to_string()];
        chain.extend(
            defs.families
                .get(&FontFamily::Proportional)
                .cloned()
                .unwrap_or_default(),
        );
        chain
    } else {
        strong_chain
    };
    defs.families
        .insert(FontFamily::Name(LYRIC_FAMILY.into()), lyric_chain);

    defs.font_data
        .insert("fa-solid".into(), Arc::new(FontData::from_static(FA_SOLID)));
    defs.families.insert(
        FontFamily::Name(ICON_FAMILY.into()),
        vec!["fa-solid".into()],
    );

    if let Some((path, bytes)) = first_file(MONO) {
        log::debug!("mono font: {path}");
        defs.font_data
            .insert("mono".into(), Arc::new(FontData::from_owned(bytes)));
        if let Some(chain) = defs.families.get_mut(&FontFamily::Monospace) {
            chain.insert(0, "mono".into());
        }
    }

    ctx.set_fonts(defs);
}

/// Font for headings and other strong text.
pub fn strong_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(STRONG_FAMILY.into()))
}

/// Unhinted face so large lyric glyphs stay grayscale-antialiased.
pub fn lyric_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(LYRIC_FAMILY.into()))
}

/// Font for body text.
pub fn body_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// Font Awesome solid, for transport and chrome icons.
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICON_FAMILY.into()))
}
