//! Read-only PDF viewing via pdfium (dynamically loaded at runtime).
//!
//! pdfium.dll is loaded next to the exe if present; if it's missing, PDF
//! viewing is simply unavailable and the rest of MulVie runs normally.
//! Pages are rendered to RGBA bitmaps on demand and uploaded as egui textures;
//! we re-render only when the page, zoom, or pane size changes.

use std::path::{Path, PathBuf};

use eframe::egui::{self, ColorImage, Pos2, Rect, TextureHandle, Vec2};
use pdfium_render::prelude::*;

/// Bind to a bundled pdfium (next to the exe: `pdfium.dll` / `libpdfium.so`),
/// else the system library.
pub fn init() -> Option<Pdfium> {
    let bindings = std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.to_path_buf()))
        .and_then(|dir| {
            Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir))
                .or_else(|_| Pdfium::bind_to_system_library())
                .ok()
        });
    // Same startup breadcrumb as the libmpv probe in main() — one console
    // line saying whether PDF viewing is active this run.
    eprintln!("[MulVie] pdfium available: {}", bindings.is_some());
    Some(Pdfium::new(bindings?))
}

pub fn page_count(pdfium: &Pdfium, path: &Path) -> Option<u16> {
    let doc = pdfium.load_pdf_from_file(path, None).ok()?;
    Some(doc.pages().len() as u16)
}

/// Render `page` (0-based) so it fits `pane_w x pane_h` physical pixels, times
/// `zoom`, rotated by `rotation` clockwise quarter-turns. Returns an RGBA image.
pub fn render_page(
    pdfium: &Pdfium,
    path: &Path,
    page: u16,
    pane_w: f32,
    pane_h: f32,
    zoom: f32,
    rotation: u8,
) -> Option<ColorImage> {
    let doc = pdfium.load_pdf_from_file(path, None).ok()?;
    let pages = doc.pages();
    let p = pages.get(page as i32).ok()?;
    let pw = p.width().value.max(1.0);
    let ph = p.height().value.max(1.0);
    // A 90°/270° turn swaps which page axis must fit which pane axis.
    let fit = if rotation % 2 == 1 {
        (pane_w / ph).min(pane_h / pw)
    } else {
        (pane_w / pw).min(pane_h / ph)
    };
    let scale = (fit * zoom).clamp(0.1, 40.0);

    let rot = match rotation % 4 {
        1 => PdfPageRenderRotation::Degrees90,
        2 => PdfPageRenderRotation::Degrees180,
        3 => PdfPageRenderRotation::Degrees270,
        _ => PdfPageRenderRotation::None,
    };
    let cfg = PdfRenderConfig::new()
        .scale_page_by_factor(scale)
        .rotate(rot, false)
        .set_maximum_width(10000)
        .set_maximum_height(10000);
    let bmp = p.render_with_config(&cfg).ok()?;
    let w = bmp.width() as usize;
    let h = bmp.height() as usize;
    let rgba = bmp.as_rgba_bytes();
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return None;
    }
    Some(ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

/// Per-pane PDF viewer state.
pub struct PdfView {
    pub path: PathBuf,
    pub page_count: u16,
    pub page: u16,
    pub zoom: f32,
    pub pan: Vec2,
    pub tex: Option<TextureHandle>,
    /// Texture size in points (physical px / pixels_per_point).
    pub tex_size: Vec2,
    /// Clockwise quarter-turns (0..4) applied at render time.
    pub rotation: u8,
    /// Cache key of the last render:
    /// (page, zoom*100, pane_w_px/32, pane_h_px/32, rotation).
    pub rendered: Option<(u16, i32, i32, i32, u8)>,
}

impl PdfView {
    pub fn new(path: PathBuf, page_count: u16) -> Self {
        Self {
            path,
            page_count,
            page: 0,
            zoom: 1.0,
            pan: Vec2::ZERO,
            tex: None,
            tex_size: Vec2::ZERO,
            rotation: 0,
            rendered: None,
        }
    }

    /// Rotate the view 90° clockwise; the page re-renders at the new fit.
    pub fn rotate_cw(&mut self) {
        self.rotation = (self.rotation + 1) % 4;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    /// Rotate the view 90° counter-clockwise.
    pub fn rotate_ccw(&mut self) {
        self.rotation = (self.rotation + 3) % 4;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    pub fn next_page(&mut self) {
        if self.page + 1 < self.page_count {
            self.page += 1;
            self.pan = Vec2::ZERO;
        }
    }

    pub fn prev_page(&mut self) {
        if self.page > 0 {
            self.page -= 1;
            self.pan = Vec2::ZERO;
        }
    }

    pub fn set_page(&mut self, page: u16) {
        let page = page.min(self.page_count.saturating_sub(1));
        if page != self.page {
            self.page = page;
            self.pan = Vec2::ZERO;
        }
    }

    pub fn reset_zoom(&mut self) {
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    /// Cursor-anchored zoom (Ctrl+scroll). The rendered texture scales with
    /// zoom, so this mirrors the image zoom-to-cursor math.
    pub fn zoom_at(&mut self, rect: Rect, cursor: Pos2, factor: f32) {
        let old = self.zoom;
        let new = (old * factor).clamp(1.0, 8.0);
        if (new - old).abs() < f32::EPSILON {
            return;
        }
        let o = rect.center().to_vec2();
        let c = cursor.to_vec2();
        self.pan = (c - o) - ((c - o) - self.pan) * (new / old);
        self.zoom = new;
        if self.zoom <= 1.0001 {
            self.pan = Vec2::ZERO;
        }
        self.clamp_pan(rect);
    }

    pub fn drag_pan(&mut self, rect: Rect, delta: Vec2) {
        self.pan += delta;
        self.clamp_pan(rect);
    }

    pub fn clamp_pan(&mut self, rect: Rect) {
        let mx = ((self.tex_size.x - rect.width()) * 0.5).max(0.0);
        let my = ((self.tex_size.y - rect.height()) * 0.5).max(0.0);
        self.pan.x = self.pan.x.clamp(-mx, mx);
        self.pan.y = self.pan.y.clamp(-my, my);
    }
}

/// Draw the current page's texture (centred + pan), clipped to the pane.
pub fn draw(painter: &egui::Painter, rect: Rect, view: &PdfView) {
    if let Some(tex) = &view.tex {
        let r = Rect::from_center_size(rect.center() + view.pan, view.tex_size);
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        painter.image(tex.id(), r, uv, egui::Color32::WHITE);
    }
}
