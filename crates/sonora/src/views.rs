//! View rendering — one method per screen.
//!
//! Data here is static preview content until the backend phase wires the
//! `shell` crate in; layout, controls and empty states match the old UI.

use egui::{vec2, RichText, Ui};

use crate::app::{FsLayout, ImBg, ImCover, ImLayout, SonoraApp, View};
use crate::theme::c;
use crate::widgets::{self, Card, TrackRow};

// ---------------------------------------------------------------------------
// Preview data
// ---------------------------------------------------------------------------

/// title, artist, album, duration (secs)
struct Song(&'static str, &'static str, &'static str, u64);

const CHART_SONGS: &[Song] = &[
    Song("Blinding Lights", "The Weeknd", "After Hours", 200),
    Song("Levitating", "Dua Lipa", "Future Nostalgia", 203),
    Song("As It Was", "Harry Styles", "Harry's House", 167),
    Song(
        "Bad Guy",
        "Billie Eilish",
        "When We All Fall Asleep, Where Do We Go?",
        194,
    ),
    Song("Stay", "The Kid LAROI & Justin Bieber", "F*ck Love 3", 141),
    Song("Heat Waves", "Glass Animals", "Dreamland", 238),
    Song("Midnight City", "M83", "Hurry Up, We're Dreaming", 243),
    Song("Instant Crush", "Daft Punk", "Random Access Memories", 337),
];

const CHART_ALBUMS: &[(&str, &str)] = &[
    ("After Hours", "The Weeknd"),
    ("Future Nostalgia", "Dua Lipa"),
    ("Harry's House", "Harry Styles"),
    ("Midnights", "Taylor Swift"),
    ("SOS", "SZA"),
    ("Renaissance", "Beyoncé"),
    ("Un Verano Sin Ti", "Bad Bunny"),
    ("Save Me", "The Kid LAROI"),
];

const CHART_PLAYLISTS: &[(&str, &str)] = &[
    ("Today's Hits", "Apple Music"),
    ("New Music Daily", "Apple Music"),
    ("Chill Mix", "Apple Music"),
    ("Deep Focus", "Apple Music"),
    ("Rap Life", "Apple Music"),
    ("A-List Pop", "Apple Music"),
];

const LIBRARY_PLAYLISTS: &[(&str, &str)] = &[
    ("Coding Flow", "42 songs"),
    ("Gym Mix", "28 songs"),
    ("Road Trip", "61 songs"),
    ("Late Nights", "17 songs"),
];

const ARTISTS: &[(&str, &str)] = &[
    ("The Weeknd", "R&B · Pop"),
    ("Dua Lipa", "Pop"),
    ("Tame Impala", "Psych Rock"),
    ("Fred again..", "Electronic"),
];

fn cards_of(items: &[(&'static str, &'static str)]) -> Vec<Card> {
    items.iter().map(|(t, s)| Card::new(t, s)).collect()
}

/// Wrap cards into rows of the auto-fill grid; returns the clicked index.
fn card_grid(ui: &mut Ui, cards: &[Card]) -> Option<usize> {
    let (_, width) = widgets::cards_layout(ui, 150.0);
    let mut clicked = None;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(16.0, 16.0);
        ui.horizontal_wrapped(|ui| {
            for (i, card) in cards.iter().enumerate() {
                if widgets::card(ui, width, card).clicked() {
                    clicked = Some(i);
                }
            }
        });
    });
    clicked
}

/// A `T`-valued option row with a label (old `<label>… <select>` lines).
fn combo<T: Copy + PartialEq>(
    ui: &mut Ui,
    id: &'static str,
    current: &mut T,
    label: &str,
    options: &[(T, &'static str)],
) {
    ui.horizontal(|ui| {
        ui.label(label);
        let selected = options
            .iter()
            .find(|(v, _)| v == current)
            .map(|(_, l)| *l)
            .unwrap_or("…");
        egui::ComboBox::from_id_salt(id)
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for (v, l) in options {
                    ui.selectable_value(current, *v, *l);
                }
            });
    });
}

// ---------------------------------------------------------------------------

impl SonoraApp {
    pub fn render_view(&mut self, ui: &mut Ui) {
        match self.view {
            View::Browse => self.render_browse(ui),
            View::Search => self.render_search(ui),
            View::Playlists => self.render_playlists(ui),
            View::Queue => self.render_queue(ui),
            View::Lyrics => self.render_lyrics(ui),
            View::Settings => self.render_settings(ui),
        }
    }

    fn song_list(&mut self, ui: &mut Ui, songs: &[Song]) {
        for (i, song) in songs.iter().enumerate() {
            let sub = format!("{} · {}", song.1, song.2);
            let row = TrackRow::simple(i as u32 + 1, song.0, &sub, song.3);
            if widgets::track_row(ui, &row).clicked() {
                let title = song.0;
                self.set_status(format!("“{title}” — playback lands with the player phase"));
            }
        }
    }

