//! Sonora UI shell (egui/eframe).
//!
//! This crate holds the whole frontend: theming, widgets, views and the
//! application state. Backend services (auth, playback, API) live in the
//! `shell` crate and are wired in as phases land.

pub mod app;
pub mod artwork;
pub mod backend;
pub mod fonts;
pub mod icons;
pub mod immersive;
pub mod kawarp;
pub mod lyrics;
pub mod motion;
pub mod player;
pub mod store;
pub mod theme;
#[cfg(test)]
mod ui_smoke;
pub mod views;
pub mod widgets;
