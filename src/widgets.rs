//! Small custom-drawn chrome widgets: modern icon buttons and the M badge.
//! Icons are drawn as crisp vector shapes so they scale and match the theme.

use eframe::egui::{
    pos2, vec2, Align, Align2, Color32, FontId, Layout, Painter, Pos2, Rect, Response, Rounding,
    Sense, Shape, Stroke, Ui, Vec2,
};

use crate::theme;

#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)] // Pause/Play/Speaker* are used by the video chrome (Phase 2)
pub enum Icon {
    Fullscreen,
    DividersShown,
    DividersHidden,
    Pause,
    Play,
    Stop,
    Speaker,
    SpeakerMuted,
    Frost,
    Clear,
    Loop,
    NavPrev,
    NavNext,
    Minimize,
    Maximize,
    Restore,
    Close,
    FileManage,
    ListManage,
    Refresh,
    Up,
    Down,
    RotateCw,
    RotateCcw,
    MouseHide,
    Settings,
    FourPanels,
    Library,
}

/// The "M" badge at the left of the header. Draws the real app icon (the same
/// art as the taskbar icon) when its texture is available; falls back to the
/// old painted badge otherwise.
pub fn logo(ui: &mut Ui, icon: Option<&eframe::egui::TextureHandle>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
    let p = ui.painter();
    if let Some(tex) = icon {
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        p.image(tex.id(), rect, uv, Color32::WHITE);
    } else {
        p.rect_filled(rect, Rounding::same(6.0), theme::ACCENT);
        p.rect_stroke(
            rect,
            Rounding::same(6.0),
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 40)),
        );
        p.text(
            rect.center() + vec2(0.0, -0.5),
            Align2::CENTER_CENTER,
            "M",
            FontId::proportional(16.0),
            Color32::WHITE,
        );
    }
    resp
}

pub fn icon_button(ui: &mut Ui, icon: Icon, active: bool, tip: &str) -> Response {
    icon_button_sized(ui, icon, active, tip, vec2(30.0, 26.0))
}

#[allow(dead_code)] // used by the video chrome (Phase 2)
pub fn small_icon_button(ui: &mut Ui, icon: Icon, active: bool, tip: &str) -> Response {
    icon_button_sized(ui, icon, active, tip, vec2(25.0, 23.0))
}

/// The Single ↔ MultiView slide switch: a box-"1" glyph on the left, a
/// box-"4" glyph on the right, a neutral track between them whose knob sits at
/// the active side. The knob POSITION is the whole indicator — the control
/// keeps the same colours in both states (hover only brightens the ink).
pub fn view_toggle(ui: &mut Ui, multi: bool, tip: &str) -> Response {
    // click_and_drag so a drag begun here is absorbed instead of moving the
    // window (matches every other header button).
    let (rect, resp) = ui.allocate_exact_size(vec2(66.0, 26.0), Sense::click_and_drag());
    let hovered = resp.hovered();
    let col = if hovered { Color32::WHITE } else { theme::SILVER };
    let p = ui.painter();
    let stroke = Stroke::new(1.4_f32, col);
    let c = rect.center();

    let box_sz = 13.0;
    let lbox = Rect::from_center_size(pos2(rect.left() + box_sz * 0.5 + 1.0, c.y), vec2(box_sz, box_sz));
    let rbox = Rect::from_center_size(pos2(rect.right() - box_sz * 0.5 - 1.0, c.y), vec2(box_sz, box_sz));
    p.rect_stroke(lbox, Rounding::same(3.0), stroke);
    p.rect_stroke(rbox, Rounding::same(3.0), stroke);
    p.text(lbox.center(), Align2::CENTER_CENTER, "1", FontId::proportional(9.5), col);
    p.text(rbox.center(), Align2::CENTER_CENTER, "4", FontId::proportional(9.5), col);

    let track = Rect::from_center_size(c, vec2(rect.width() - 2.0 * (box_sz + 7.0), 12.0));
    p.rect_filled(track, Rounding::same(6.0), theme::PANEL_STRONG);
    p.rect_stroke(track, Rounding::same(6.0), Stroke::new(1.0_f32, theme::ACCENT_DIM));
    let knob_x = if multi { track.right() - 6.0 } else { track.left() + 6.0 };
    p.circle_filled(pos2(knob_x, c.y), 4.0, col);

    resp.on_hover_text(tip)
}