    fn on_card_click(&mut self, title: &str) {
        self.set_status(format!(
            "“{title}” — detail pages land with the browse phase"
        ));
    }

    fn render_browse(&mut self, ui: &mut Ui) {
        widgets::hint(
            ui,
            "Preview data — live charts arrive with the backend phase.",
        );
        ui.add_space(6.0);

        widgets::h2(ui, "Top Songs");
        ui.add_space(4.0);
        self.song_list(ui, CHART_SONGS);

        ui.add_space(16.0);
        widgets::h2(ui, "Top Albums");
        ui.add_space(6.0);
        let albums = cards_of(CHART_ALBUMS);
        if let Some(i) = card_grid(ui, &albums) {
            let title = albums[i].title.clone();
            self.on_card_click(&title);
        }

        ui.add_space(16.0);
        widgets::h2(ui, "Top Playlists");
        ui.add_space(6.0);
        let playlists = cards_of(CHART_PLAYLISTS);
        if let Some(i) = card_grid(ui, &playlists) {
            let title = playlists[i].title.clone();
            self.on_card_click(&title);
        }
    }

    fn render_search(&mut self, ui: &mut Ui) {
        let term = self.search_term.clone();
        widgets::h2(ui, &format!("Results for '{term}'"));
        widgets::hint(
            ui,
            "Preview data — live search arrives with the backend phase.",
        );
        ui.add_space(6.0);

        widgets::h3(ui, "Songs");
        ui.add_space(4.0);
        self.song_list(ui, &CHART_SONGS[..4]);

        ui.add_space(14.0);
        widgets::h3(ui, "Albums");
        ui.add_space(6.0);
        let albums = cards_of(&CHART_ALBUMS[..4]);
        if let Some(i) = card_grid(ui, &albums) {
            let title = albums[i].title.clone();
            self.on_card_click(&title);
        }

        ui.add_space(14.0);
        widgets::h3(ui, "Playlists");
        ui.add_space(6.0);
        let playlists = cards_of(&CHART_PLAYLISTS[..4]);
        if let Some(i) = card_grid(ui, &playlists) {
            let title = playlists[i].title.clone();
            self.on_card_click(&title);
        }

        ui.add_space(14.0);
        widgets::h3(ui, "Artists");
        ui.add_space(6.0);
        let artists = cards_of(ARTISTS);
        if let Some(i) = card_grid(ui, &artists) {
            let title = artists[i].title.clone();
            self.on_card_click(&title);
        }
    }

    fn render_playlists(&mut self, ui: &mut Ui) {
        widgets::h2(ui, "Your Playlists");
        widgets::hint(
            ui,
            "Preview data — library playlists arrive with the backend phase.",
        );
        ui.add_space(6.0);
        let playlists = cards_of(LIBRARY_PLAYLISTS);
        if let Some(i) = card_grid(ui, &playlists) {
            let title = playlists[i].title.clone();
            self.on_card_click(&title);
        }
    }

