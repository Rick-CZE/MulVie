//! The adjustable split geometry.
//!
//! One vertical line splits the content area into a left and right column.
//! Each column has its own horizontal line. Dragging a line to an edge
//! collapses the pane on that side, giving 1–4 panes total:
//!
//! ```text
//!            v
//!   +--------+---------+
//!   |  TL    |   TR    |
//!   +---lh---+         |   (lh, rh are independent)
//!   |  BL    +---rh----+
//!   |        |   BR    |
//!   +--------+---------+
//! ```

use eframe::egui::Rect;

/// Which of the four quadrants a pane occupies. The index matches the
/// `panes` array everywhere in the app.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quadrant {
    TopLeft = 0,
    BottomLeft = 1,
    TopRight = 2,
    BottomRight = 3,
}

/// Below this fraction a column/row is considered collapsed.
pub const EPS: f32 = 0.012;

#[derive(Clone, Copy)]
pub struct Layout {
    /// Vertical divider position as a fraction of width (0..=1).
    pub v: f32,
    /// Left column's horizontal divider, fraction of height.
    pub lh: f32,
    /// Right column's horizontal divider, fraction of height.
    pub rh: f32,
}

impl Layout {
    pub fn new(v: f32, lh: f32, rh: f32) -> Self {
        Self {
            v: v.clamp(0.0, 1.0),
            lh: lh.clamp(0.0, 1.0),
            rh: rh.clamp(0.0, 1.0),
        }
    }

    pub fn left_visible(&self) -> bool {
        self.v > EPS
    }
    pub fn right_visible(&self) -> bool {
        self.v < 1.0 - EPS
    }

    /// Screen-space rectangles for every *visible* pane.
    pub fn pane_rects(&self, area: Rect) -> Vec<(Quadrant, Rect)> {
        let mut out = Vec::with_capacity(4);
        let x = area.left() + self.v * area.width();
        let ly = area.top() + self.lh * area.height();
        let ry = area.top() + self.rh * area.height();

        let left = self.left_visible();
        let right = self.right_visible();
        let top_left = left && self.lh > EPS;
        let bot_left = left && self.lh < 1.0 - EPS;
        let top_right = right && self.rh > EPS;
        let bot_right = right && self.rh < 1.0 - EPS;

        if top_left {
            out.push((
                Quadrant::TopLeft,
                Rect::from_min_max(area.left_top(), egui_pos(x, ly)),
            ));
        }
        if bot_left {
            out.push((
                Quadrant::BottomLeft,
                Rect::from_min_max(egui_pos(area.left(), ly), egui_pos(x, area.bottom())),
            ));
        }
        if top_right {
            out.push((
                Quadrant::TopRight,
                Rect::from_min_max(egui_pos(x, area.top()), egui_pos(area.right(), ry)),
            ));
        }
        if bot_right {
            out.push((
                Quadrant::BottomRight,
                Rect::from_min_max(egui_pos(x, ry), area.right_bottom()),
            ));
        }
        out
    }
}

fn egui_pos(x: f32, y: f32) -> eframe::egui::Pos2 {
    eframe::egui::pos2(x, y)
}

#[cfg(test)]
mod tests {
    use super::{Layout, Quadrant};
    use eframe::egui::{pos2, vec2, Rect};

    /// The startup layout (all dividers pushed to the edges) must be exactly
    /// one pane — the top-left — filling the whole area, like a normal
    /// single-image gallery. The MultiView button restores 4 panes.
    #[test]
    fn startup_layout_is_single_full_pane() {
        let area = Rect::from_min_size(pos2(0.0, 34.0), vec2(1280.0, 766.0));
        let rects = Layout::new(1.0, 1.0, 1.0).pane_rects(area);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, Quadrant::TopLeft);
        assert_eq!(rects[0].1, area);

        let multi = Layout::new(0.5, 0.5, 0.5).pane_rects(area);
        assert_eq!(multi.len(), 4);
    }
}
