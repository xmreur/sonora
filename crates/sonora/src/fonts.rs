//! System font discovery and font-family setup.
//!
//! egui has no notion of font weight (`FontId` is size + family only), so
//! "strong" text — headings, titles, card names — uses a dedicated
//! [`FontFamily::Name`] family backed by a real bold font file when one can
//! be found on disk. When none can, the family still exists and maps to the
//! regular fonts, so [`strong_font`] is always safe to use.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontId};

/// Family name used for bold/strong text (see [`strong_font`]).
pub const STRONG_FAMILY: &str = "strong";

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

    if let Some((path, bytes)) = first_file(PRIMARY) {
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

    let strong_chain: Vec<String> = match first_file(STRONG) {
        Some((path, bytes)) => {
            log::debug!("strong font: {path}");
            defs.font_data
                .insert("strong".into(), Arc::new(FontData::from_owned(bytes)));
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
        .insert(FontFamily::Name(STRONG_FAMILY.into()), strong_chain);

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

/// Font for body text.
pub fn body_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