/// An icon button for the glassy context menus: black ink so it reads on the
/// acrylic (silver washes out there), the usual accent pill + white icon on
/// hover, dim when disabled (ignores clicks).
pub fn menu_icon_button(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool) -> Response {
    let sense = if enabled {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), sense);
    let hovered = enabled && resp.hovered();
    let bg = if hovered {
        theme::ACCENT_DIM
    } else {
        Color32::TRANSPARENT
    };
    {
        let p = ui.painter();
        p.rect_filled(rect, Rounding::same(6.0), bg);
        let col = if !enabled {
            theme::HINT
        } else if hovered {
            Color32::WHITE
        } else {
            theme::MENU_INK
        };
        draw_icon(p, rect, icon, col);
    }
    resp.on_hover_text(tip)
}

/// Shared width for the content right-click menus. Sized to comfortably clear
/// the widest menu item ("Switch panel content" ≈ 133pt) while leaving the top
/// row enough room to keep a small gap between the left-hand nav pair and the
/// right-aligned rotate pair (each pair ≈ 68pt). Kept snug so the menus stay
/// sleek. See the `diag_widths` measurements this was derived from.
pub const MENU_WIDTH: f32 = 152.0;

/// What [`nav_rotate_row`] drew and what the user clicked: the four click
/// flags the caller acts on, plus the button rects (used by the layout
/// regression test to assert nav-left / rotate-right-aligned).
pub struct NavRotateRow {
    pub prev: bool,
    pub next: bool,
    pub cw: bool,
    pub ccw: bool,
    pub prev_rect: Rect,
    pub next_rect: Rect,
    pub cw_rect: Rect,
    pub ccw_rect: Rect,
}

impl Default for NavRotateRow {
    fn default() -> Self {
        // egui::Rect has no Default; rects are overwritten with the real
        // button geometry as the row is built.
        Self {
            prev: false,
            next: false,
            cw: false,
            ccw: false,
            prev_rect: Rect::NOTHING,
            next_rect: Rect::NOTHING,
            cw_rect: Rect::NOTHING,
            ccw_rect: Rect::NOTHING,
        }
    }
}

/// The shared top row of every content context menu: prev/next file on the
/// left, rotate cw/ccw aligned to the right edge. The caller acts on the
/// returned clicks — nav should close the menu; rotate deliberately should
/// not, so it can be clicked repeatedly (90° per click).
pub fn nav_rotate_row(ui: &mut Ui, enabled: bool) -> NavRotateRow {
    let mut out = NavRotateRow::default();
    ui.horizontal(|ui| {
        // Pin the row to MENU_WIDTH. Without this the right-to-left rotate
        // group expands greedily to fill the menu's default available width,
        // which balloons every content menu wider than its text needs.
        ui.set_min_width(MENU_WIDTH);
        ui.set_max_width(MENU_WIDTH);
        let prev = menu_icon_button(ui, Icon::NavPrev, "Previous file  (Shift+F)", enabled);
        let next = menu_icon_button(ui, Icon::NavNext, "Next file  (F)", enabled);
        out.prev = prev.clicked();
        out.next = next.clicked();
        out.prev_rect = prev.rect;
        out.next_rect = next.rect;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // right_to_left: the first widget added sits at the far right.
            let ccw = menu_icon_button(
                ui,
                Icon::RotateCcw,
                "Rotate counter-clockwise  (Shift+R)",
                enabled,
            );
            let cw = menu_icon_button(ui, Icon::RotateCw, "Rotate clockwise  (R)", enabled);
            out.ccw = ccw.clicked();
            out.cw = cw.clicked();
            out.ccw_rect = ccw.rect;
            out.cw_rect = cw.rect;
        });
    });
    out
}

/// Clamp a playback-speed multiplier to MulVie's supported range (1%..500%).
/// NaN (a typed "nan" that parsed) falls back to 100% — `f64::clamp` passes NaN
/// through, and a NaN speed would freeze a GIF and spin the repaint loop.
/// (±inf clamp sanely to 5.0 / 0.01, so only NaN needs the guard.)
pub fn clamp_speed(mult: f64) -> f64 {
    if mult.is_nan() {
        return 1.0;
    }
    mult.clamp(0.01, 5.0)
}

