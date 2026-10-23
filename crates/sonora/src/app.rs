//! Application state and the persistent chrome (sidebar, top bar, player bar).

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use egui::{vec2, Margin, Pos2, RichText, Sense, Stroke};

use crate::fonts;
use crate::icons;
use crate::theme::c;

/// Sidebar footer build tag.
pub const BUILD: &str = concat!("build ", env!("CARGO_PKG_VERSION"));

const ACTIVITY_CAP: usize = 120;
const SIDEBAR_W: f32 = 248.0;

/// Which screen the content area shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Browse,
    Search,
    Playlists,
    Queue,
    Lyrics,
    Settings,
}

// ---------------------------------------------------------------------------
// Settings (mirror the old `aml-settings` localStorage object; JSON
// persistence lands with the settings phase)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsLayout {
    Vertical,
    Horizontal,
}

impl FsLayout {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vertical => "vertical",
            Self::Horizontal => "horizontal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImLayout {
    Split,
    Overlay,
}

impl ImLayout {
    pub fn label(self) -> &'static str {
        match self {
            Self::Split => "split (cover + lyrics)",
            Self::Overlay => "overlay (lyrics over background)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImBg {
    Flow,
    Static,
    Motion,
    Dynamic,
    Gradient,
}

impl ImBg {
    pub fn label(self) -> &'static str {
        match self {
            Self::Flow => "immersive (old)",
            Self::Static => "blurred still cover",
            Self::Motion => "animated cover",
            Self::Dynamic => "immersive",
            Self::Gradient => "colors only",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImCover {
    Animated,
    Still,
    Hidden,
}

impl ImCover {
    pub fn label(self) -> &'static str {
        match self {
            Self::Animated => "animated",
            Self::Still => "still",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    // Display
    pub fs_lyrics: bool,
    pub lyrics_focus: bool,
    pub fs_layout: FsLayout,
    pub native_fs: bool,
    // Immersive fullscreen
    pub immersive: bool,
    pub im_layout: ImLayout,
    pub im_bg: ImBg,
    pub motion_covers: bool,
    pub im_cover: ImCover,
    pub im_blur: f32,
    pub im_dim: f32,
    // Integrations
    pub notifications: bool,
    pub discord: bool,
    pub discord_app_id: String,
    // Debug
    pub debug: bool,
    // Playback (UI-side)
    pub infinite: bool,
    pub loop_one: bool,
    pub volume: f32,
    // Auth
    pub advanced_auth: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fs_lyrics: true,
            lyrics_focus: false,
            fs_layout: FsLayout::Vertical,
            native_fs: false,
            immersive: true,
            im_layout: ImLayout::Split,
            im_bg: ImBg::Dynamic,
            motion_covers: true,
            im_cover: ImCover::Animated,
            im_blur: 24.0,
            im_dim: 55.0,
            notifications: false,
            discord: false,
            discord_app_id: String::new(),
            debug: false,
            infinite: true,
            loop_one: false,
            volume: 80.0,
            advanced_auth: false,
        }
    }
}

// ---------------------------------------------------------------------------

pub struct ActivityRow {
    pub stamp: String,
    pub text: String,
    pub dbg: bool,
}

pub struct SonoraApp {
    pub view: View,
    pub search_input: String,
    pub search_term: String,
    pub status: String,
    pub activity: VecDeque<ActivityRow>,
    pub settings: Settings,
    pub fullscreen: bool,
    pub mut_input: String,
    pub auth_url_input: String,
    seek: f32,
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

impl Default for SonoraApp {
    fn default() -> Self {
        Self::new()
    }
}

impl SonoraApp {
    pub fn new() -> Self {
        let mut app = Self {
            view: View::Browse,
            search_input: String::new(),
            search_term: String::new(),
            status: String::new(),
            activity: VecDeque::new(),
            settings: Settings::default(),
            fullscreen: false,
            mut_input: String::new(),
            auth_url_input: String::new(),
            seek: 0.0,
        };
        app.log(format!("sonora {} — egui shell", env!("CARGO_PKG_VERSION")));
        app
    }

    pub fn log(&mut self, text: impl Into<String>) {
        self.activity.push_back(ActivityRow {
            stamp: stamp(),
            text: text.into(),
            dbg: false,
        });
        while self.activity.len() > ACTIVITY_CAP {
            self.activity.pop_front();
        }
    }

