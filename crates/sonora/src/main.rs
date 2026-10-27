//! Sonora binary — eframe bootstrap.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

const ICON_PNG: &[u8] = include_bytes!("../icons/128x128.png");

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sonora")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([900.0, 600.0])
            .with_icon(load_icon()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    // Boot the shared backend (tokio runtime + shell state + media
    // session) before the window so the first frame already polls.
    let backend = sonora::backend::Backend::new();

    eframe::run_native(
        "com.xmreur.sonora",
        options,
        Box::new(|cc| {
            sonora::theme::apply(&cc.egui_ctx);
            Ok(Box::new(sonora::app::SonoraApp::new(backend)))
        }),
    )
}

fn load_icon() -> egui::IconData {
    match image::load_from_memory(ICON_PNG) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(err) => {
            log::warn!("window icon decode failed: {err}");
            egui::IconData::default()
        }
    }
}