/// The shared "Speed" submenu body (video + GIF menus): percentage presets
/// plus a custom field (1..500 %). Returns the chosen multiplier (1.0 = 100%)
/// when the user picks one, closing the menu. `current` marks the active
/// preset; `input` is the caller-owned text buffer for the custom field.
pub fn speed_menu(ui: &mut Ui, current: f64, input: &mut String) -> Option<f64> {
    let mut chosen: Option<f64> = None;
    let cur_pct = (current * 100.0).round() as i32;
    for pct in [90_i32, 100, 110, 150] {
        if ui.radio(cur_pct == pct, format!("{pct}%")).clicked() {
            chosen = Some(pct as f64 / 100.0);
            ui.close_menu();
        }
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Custom");
        // Visible accent frame on the glassy menu (as the custom-loop field).
        ui.scope(|ui| {
            let w = &mut ui.visuals_mut().widgets;
            w.inactive.bg_stroke = Stroke::new(1.0_f32, theme::ACCENT);
            w.hovered.bg_stroke = Stroke::new(1.0_f32, theme::ACCENT);
            ui.add(eframe::egui::TextEdit::singleline(input).desired_width(42.0));
        });
        ui.label("%");
        if ui.button("Set").clicked() {
            if let Ok(pct) = input.trim().parse::<f64>() {
                chosen = Some(clamp_speed(pct / 100.0));
            }
            ui.close_menu();
        }
    });
    chosen
}

/// A modern rounded text button in the app palette: accent-filled when
/// `primary`, glassy dark otherwise, with a subtle top gloss and hover states.
/// Shared by the rename window and in-app dialogs.
pub fn text_button(ui: &mut Ui, label: &str, primary: bool, enabled: bool) -> Response {
    let font = FontId::proportional(15.0);
    let text_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::WHITE)
        .size()
        .x;
    let size = vec2((text_w + 30.0).max(96.0), 32.0);
    text_button_sized(ui, label, primary, enabled, size, 15.0)
}

/// Same look as [`text_button`] but at a caller-chosen size and glyph size —
/// used for compact inline buttons (e.g. the Library "+" save button, which is
/// a small button but wants a big "+").
pub fn text_button_sized(
    ui: &mut Ui,
    label: &str,
    primary: bool,
    enabled: bool,
    size: Vec2,
    glyph_size: f32,
) -> Response {
    let font = FontId::proportional(glyph_size);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hovered = enabled && resp.hovered();
    let fill = match (primary, enabled, hovered) {
        (true, true, true) => Color32::from_rgb(0x5E, 0x93, 0xE0),
        (true, true, false) => theme::ACCENT,
        (true, false, _) => theme::ACCENT_DIM.gamma_multiply(0.6),
        (false, _, true) => theme::ACCENT_DIM,
        (false, _, false) => theme::PANEL_STRONG,
    };
    let p = ui.painter();
    let rounding = Rounding::same(8.0);
    p.rect_filled(rect, rounding, fill);
    p.rect_stroke(
        rect,
        rounding,
        Stroke::new(
            1.0_f32,
            Color32::from_rgba_unmultiplied(255, 255, 255, if primary { 60 } else { 28 }),
        ),
    );
    // Top-edge gloss.
    p.line_segment(
        [
            rect.left_top() + vec2(6.0, 1.5),
            rect.right_top() + vec2(-6.0, 1.5),
        ],
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 45)),
    );
    let text_col = if !enabled {
        theme::HINT
    } else if primary || hovered {
        Color32::WHITE
    } else {
        theme::BRIGHT
    };
    p.text(rect.center(), Align2::CENTER_CENTER, label, font, text_col);
    resp
}

/// A titlebar window control (minimize / maximize / close). `danger` gives the
/// close button a red hover, matching the OS convention.
pub fn window_button(ui: &mut Ui, icon: Icon, danger: bool, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click_and_drag());
    let hovered = resp.hovered();
    let bg = if hovered {
        if danger {
            Color32::from_rgb(0xC0, 0x3A, 0x2E)
        } else {
            theme::ACCENT_DIM
        }
    } else {
        Color32::TRANSPARENT
    };
    {
        let p = ui.painter();
        p.rect_filled(rect, Rounding::same(6.0), bg);
        let col = if hovered { Color32::WHITE } else { theme::SILVER };
        draw_icon(p, rect, icon, col);
    }
    resp.on_hover_text(tip)
}