    pub fn log_dbg(&mut self, text: impl Into<String>) {
        self.activity.push_back(ActivityRow {
            stamp: stamp(),
            text: text.into(),
            dbg: true,
        });
        while self.activity.len() > ACTIVITY_CAP {
            self.activity.pop_front();
        }
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    pub fn run_search(&mut self) {
        let term = self.search_input.trim().to_owned();
        if term.is_empty() {
            return;
        }
        self.log(format!("search “{term}”"));
        self.search_term = term;
        self.view = View::Search;
    }

    /// off → loop-one → infinite queue → off (the old cycle button).
    pub fn cycle_mode(&mut self) {
        if self.settings.loop_one {
            self.settings.loop_one = false;
            self.settings.infinite = true;
            self.set_status("Infinite queue on");
        } else if self.settings.infinite {
            self.settings.infinite = false;
            self.set_status("Playback mode off");
        } else {
            self.settings.loop_one = true;
            self.set_status("Looping the current track");
        }
        self.log("mode cycle");
    }

    // -- chrome -------------------------------------------------------------

    fn render_topbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("topbar")
            .frame(
                egui::Frame::new()
                    .fill(c::BG)
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                let mut run = false;
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.search_input)
                            .hint_text("Search songs, albums, playlists…")
                            .desired_width(f32::INFINITY),
                    );
                    if resp.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter)) {
                        run = true;
                    }
                    let search_btn = egui::Button::new(
                        RichText::new("Search")
                            .color(c::WHITE)
                            .font(fonts::strong_font(14.0)),
                    )
                    .fill(c::ACCENT)
                    .corner_radius(egui::CornerRadius::from(10.0))
                    .min_size(vec2(0.0, 38.0));
                    if ui.add(search_btn).clicked() {
                        run = true;
                    }
                });

                // hairline under the bar
                let r = ui.max_rect();
                ui.painter().line_segment(
                    [
                        Pos2::new(r.left(), r.bottom() - 0.5),
                        Pos2::new(r.right(), r.bottom() - 0.5),
                    ],
                    Stroke::new(1.0, c::LINE),
                );

                if run {
                    self.run_search();
                }
            });
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("sidebar")
            .exact_size(SIDEBAR_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(c::PANEL).inner_margin(14.0))
            .show(ui, |ui| {
                // brand
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("♪")
                            .font(fonts::strong_font(20.0))
                            .color(c::ACCENT),
                    );
                    ui.label(
                        RichText::new("Sonora")
                            .font(fonts::strong_font(20.0))
                            .color(c::TEXT),
                    );
                });
                ui.add_space(6.0);

                // nav
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
                    let items = [
                        (View::Browse, "Browse"),
                        (View::Playlists, "Playlists"),
                        (View::Queue, "Queue"),
                        (View::Lyrics, "Lyrics"),
                        (View::Settings, "Settings"),
                    ];
                    for (view, label) in items {
                        let selected = self.view == view;
                        if nav_button(ui, label, selected).clicked() {
                            self.view = view;
                        }
                    }
                });

                ui.add_space(10.0);
                ui.label(RichText::new(&self.status).size(12.0).color(c::DIM));
                ui.add_space(10.0);

                // Activity section
                section_line(ui);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("ACTIVITY")
                            .font(fonts::strong_font(12.0))
                            .color(c::DIM),
                    );
                    ui.checkbox(&mut self.settings.debug, "debug");
                });
                if self.settings.debug {
                    self.activity_log(ui);
                }

                ui.add_space(8.0);
                ui.label(RichText::new(BUILD).size(11.0).color(c::DIM));
                ui.label(RichText::new("Not signed in").size(12.0).color(c::DIM));
            });
    }

    fn activity_log(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("activity")
            .max_height(150.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for row in &self.activity {
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        &format!("{} ", row.stamp),
                        0.0,
                        egui::TextFormat {
                            font_id: fonts::body_font(11.0),
                            color: c::DIM,
                            ..Default::default()
                        },
                    );
                    job.append(
                        &row.text,
                        0.0,
                        egui::TextFormat {
                            font_id: fonts::body_font(11.0),
                            color: if row.dbg { c::DIM } else { c::TEXT },
                            ..Default::default()
                        },
                    );
                    ui.add(egui::Label::new(job));
                }
            });
    }

    fn render_playerbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("playerbar")
            .frame(
                egui::Frame::new()
                    .fill(c::PANEL)
                    .inner_margin(Margin::symmetric(14, 10)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().line_segment(
                    [
                        Pos2::new(r.left(), r.top() + 0.5),
                        Pos2::new(r.right(), r.top() + 0.5),
                    ],
                    Stroke::new(1.0, c::LINE),
                );

                ui.horizontal(|ui| {
                    // cover placeholder
                    let (cover, _) = ui.allocate_exact_size(vec2(52.0, 52.0), Sense::click());
                    ui.painter()
                        .rect_filled(cover, egui::CornerRadius::from(8.0), c::PANEL2);
                    icons::music_note(ui.painter(), cover, c::DIM);

                    // now-playing meta
                    ui.allocate_ui_with_layout(
                        vec2(240.0, 52.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.add_space(17.0);
                            ui.label(
                                RichText::new("Not playing.")
                                    .font(fonts::strong_font(14.0))
                                    .color(c::TEXT),
                            );
                        },
                    );

                    // transport
                    ui.add_space(10.0);
                    if icons::button(ui, vec2(34.0, 34.0), icons::previous, "Previous").clicked() {
                        self.set_status("Nothing queued.");
                    }
                    if icons::button(ui, vec2(38.0, 38.0), icons::play, "Play").clicked() {
                        self.set_status("Nothing queued.");
                    }
                    if icons::button(ui, vec2(34.0, 34.0), icons::next, "Next").clicked() {
                        self.set_status("Nothing queued.");
                    }

                    // playback mode (off → loop-one → infinite)
                    let badge = if self.settings.loop_one {
                        Some("1")
                    } else if self.settings.infinite {
                        Some("∞")
                    } else {
                        None
                    };
                    let tip = if self.settings.loop_one {
                        "Playback mode: loop one (click for infinite queue)"
                    } else if self.settings.infinite {
                        "Playback mode: infinite queue (click for off)"
                    } else {
                        "Playback mode: off (click for loop)"
                    };
                    let (mrect, mresp) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::click());
                    if mresp.hovered() {
                        ui.painter()
                            .rect_filled(mrect, egui::CornerRadius::from(17.0), c::PANEL2);
                    }
                    let mode_color = if badge.is_some() { c::ACCENT2 } else { c::TEXT };
                    icons::loop_icon(ui.painter(), mrect, mode_color, badge);
                    let mresp = mresp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(tip);
                    if mresp.clicked() {
                        self.cycle_mode();
                    }

                    // position / seek / duration
                    ui.add_space(12.0);
                    ui.label(RichText::new("0:00").size(12.0).color(c::DIM));
                    // 0.37 Slider width comes from `spacing::slider_width`, so
                    // size the seek bar to whatever is left after the trailing
                    // widgets (duration, queue button, volume, fullscreen).
                    let trailing =
                        46.0 + 30.0 + 110.0 + 30.0 + ui.spacing().item_spacing.x * 6.0 + 18.0;
                    let seek_w = (ui.available_width() - trailing).max(120.0);
                    ui.scope(|ui| {
                        ui.spacing_mut().slider_width = seek_w;
                        ui.add_enabled_ui(false, |ui| {
                            ui.add(
                                egui::Slider::new(&mut self.seek, 0.0..=1000.0).show_value(false),
                            );
                        });
                    });
                    ui.label(RichText::new("0:00").size(12.0).color(c::DIM));

                    // queue / volume / fullscreen
                    ui.add_space(6.0);
                    if icons::button(ui, vec2(30.0, 30.0), icons::queue_list, "Queue").clicked() {
                        self.view = View::Queue;
                    }
                    ui.scope(|ui| {
                        ui.spacing_mut().slider_width = 110.0;
                        ui.add(
                            egui::Slider::new(&mut self.settings.volume, 0.0..=100.0)
                                .show_value(false),
                        )
                        .on_hover_text("Volume");
                    });
                    let fs_tip = if self.fullscreen {
                        "Exit fullscreen"
                    } else {
                        "Fullscreen"
                    };
                    if icons::button(ui, vec2(30.0, 30.0), icons::fullscreen, fs_tip).clicked() {
                        self.fullscreen = !self.fullscreen;
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                    }
                });
            });
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }
}