    fn render_queue(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            widgets::h2(ui, "Queue");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("Clear queue")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.set_status("Nothing queued.");
                }
                ui.checkbox(&mut self.settings.infinite, "∞ Infinite")
                    .on_hover_text("Keep playing similar songs when the queue runs out");
            });
        });
        ui.add_space(8.0);
        widgets::hint(ui, "Nothing queued.");
    }

    fn render_lyrics(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            widgets::h2(ui, "Lyrics");
            ui.add_space(4.0);
            widgets::hint(ui, "Pick a track → Lyrics.");
        });
    }

    fn render_settings(&mut self, ui: &mut Ui) {
        widgets::h2(ui, "Settings");
        ui.add_space(8.0);

        widgets::h3(ui, "Playback");
        widgets::settings_card(ui, |ui| {
            ui.label("Playback is in-process (no browser engine).");
        });

        ui.add_space(12.0);
        widgets::h3(ui, "Display");
        widgets::settings_card(ui, |ui| {
            ui.checkbox(&mut self.settings.fs_lyrics, "lyrics in fullscreen");
            ui.checkbox(&mut self.settings.lyrics_focus, "focused lyrics (±2)");
            combo(
                ui,
                "fs-layout",
                &mut self.settings.fs_layout,
                "Fullscreen layout",
                &[
                    (FsLayout::Vertical, "vertical"),
                    (FsLayout::Horizontal, "horizontal"),
                ],
            );
            ui.checkbox(&mut self.settings.native_fs, "native fullscreen")
                .on_hover_text(
                    "True fullscreen hides window decorations but aborts the app on some \
                 GPU/compositor combos — the overlay covers the screen either way",
                );
        });

        ui.add_space(12.0);
        widgets::h3(ui, "Immersive fullscreen");
        widgets::settings_card(ui, |ui| {
            ui.checkbox(
                &mut self.settings.immersive,
                "immersive fullscreen (default, uncheck for classic)",
            );
            combo(
                ui,
                "im-layout",
                &mut self.settings.im_layout,
                "Immersive layout",
                &[
                    (ImLayout::Split, "split (cover + lyrics)"),
                    (ImLayout::Overlay, "overlay (lyrics over background)"),
                ],
            );
            combo(
                ui,
                "im-bg",
                &mut self.settings.im_bg,
                "Background",
                &[
                    (ImBg::Flow, "immersive (old)"),
                    (ImBg::Static, "blurred still cover"),
                    (ImBg::Motion, "animated cover"),
                    (ImBg::Dynamic, "immersive"),
                    (ImBg::Gradient, "colors only"),
                ],
            );
            ui.checkbox(&mut self.settings.motion_covers, "animated covers (video)");
            combo(
                ui,
                "im-cover",
                &mut self.settings.im_cover,
                "Foreground cover",
                &[
                    (ImCover::Animated, "animated"),
                    (ImCover::Still, "still"),
                    (ImCover::Hidden, "hidden"),
                ],
            );
            ui.horizontal(|ui| {
                ui.label("Blur");
                ui.add(
                    egui::Slider::new(&mut self.settings.im_blur, 0.0..=40.0)
                        .fixed_decimals(0)
                        .show_value(false),
                );
                ui.label(
                    RichText::new(format!("{}", self.settings.im_blur.round() as i32))
                        .size(12.0)
                        .color(c::DIM),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Dim");
                ui.add(
                    egui::Slider::new(&mut self.settings.im_dim, 0.0..=90.0)
                        .fixed_decimals(0)
                        .show_value(false),
                );
                ui.label(
                    RichText::new(format!("{}", self.settings.im_dim.round() as i32))
                        .size(12.0)
                        .color(c::DIM),
                );
            });
        });

        ui.add_space(12.0);
        widgets::h3(ui, "Notifications");
        widgets::settings_card(ui, |ui| {
            ui.checkbox(
                &mut self.settings.notifications,
                "track-change notifications",
            );
            ui.label(
                RichText::new(
                    "Shows a desktop notification with cover art when the track changes \
                     (off by default).",
                )
                .size(13.0)
                .color(c::DIM),
            );
        });

        ui.add_space(12.0);
        widgets::h3(ui, "Discord");
        widgets::settings_card(ui, |ui| {
            ui.checkbox(&mut self.settings.discord, "show Discord status");
            ui.horizontal(|ui| {
                ui.label("App ID");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.discord_app_id)
                        .hint_text("built-in Sonora app")
                        .desired_width(240.0),
                )
                .on_hover_text(
                    "Optional: paste your own Discord application ID to use a personal app \
                     instead of the built-in one",
                );
            });
            ui.label(
                RichText::new(
                    "Shows the playing song on Discord while music plays (leave empty for \
                     the built-in app). Needs the Discord desktop app running.",
                )
                .size(13.0)
                .color(c::DIM),
            );
        });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            widgets::h3(ui, "Account");
            ui.label(
                RichText::new("(token source checked at sign-in)")
                    .size(12.0)
                    .color(c::DIM),
            );
        });
        let auth_tip = "wired to the backend in the auth phase";
        widgets::settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(false, |ui| {
                    ui.button("Sign in with Apple Music")
                        .on_hover_text(auth_tip);
                    ui.button("Cancel")
                        .on_hover_text("Abort a pending browser sign-in");
                    ui.button("Log out")
                        .on_hover_text("Stops playback and clears the queue");
                });
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Not signed in.").size(13.0).color(c::DIM));
            ui.add_space(4.0);
            ui.checkbox(&mut self.settings.advanced_auth, "Advanced login")
                .on_hover_text("Show the manual token-paste fallback");

            if self.settings.advanced_auth {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(false, |ui| {
                        ui.button("Get authorize URL").on_hover_text(auth_tip);
                    });
                });
                ui.horizontal(|ui| {
                    ui.label("MUT");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.mut_input)
                            .hint_text("paste Music-User-Token")
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(false, |ui| {
                        ui.button("Save token").on_hover_text(auth_tip);
                    });
                });
                ui.horizontal(|ui| {
                    ui.label("Redirect URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.auth_url_input)
                            .hint_text("…or paste authorize redirect URL")
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(false, |ui| {
                        ui.button("Save from URL").on_hover_text(auth_tip);
                    });
                });
            }
        });
    }
}