/// A small numbered audio button. Highlighted (accent) when that pane's audio
/// is ON; dim with a slash when muted.
pub fn mute_button(ui: &mut Ui, digit: u8, muted: bool, tip: &str) -> Response {
    // click_and_drag (not click) so a drag begun on this button is absorbed
    // here instead of starting a titlebar window-move.
    let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 22.0), Sense::click_and_drag());
    let hovered = resp.hovered();
    let bg = if !muted {
        theme::ACCENT
    } else if hovered {
        theme::ACCENT_DIM
    } else {
        Color32::TRANSPARENT
    };
    {
        let p = ui.painter();
        p.rect_filled(rect, Rounding::same(6.0), bg);
        let col = if !muted || hovered {
            Color32::WHITE
        } else {
            theme::SILVER
        };
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            digit.to_string(),
            FontId::proportional(13.0),
            col,
        );
        if muted {
            p.line_segment(
                [
                    pos2(rect.left() + 6.0, rect.top() + 5.0),
                    pos2(rect.right() - 6.0, rect.bottom() - 5.0),
                ],
                Stroke::new(1.7_f32, col),
            );
        }
    }
    resp.on_hover_text(tip)
}

fn icon_button_sized(ui: &mut Ui, icon: Icon, active: bool, tip: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let hovered = resp.hovered();
    let bg = if active {
        theme::ACCENT
    } else if hovered {
        theme::ACCENT_DIM
    } else {
        Color32::TRANSPARENT
    };
    {
        let p = ui.painter();
        p.rect_filled(rect, Rounding::same(6.0), bg);
        let col = if active || hovered {
            Color32::WHITE
        } else {
            theme::SILVER
        };
        draw_icon(p, rect, icon, col);
    }
    resp.on_hover_text(tip)
}

