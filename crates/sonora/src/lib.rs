//! Sonora UI shell (egui/eframe).
//!
//! This crate holds the whole frontend: theming, widgets, views and the
//! application state. Backend services (auth, playback, API) live in the
//! `shell` crate and are wired in as phases land.

pub mod app;
pub mod fonts;
pub mod icons;
pub mod theme;
pub mod views;
pub mod widgets;