impl eframe::App for SonoraApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Order matters: the player bar must span the full window width, so
        // it claims space before the sidebar and top bar.
        self.render_playerbar(ui);
        self.render_sidebar(ui);
        self.render_topbar(ui);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(c::BG)
                    .inner_margin(Margin::symmetric(22, 18)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("view")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.render_view(ui));
            });

        self.handle_keys(ui.ctx());
    }
}

// ---------------------------------------------------------------------------

/// Sidebar navigation item: transparent pill, accent2 + strong when active.
fn nav_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width().max(1.0), 32.0), Sense::click());
    if selected || resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::from(8.0), c::PANEL2);
    }
    let color = if selected { c::ACCENT2 } else { c::TEXT };
    let font = if selected {
        fonts::strong_font(14.0)
    } else {
        fonts::body_font(14.0)
    };
    ui.painter().text(
        rect.left_center() + vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        color,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Hairline separator used between sidebar sections (old `.side-section`).
fn section_line(ui: &egui::Ui) {
    let y = ui.cursor().top() - ui.spacing().item_spacing.y * 0.5;
    let r = ui.max_rect();
    ui.painter().line_segment(
        [Pos2::new(r.left(), y), Pos2::new(r.right(), y)],
        Stroke::new(1.0, c::LINE),
    );
}