fn draw_icon(p: &Painter, rect: Rect, icon: Icon, col: Color32) {
    let c = rect.center();
    let s = 6.5_f32;
    let stroke = Stroke::new(1.8_f32,col);
    match icon {
        Icon::Fullscreen => {
            let corners = [
                (pos2(c.x - s, c.y - s), vec2(1.0, 0.0), vec2(0.0, 1.0)),
                (pos2(c.x + s, c.y - s), vec2(-1.0, 0.0), vec2(0.0, 1.0)),
                (pos2(c.x + s, c.y + s), vec2(-1.0, 0.0), vec2(0.0, -1.0)),
                (pos2(c.x - s, c.y + s), vec2(1.0, 0.0), vec2(0.0, -1.0)),
            ];
            let l = 5.0;
            for (corner, dx, dy) in corners {
                p.line_segment([corner, corner + dx * l], stroke);
                p.line_segment([corner, corner + dy * l], stroke);
            }
        }
        Icon::DividersShown => {
            // Lines visible (default state): a plain solid cross.
            let r = Rect::from_center_size(c, vec2(2.0 * s, 2.0 * s));
            p.line_segment([pos2(c.x, r.top()), pos2(c.x, r.bottom())], stroke);
            p.line_segment([pos2(r.left(), c.y), pos2(r.right(), c.y)], stroke);
        }
        Icon::DividersHidden => {
            // Lines hidden (pressed state, drawn on the accent pill): the same
            // cross, thinner.
            let r = Rect::from_center_size(c, vec2(2.0 * s, 2.0 * s));
            let thin = Stroke::new(1.0_f32, col);
            p.line_segment([pos2(c.x, r.top()), pos2(c.x, r.bottom())], thin);
            p.line_segment([pos2(r.left(), c.y), pos2(r.right(), c.y)], thin);
        }
        Icon::Settings => {
            // A cog: a ring of radial teeth around a hub.
            let teeth = 8;
            for k in 0..teeth {
                let ang = k as f32 / teeth as f32 * std::f32::consts::TAU;
                let (dx, dy) = (ang.cos(), ang.sin());
                p.line_segment(
                    [
                        pos2(c.x + dx * s * 0.68, c.y + dy * s * 0.68),
                        pos2(c.x + dx * s * 1.15, c.y + dy * s * 1.15),
                    ],
                    Stroke::new(2.1_f32, col),
                );
            }
            p.circle_stroke(c, s * 0.62, Stroke::new(1.6_f32, col));
            p.circle_filled(c, s * 0.2, col);
        }
        Icon::FourPanels => {
            // A 2×2 grid: a boxed cross.
            let r = Rect::from_center_size(c, vec2(2.0 * s, 2.0 * s));
            p.rect_stroke(r, Rounding::same(1.5), stroke);
            p.line_segment([pos2(c.x, r.top()), pos2(c.x, r.bottom())], stroke);
            p.line_segment([pos2(r.left(), c.y), pos2(r.right(), c.y)], stroke);
        }
        Icon::Library => {
            // A row of book spines, one leaning.
            let bs = Stroke::new(1.4_f32, col);
            let top = c.y - s;
            let bot = c.y + s;
            for bx in [c.x - s, c.x - s * 0.35, c.x + s * 0.3] {
                p.rect_stroke(
                    Rect::from_min_max(pos2(bx, top), pos2(bx + s * 0.5, bot)),
                    Rounding::same(0.5),
                    bs,
                );
            }
            // A leaning book on the right.
            let lean = [
                pos2(c.x + s * 0.95, bot),
                pos2(c.x + s * 1.3, bot),
                pos2(c.x + s * 0.75, top),
                pos2(c.x + s * 0.4, top),
            ];
            p.add(Shape::closed_line(lean.to_vec(), bs));
        }
        Icon::Pause => {
            let w = 2.6;
            let h = s * 1.7;
            p.rect_filled(
                Rect::from_center_size(pos2(c.x - 3.0, c.y), vec2(w, h)),
                Rounding::same(1.0),
                col,
            );
            p.rect_filled(
                Rect::from_center_size(pos2(c.x + 3.0, c.y), vec2(w, h)),
                Rounding::same(1.0),
                col,
            );
        }
        Icon::Play => {
            let h = s * 1.6;
            let pts = vec![
                pos2(c.x - 4.0, c.y - h * 0.5),
                pos2(c.x - 4.0, c.y + h * 0.5),
                pos2(c.x + 5.5, c.y),
            ];
            p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
        }
        Icon::Stop => {
            p.rect_filled(
                Rect::from_center_size(c, vec2(s * 1.6, s * 1.6)),
                Rounding::same(1.5),
                col,
            );
        }
        Icon::Speaker => {
            draw_speaker(p, c, col);
            let wv = Stroke::new(1.5_f32,col);
            p.line_segment([pos2(c.x + 3.5, c.y - 2.5), pos2(c.x + 6.0, c.y - 4.0)], wv);
            p.line_segment([pos2(c.x + 4.5, c.y), pos2(c.x + 7.5, c.y)], wv);
            p.line_segment([pos2(c.x + 3.5, c.y + 2.5), pos2(c.x + 6.0, c.y + 4.0)], wv);
        }
        Icon::SpeakerMuted => {
            draw_speaker(p, c, col);
            let sl = Stroke::new(1.7_f32,col);
            p.line_segment([pos2(c.x + 3.5, c.y - 3.5), pos2(c.x + 7.5, c.y + 3.5)], sl);
            p.line_segment([pos2(c.x + 7.5, c.y - 3.5), pos2(c.x + 3.5, c.y + 3.5)], sl);
        }
        Icon::Frost => {
            // Snowflake: three lines through the centre.
            for deg in [90.0_f32, 30.0, 150.0] {
                let a = deg.to_radians();
                let (dx, dy) = (a.cos() * s, a.sin() * s);
                p.line_segment([pos2(c.x - dx, c.y - dy), pos2(c.x + dx, c.y + dy)], stroke);
            }
        }
        Icon::Clear => {
            // Trash can.
            p.line_segment([pos2(c.x - s, c.y - s * 0.7), pos2(c.x + s, c.y - s * 0.7)], stroke);
            p.line_segment([pos2(c.x - 2.5, c.y - s * 0.7), pos2(c.x - 2.5, c.y - s * 1.1)], stroke);
            p.line_segment([pos2(c.x + 2.5, c.y - s * 0.7), pos2(c.x + 2.5, c.y - s * 1.1)], stroke);
            p.line_segment([pos2(c.x - 2.5, c.y - s * 1.1), pos2(c.x + 2.5, c.y - s * 1.1)], stroke);
            let body = Rect::from_min_max(
                pos2(c.x - s * 0.7, c.y - s * 0.5),
                pos2(c.x + s * 0.7, c.y + s),
            );
            p.rect_stroke(body, Rounding::same(1.5), stroke);
            let thin = Stroke::new(1.3_f32, col);
            p.line_segment([pos2(c.x - 2.2, c.y - s * 0.1), pos2(c.x - 2.2, c.y + s * 0.6)], thin);
            p.line_segment([pos2(c.x + 2.2, c.y - s * 0.1), pos2(c.x + 2.2, c.y + s * 0.6)], thin);
        }
        Icon::Loop => {
            // A circular arrow: an arc most of the way round with an arrowhead.
            let r = s * 0.95;
            let a0 = (-50.0_f32).to_radians();
            let a1 = (215.0_f32).to_radians();
            let n = 22;
            let mut pts = Vec::with_capacity(n + 1);
            for k in 0..=n {
                let a = a0 + (a1 - a0) * (k as f32 / n as f32);
                pts.push(pos2(c.x + a.cos() * r, c.y + a.sin() * r));
            }
            p.add(Shape::line(pts, stroke));
            // Arrowhead at the a1 end, pointing along the direction of travel.
            let dir = vec2(-a1.sin(), a1.cos()); // tangent, increasing-angle
            let nor = vec2(a1.cos(), a1.sin()); // radial
            let tip = pos2(c.x + a1.cos() * r, c.y + a1.sin() * r);
            let ah = 3.6;
            p.add(Shape::convex_polygon(
                vec![tip + dir * ah, tip + nor * ah * 0.85, tip - nor * ah * 0.85],
                col,
                Stroke::NONE,
            ));
        }
        Icon::NavPrev => {
            let h = s * 1.5;
            p.add(Shape::convex_polygon(
                vec![
                    pos2(c.x + 4.0, c.y - h * 0.5),
                    pos2(c.x + 4.0, c.y + h * 0.5),
                    pos2(c.x - 5.0, c.y),
                ],
                col,
                Stroke::NONE,
            ));
        }
        Icon::NavNext => {
            let h = s * 1.5;
            p.add(Shape::convex_polygon(
                vec![
                    pos2(c.x - 4.0, c.y - h * 0.5),
                    pos2(c.x - 4.0, c.y + h * 0.5),
                    pos2(c.x + 5.0, c.y),
                ],
                col,
                Stroke::NONE,
            ));
        }
        Icon::Minimize => {
            let thin = Stroke::new(1.5_f32, col);
            p.line_segment([pos2(c.x - s, c.y + s * 0.5), pos2(c.x + s, c.y + s * 0.5)], thin);
        }
        Icon::Maximize => {
            let d = s * 0.85;
            let thin = Stroke::new(1.4_f32, col);
            p.rect_stroke(
                Rect::from_center_size(c, vec2(2.0 * d, 2.0 * d)),
                Rounding::same(1.0),
                thin,
            );
        }
        Icon::Restore => {
            let d = s * 0.72;
            let thin = Stroke::new(1.3_f32, col);
            let front = Rect::from_min_max(pos2(c.x - d, c.y - d + 3.0), pos2(c.x + d - 3.0, c.y + d));
            // Back window: peek its top and right edges up-and-right of the front.
            let bx0 = front.left() + 3.0;
            let bx1 = front.right() + 3.0;
            let by = front.top() - 3.0;
            p.line_segment([pos2(bx0, by), pos2(bx1, by)], thin);
            p.line_segment([pos2(bx1, by), pos2(bx1, front.bottom() - 3.0)], thin);
            p.rect_stroke(front, Rounding::same(1.0), thin);
        }
        Icon::Close => {
            let d = s * 0.85;
            let thin = Stroke::new(1.5_f32, col);
            p.line_segment([pos2(c.x - d, c.y - d), pos2(c.x + d, c.y + d)], thin);
            p.line_segment([pos2(c.x + d, c.y - d), pos2(c.x - d, c.y + d)], thin);
        }
        Icon::FileManage => {
            // "Aa" — the common bulk-rename glyph.
            p.text(
                c,
                Align2::CENTER_CENTER,
                "Aa",
                FontId::proportional(13.0),
                col,
            );
        }
        Icon::ListManage => {
            // A playlist: three rows of bullet + line.
            let thin = Stroke::new(1.6_f32, col);
            for k in 0..3 {
                let y = c.y - 5.0 + k as f32 * 5.0;
                p.rect_filled(
                    Rect::from_center_size(pos2(c.x - 5.5, y), vec2(2.6, 2.6)),
                    Rounding::same(0.8),
                    col,
                );
                p.line_segment([pos2(c.x - 1.8, y), pos2(c.x + 6.5, y)], thin);
            }
        }
        Icon::Refresh => {
            // A circular refresh arrow (a near-full arc + arrowhead).
            let r = s * 0.9;
            let a0 = (30.0_f32).to_radians();
            let a1 = (310.0_f32).to_radians();
            let n = 24;
            let mut pts = Vec::with_capacity(n + 1);
            for k in 0..=n {
                let a = a0 + (a1 - a0) * (k as f32 / n as f32);
                pts.push(pos2(c.x + a.cos() * r, c.y - a.sin() * r));
            }
            let tip = pts[n];
            let prev = pts[n - 1];
            p.add(Shape::line(pts, stroke));
            let dir = (tip - prev).normalized();
            let nor = vec2(-dir.y, dir.x);
            let ah = 3.4;
            p.add(Shape::convex_polygon(
                vec![tip + dir * ah, tip + nor * ah * 0.9, tip - nor * ah * 0.9],
                col,
                Stroke::NONE,
            ));
        }
        Icon::Up => {
            let w = s * 1.4;
            p.add(Shape::convex_polygon(
                vec![
                    pos2(c.x - w * 0.5, c.y + 3.0),
                    pos2(c.x + w * 0.5, c.y + 3.0),
                    pos2(c.x, c.y - 4.0),
                ],
                col,
                Stroke::NONE,
            ));
        }
        Icon::Down => {
            let w = s * 1.4;
            p.add(Shape::convex_polygon(
                vec![
                    pos2(c.x - w * 0.5, c.y - 3.0),
                    pos2(c.x + w * 0.5, c.y - 3.0),
                    pos2(c.x, c.y + 4.0),
                ],
                col,
                Stroke::NONE,
            ));
        }
        Icon::RotateCw => arc_arrow(p, c, s * 0.9, false, col, stroke),
        Icon::RotateCcw => arc_arrow(p, c, s * 0.9, true, col, stroke),
        Icon::MouseHide => {
            // A PC mouse: a body rounded more at the top than the bottom (the
            // mouse silhouette) with a button-split line down the upper half.
            let w = s * 0.56;
            let h = s * 1.02;
            let body = Rect::from_center_size(c, vec2(2.0 * w, 2.0 * h));
            let rounding = Rounding {
                nw: w,
                ne: w,
                sw: w * 0.6,
                se: w * 0.6,
            };
            p.rect_stroke(body, rounding, stroke);
            p.line_segment(
                [pos2(c.x, c.y - h * 0.82), pos2(c.x, c.y - h * 0.12)],
                Stroke::new(1.5_f32, col),
            );
        }
    }
}

