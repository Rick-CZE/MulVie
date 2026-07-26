// MulVie — a portable, no-install, offline multi-image viewer.
// Hide the console window in release builds (keep it in debug for logs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod file_ops;
mod fonts;
mod gallery;
mod image_store;
mod layout;
mod list_manager;
mod os;
mod pane;
mod pdf;
mod theme;
mod thumbs;
mod video;
mod widgets;

use eframe::egui;
use std::sync::Arc;

/// Decode the embedded window/taskbar icon (the blue "M").
fn load_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .ok()?
        .to_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    // "Open with MulVie": a supported media file passed as the first argument
    // (e.g. via Windows file association or drag-onto-exe).
    let open_file: Option<std::path::PathBuf> = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file() && gallery::is_media(p));

    // Open-with hand-over: if another MulVie is already running and we were
    // asked to open a file, atomically hand the path to it and quit — the
    // running window shows the file. If the hand-off write fails (e.g. the
    // stick is read-only) we DON'T exit; we fall through and open our own
    // window so the file is still shown.
    if os::another_instance_running() {
        if let Some(f) = &open_file {
            if config::write_inbox(f) {
                return Ok(());
            }
        }
        // No file to hand over, or the hand-off failed: open a normal window.
    }

    eprintln!("[MulVie] libmpv available: {}", video::probe());

    let cfg = config::load();
    let size = cfg.window_size.unwrap_or([1280.0, 800.0]);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("MulVie")
        .with_inner_size(size)
        .with_min_inner_size([720.0, 460.0])
        .with_decorations(false) // custom in-app titlebar (drawn in the header)
        // Frosted-glass canvas — Windows only. On Linux there is no acrylic,
        // and requesting an alpha-capable GL config can make glutin's GLX
        // picker fail at startup (it hard-filters on transparency).
        .with_transparent(cfg!(windows));
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    if cfg.maximized {
        viewport = viewport.with_maximized(true);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        vsync: true,
        ..Default::default()
    };

    eframe::run_native(
        "MulVie",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::MulVieApp::new(cc, cfg, open_file)))),
    )
}
