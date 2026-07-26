//! Central palette + egui styling for MulVie.
//!
//! Design brief: "decent, no-nonsense, silver and dark blue".
//! The canvas behind photos is a deep near-black navy so images pop and
//! letterboxing is unobtrusive; chrome is brushed silver with a steel-blue
//! accent.

use eframe::egui::{self, Color32, Context, Rounding, Stroke};

// --- Canvas / panes -------------------------------------------------------
/// Deep navy used behind photos (letterbox areas).
pub const CANVAS: Color32 = Color32::from_rgb(0x0D, 0x14, 0x20);
/// Slightly lighter navy for panels / popups.
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x20, 0x33);
pub const PANEL_STRONG: Color32 = Color32::from_rgb(0x1C, 0x2A, 0x42);

// --- Silver chrome --------------------------------------------------------
pub const SILVER: Color32 = Color32::from_rgb(0xC8, 0xCF, 0xDA);
pub const HEADER_BG: Color32 = Color32::from_rgb(0x20, 0x2E, 0x45);

// --- Accents --------------------------------------------------------------
pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x82, 0xD3);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x2E, 0x4A, 0x72);

// --- Dividers -------------------------------------------------------------
pub const DIVIDER: Color32 = Color32::from_rgb(0x2C, 0x3C, 0x59);
pub const DIVIDER_HOVER: Color32 = ACCENT;

/// Faint ink for empty-state hints.
pub const HINT: Color32 = Color32::from_rgb(0x5A, 0x67, 0x7C);

/// Black ink for the icon buttons on the glassy (acrylic) context menus,
/// where silver washes out.
pub const MENU_INK: Color32 = Color32::BLACK;

/// Dark app-blue ink for question text on the glassy menu/popup surfaces.
pub const INK_BLUE: Color32 = Color32::from_rgb(0x2E, 0x4A, 0x72);

/// Bright silvery-white for text that must pop over glass/video.
pub const BRIGHT: Color32 = Color32::from_rgb(0xE9, 0xEE, 0xF7);

/// Apply the global egui style once at startup.
pub fn apply(ctx: &Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = CANVAS;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0_f32, ACCENT_DIM);
    visuals.window_rounding = Rounding::same(6.0);
    visuals.override_text_color = Some(SILVER);
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.0_f32,SILVER);
    visuals.hyperlink_color = ACCENT;

    // Popups / context menus.
    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.inactive.bg_fill = PANEL_STRONG;
    visuals.widgets.inactive.weak_bg_fill = PANEL_STRONG;
    visuals.widgets.hovered.bg_fill = ACCENT_DIM;
    visuals.widgets.hovered.weak_bg_fill = ACCENT_DIM;
    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32,SILVER);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32,Color32::WHITE);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32,Color32::WHITE);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.menu_margin = egui::Margin::same(6.0);
    ctx.set_style(style);
}