/// A circular arrow (most of a circle plus an arrowhead at its end); the
/// mirrored variant reads as counter-clockwise.
fn arc_arrow(p: &Painter, c: Pos2, r: f32, mirror: bool, col: Color32, stroke: Stroke) {
    let a0 = (-50.0_f32).to_radians();
    let a1 = (215.0_f32).to_radians();
    let n = 22;
    let mut pts = Vec::with_capacity(n + 1);
    for k in 0..=n {
        let a = a0 + (a1 - a0) * (k as f32 / n as f32);
        let x = if mirror { c.x - a.cos() * r } else { c.x + a.cos() * r };
        pts.push(pos2(x, c.y + a.sin() * r));
    }
    let tip = pts[n];
    let prev = pts[n - 1];
    p.add(Shape::line(pts, stroke));
    let dir = (tip - prev).normalized();
    let nor = vec2(-dir.y, dir.x);
    let ah = 3.6;
    p.add(Shape::convex_polygon(
        vec![tip + dir * ah, tip + nor * ah * 0.85, tip - nor * ah * 0.85],
        col,
        Stroke::NONE,
    ));
}

fn draw_speaker(p: &Painter, c: Pos2, col: Color32) {
    let bx = c.x - 5.5;
    p.rect_filled(
        Rect::from_min_max(pos2(bx, c.y - 2.0), pos2(bx + 2.5, c.y + 2.0)),
        Rounding::same(0.0),
        col,
    );
    let pts = vec![
        pos2(bx + 2.0, c.y - 2.0),
        pos2(bx + 2.0, c.y + 2.0),
        pos2(bx + 5.0, c.y + 4.5),
        pos2(bx + 5.0, c.y - 4.5),
    ];
    p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
}

