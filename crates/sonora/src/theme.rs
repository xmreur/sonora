//! Visual identity — palette, fonts, widget styling.
//!
//! The palette mirrors the CSS custom properties of the old web UI so the
//! egui build reads as the same app.

use egui::{vec2, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

use crate::fonts;

/// App palette (was `:root` CSS variables).
pub mod c {
    use egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0e);
    pub const PANEL: Color32 = Color32::from_rgb(0x14, 0x14, 0x19);
    pub const PANEL2: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x22);
    pub const HOVER: Color32 = Color32::from_rgb(0x26, 0x26, 0x2f);
    pub const LINE: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x33);
    pub const TEXT: Color32 = Color32::from_rgb(0xf5, 0xf5, 0xf7);
    pub const DIM: Color32 = Color32::from_rgb(0xa1, 0xa1, 0xa9);
    pub const ACCENT: Color32 = Color32::from_rgb(0xfa, 0x24, 0x3c);
    pub const ACCENT2: Color32 = Color32::from_rgb(0xfc, 0x5c, 0x7d);
    pub const WHITE: Color32 = Color32::WHITE;
}

/// Install fonts and apply the app-wide style.
pub fn apply(ctx: &egui::Context) {
    fonts::install(ctx);

    // Sonora is dark-only: pin the theme and install the same style for
    // both variants so an OS light-mode preference can't drift us off-palette.
    ctx.set_theme(egui::Theme::Dark);

    let mut style = egui::Style::default();

    // Typography. egui has no font weights, so "bold" is the separate
    // `strong` family from `fonts`.
    style
        .text_styles
        .insert(TextStyle::Heading, fonts::strong_font(20.0));
    style
        .text_styles
        .insert(TextStyle::Body, fonts::body_font(14.0));
    style
        .text_styles
        .insert(TextStyle::Button, fonts::body_font(14.0));
    style
        .text_styles
        .insert(TextStyle::Small, fonts::body_font(12.0));
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );

    // Spacing (old CSS paddings: nav buttons 8/10, cards gap 16, …).
    style.spacing.item_spacing = vec2(8.0, 6.0);
    style.spacing.button_padding = vec2(10.0, 5.0);

    style.visuals = visuals();
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();

    v.panel_fill = c::PANEL;
    v.window_fill = c::PANEL2;
    v.override_text_color = Some(c::TEXT);
    v.weak_text_color = Some(c::DIM);
    v.hyperlink_color = c::ACCENT2;
    // Text-edit backgrounds, scroll-area backgrounds.
    v.extreme_bg_color = c::PANEL2;
    v.faint_bg_color = c::HOVER;
    v.selection.bg_fill = Color32::from_rgba_premultiplied(0xfa, 0x24, 0x3c, 110);
    v.selection.stroke = Stroke::new(1.0, c::ACCENT);

    v.window_corner_radius = CornerRadius::from(12.0);
    v.window_stroke = Stroke::new(1.0, c::LINE);
    v.menu_corner_radius = CornerRadius::from(12.0);
    v.slider_trailing_fill = true;

    let ni = &mut v.widgets.noninteractive;
    ni.bg_fill = c::PANEL2;
    ni.bg_stroke = Stroke::new(1.0, c::LINE);
    ni.corner_radius = CornerRadius::from(8.0);
    ni.fg_stroke = Stroke::new(1.0, c::TEXT);

    let inactive = &mut v.widgets.inactive;
    inactive.weak_bg_fill = c::PANEL2;
    inactive.bg_fill = c::PANEL2;
    inactive.bg_stroke = Stroke::new(1.0, c::LINE);
    inactive.corner_radius = CornerRadius::from(8.0);
    inactive.fg_stroke = Stroke::new(1.0, c::TEXT);

    let hovered = &mut v.widgets.hovered;
    hovered.weak_bg_fill = c::HOVER;
    hovered.bg_fill = c::HOVER;
    hovered.bg_stroke = Stroke::new(1.0, c::DIM);
    hovered.corner_radius = CornerRadius::from(8.0);
    hovered.fg_stroke = Stroke::new(1.0, c::TEXT);

    let active = &mut v.widgets.active;
    active.weak_bg_fill = c::ACCENT;
    active.bg_fill = c::ACCENT;
    active.bg_stroke = Stroke::new(1.0, c::ACCENT2);
    active.corner_radius = CornerRadius::from(8.0);
    active.fg_stroke = Stroke::new(1.0, c::WHITE);

    v.widgets.open.weak_bg_fill = c::HOVER;
    v.widgets.open.bg_stroke = Stroke::new(1.0, c::LINE);

    v
}