#[cfg(test)]
mod tests {
    use super::{clamp_speed, nav_rotate_row};
    use eframe::egui::{self, pos2, vec2, Id, RawInput, Rect};

    /// Speed multiplier stays within MulVie's 1%..500% range.
    #[test]
    fn speed_clamps_to_supported_range() {
        assert_eq!(clamp_speed(1.0), 1.0);
        assert_eq!(clamp_speed(1.5), 1.5);
        assert_eq!(clamp_speed(5.0), 5.0);
        assert_eq!(clamp_speed(0.0), 0.01); // "0%" floors to 1%, never 0 (no div-by-zero)
        assert_eq!(clamp_speed(-3.0), 0.01);
        assert_eq!(clamp_speed(9.9), 5.0); // above 500% caps at 5x
        assert_eq!(clamp_speed(f64::NAN), 1.0); // typed "nan" falls back to 100%
        assert_eq!(clamp_speed(f64::INFINITY), 5.0); // inf caps at 5x
        assert_eq!(clamp_speed(f64::NEG_INFINITY), 0.01); // -inf floors at 1%
    }

    /// Build the real shared row inside a fixed-width area and return the
    /// button rects it drew. Two passes: egui's first frame is a sizing pass,
    /// so the second frame carries the settled geometry.
    fn row_rects(width: f32) -> super::NavRotateRow {
        let ctx = egui::Context::default();
        let mut out = super::NavRotateRow::default();
        for _ in 0..2 {
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 400.0))),
                ..Default::default()
            };
            let _ = ctx.run(raw, |ctx| {
                egui::Area::new(Id::new("test_nav_rotate_row"))
                    .fixed_pos(pos2(0.0, 0.0))
                    .constrain(false)
                    .show(ctx, |ui| {
                        // Mirror how the real context menus pin the row width.
                        ui.set_min_width(width);
                        ui.set_max_width(width);
                        out = nav_rotate_row(ui, true);
                    });
            });
        }
        out
    }

    /// Fixed layout requirement, guarded deterministically: the navigation
    /// arrows sit together at the LEFT of the row and the rotate pair is
    /// right-aligned to the row's right edge, with no overlap between the two
    /// groups. (Clickability of these buttons is exercised in production —
    /// they are the same `menu_icon_button` used across the menus.)
    #[test]
    fn nav_is_left_and_rotate_is_right_aligned() {
        let width = super::MENU_WIDTH;
        let r = row_rects(width);

        // Every button is a real 30x26 hit target (not collapsed to zero).
        for (name, rect) in [
            ("prev", r.prev_rect),
            ("next", r.next_rect),
            ("cw", r.cw_rect),
            ("ccw", r.ccw_rect),
        ] {
            assert!(
                rect.width() > 20.0 && rect.height() > 20.0,
                "{name} button collapsed: {rect:?}"
            );
        }

        // Nav arrows anchored at the left, in prev-then-next order.
        assert!(r.prev_rect.left() <= 1.0, "prev not at the left edge: {:?}", r.prev_rect);
        assert!(
            r.next_rect.left() > r.prev_rect.right() - 0.5,
            "next should follow prev on the left"
        );

        // Rotate pair right-aligned to the row's right edge, cw-then-ccw order.
        assert!(
            (width - r.ccw_rect.right()).abs() <= 1.0,
            "rotate group not flush to the right edge: ccw={:?}",
            r.ccw_rect
        );
        assert!(
            r.cw_rect.right() <= r.ccw_rect.left() + 0.5,
            "cw should sit to the left of ccw within the right group"
        );

        // The two groups are clearly separated (nav fully left of rotate).
        assert!(
            r.next_rect.right() < r.cw_rect.left(),
            "nav group ({:?}) overlaps rotate group ({:?})",
            r.next_rect,
            r.cw_rect
        );
    }
}
