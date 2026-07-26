//! Gallery management: a per-panel browser and file manager.
//!
//! Shows the managed panel's folder (whole subtree) as a browsable list/grid
//! of in-app, RAM-only thumbnails. Every file carries a small blue mark
//! (top-right) = "part of what the panel plays"; un-marking skips it during
//! navigation without touching the disk. Click selects (frame highlight),
//! Shift = range, Ctrl = add/remove one; double-click makes the panel jump to
//! that file; dragging the highlighted selection onto any panel makes THAT
//! panel play exactly those files.
//!
//! Up to FOUR instances can run at once (each in its own window or pinned
//! into a panel); any one panel is managed by at most one instance. A toggle
//! flips an instance into **File management**, which adds batch rename,
//! duplicate finding, delete-marked (recycle bin) and move-marked.
//!
//! The content background and item-name colour follow the user's MulVie
//! settings (see `LmStyle`), so the browser reads as part of the app; only
//! the titlebar keeps the fixed chrome colour.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontId, Id, Layout, Painter, Rect, RichText,
    Rounding, Sense, Stroke, Ui,
};

use crate::file_ops;
use crate::gallery::{self, SortOrder};
use crate::pane::Pane;
use crate::pdf;
use crate::theme;
use crate::thumbs::{self, ThumbStore};
use crate::widgets::{self, Icon};
use pdfium_render::prelude::Pdfium;

/// Base window title; instance windows append " #2".." #4" so each native
/// window stays findable by its unique title (for the acrylic effect).
pub const WINDOW_TITLE: &str = "MulVie — Gallery management";

/// The most simultaneous instances (also: one per panel at most).
pub const MAX_INSTANCES: usize = 4;

/// Panel letters ↔ pane indices (A=TL, B=TR, C=BL, D=BR), as everywhere.
pub const PANEL_ORDER: [usize; 4] = [0, 2, 1, 3];
pub const PANEL_LABELS: [&str; 4] = ["A", "B", "C", "D"];

/// Below this icon size the browser renders as a list (rows) instead of a grid.
const LIST_THRESHOLD: f32 = 56.0;
pub const MIN_ICON: f32 = 28.0;
pub const MAX_ICON: f32 = 768.0;

pub(crate) const GRID_PAD: f32 = 7.0;
pub(crate) const GRID_LABEL_H: f32 = 17.0;
const LIST_ROW_H: f32 = 26.0;
/// Reserved right-hand gutter so the scrollbar never overlaps content (or the
/// now-playing marker) and its appearance never resizes anything.
const SCROLL_GUTTER: f32 = 14.0;
/// Folder-separator header height in the grid view.
const HEADER_H: f32 = 22.0;

/// The styling Gallery management borrows from the main window (its background
/// and the user-chosen item-name colour), so it looks like part of the app.
#[derive(Clone, Copy)]
pub struct LmStyle {
    /// The main window's acrylic is active (→ a pinned host fills transparent).
    pub glass: bool,
    /// The user's opaque background tint colour (fallback fill without glass).
    pub bg_rgb: Color32,
    /// The user's tint as ABGR, applied to a standalone window's own acrylic.
    pub tint_abgr: u32,
    /// The user-chosen colour for item names.
    pub text: Color32,
}

/// The panel letter for a pane index ("A".."D").
pub fn letter_of(idx: usize) -> &'static str {
    PANEL_ORDER
        .iter()
        .position(|&i| i == idx)
        .map(|k| PANEL_LABELS[k])
        .unwrap_or("?")
}

/// Which face an instance currently shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LmMode {
    Gallery,
    Files,
}

/// An in-flight drag of the highlighted selection out of the list.
pub struct DragPayload {
    pub folder: PathBuf,
    pub files: Vec<PathBuf>,
    pub picked: Vec<PathBuf>,
    pub dropped: bool,
}

#[derive(Clone, PartialEq)]
enum MenuAction {
    None,
    /// Toolbar buttons: act on the highlighted selection (whole list).
    MarkSelected,
    UnmarkSelected,
    /// Right-click menu: act on ONE folder's files only.
    SelectFolder(PathBuf),
    DeselectFolder(PathBuf),
    MarkFolder(PathBuf),
    UnmarkFolder(PathBuf),
    /// Files mode, right-click ON a file: act on THAT file only — regardless
    /// of what is selected or marked.
    DeleteFile(PathBuf),
    MoveFile(PathBuf),
}

#[derive(Default)]
struct Actions {
    clicked: Option<usize>,
    tick: Option<usize>,
    jump: Option<usize>,
    drag_from: Option<usize>,
    menu: Option<MenuAction>,
    bg_clicked: bool,
    /// True when one of OUR responses opened (or is showing) the context
    /// menu this pass — the claim behind `ListManager::menu_owns`.
    menu_open: bool,
    /// The claiming response's id — the menu ROOT is created with exactly
    /// this id, so ownership can later be re-checked against the BarState.
    menu_id: Option<Id>,
    /// Outer Some = a background right-click happened this frame; inner =
    /// the folder section under it, if any. Persisted into `lm.bg_menu` so
    /// the menu keeps its target on the frames after the opening click.
    bg_menu_set: Option<Option<(PathBuf, String)>>,
}

/// Sort orders for the RENAME subset (numbering follows list order).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RnSort {
    NameAsc,
    NameDesc,
    SizeAsc,
    SizeDesc,
    DateAsc,
    DateDesc,
}

impl RnSort {
    const ALL: [RnSort; 6] = [
        RnSort::NameAsc,
        RnSort::NameDesc,
        RnSort::SizeAsc,
        RnSort::SizeDesc,
        RnSort::DateAsc,
        RnSort::DateDesc,
    ];
    fn label(self) -> &'static str {
        match self {
            RnSort::NameAsc => "Name  A to Z",
            RnSort::NameDesc => "Name  Z to A",
            RnSort::SizeAsc => "Size  small to large",
            RnSort::SizeDesc => "Size  large to small",
            RnSort::DateAsc => "Date  old to new",
            RnSort::DateDesc => "Date  new to old",
        }
    }
    fn apply(self, files: &mut [PathBuf]) {
        let name = |p: &PathBuf| file_ops::name_of(p);
        let meta = |p: &PathBuf| std::fs::metadata(p).ok();
        let size = |p: &PathBuf| meta(p).map(|m| m.len()).unwrap_or(0);
        let date = |p: &PathBuf| {
            meta(p)
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        };
        match self {
            RnSort::NameAsc => files.sort_by(|a, b| natord::compare(&name(a), &name(b))),
            RnSort::NameDesc => files.sort_by(|a, b| natord::compare(&name(b), &name(a))),
            RnSort::SizeAsc => files.sort_by_key(size),
            RnSort::SizeDesc => {
                files.sort_by_key(size);
                files.reverse();
            }
            RnSort::DateAsc => files.sort_by_key(date),
            RnSort::DateDesc => {
                files.sort_by_key(date);
                files.reverse();
            }
        }
    }
}

struct RenameConfirm {
    plan: Vec<(PathBuf, PathBuf)>,
    summary: String,
}

pub struct ListManager {
    pub slot: usize,
    pub title: String,
    pub pinned: Option<usize>,
    pub target: usize,
    pub mode: LmMode,
    pub icon_px: f32,
    selected: HashSet<PathBuf>,
    anchor: Option<PathBuf>,
    sel_target: usize,
    sel_folder: Option<PathBuf>,
    pub drag: Option<DragPayload>,
    pub pin_request: Option<Option<usize>>,
    pub close_request: bool,
    pub open_folder_request: bool,
    pub delete_request: Option<Vec<PathBuf>>,
    pub move_request: Option<Vec<PathBuf>>,
    /// A confirmed rename plan, applied by the APP (players holding a file
    /// open must be torn down first; other panes refreshed after).
    pub rename_request: Option<Vec<(PathBuf, PathBuf)>>,
    pub status: String,
    pub taken_targets: [bool; 4],
    pub taken_pins: [bool; 4],
    pub covered_was_playing: bool,
    /// Standalone-window fullscreen state (its own titlebar button + Esc).
    fullscreen: bool,
    pub menu_owns: bool,
    /// The id of the context-menu root THIS browser opened, while it lives.
    /// The root object survives in the app-global BarState across passes —
    /// unlike `context_menu()`'s per-pass return value, which is None both
    /// on the opening pass and on heartbeat passes without child input (the
    /// bug that made Esc close the window "through" an open menu).
    claimed_menu: Option<Id>,
    suppress: bool,
    popup_open_prev: bool,
    /// Whether a text field had keyboard focus at the END of the last frame.
    /// The live check is useless on the Escape frame itself: egui clears the
    /// focused widget while PROCESSING the Escape event, before any ui code
    /// runs — so "Esc leaves the field, second Esc closes the window" needs
    /// the previous frame's state.
    kb_focus_prev: bool,
    glass: bool,
    glass_attempts: u32,
    applied_abgr: Option<u32>,
    /// Deferred requests applied where the pane is mutable.
    sort_request: Option<SortOrder>,
    refresh_request: bool,
    /// Start time of the refresh pulse played mid-browser.
    refresh_flash: Option<f64>,
    /// OS files/folders dropped onto the STANDALONE window (its viewport gets
    /// the drop events, not the main window); the app loads them into the
    /// managed panel.
    pub dropped_paths: Option<Vec<PathBuf>>,
    // File management.
    pub rename_view: bool,
    rename_order: Vec<PathBuf>,
    rn_sort: RnSort,
    rename_name: bool,
    base_input: String,
    rename_ext: bool,
    ext_input: String,
    rename_confirm: Option<RenameConfirm>,
    /// A completed rename's result, shown in a small confirmation window
    /// (set by the app once it has applied the batch).
    pub rename_done: Option<String>,
    pub dupe_view: bool,
    dupe_groups: Vec<Vec<PathBuf>>,
    dupe_request: bool,
    /// The folder section a background right-click menu is open for.
    bg_menu: Option<(PathBuf, String)>,
    /// The pane's un-marked set as it was when the duplicates view opened.
    /// The wipe-on-entry is WORKING state for picking dupes, not a lasting
    /// edit: every way out of the view puts these marks back (without that,
    /// backing out left 0 files marked = the panel's navigation fully
    /// blocked until a manual re-mark).
    saved_marks: Option<HashSet<PathBuf>>,
    menu_action_backlog: Option<MenuAction>,
}

impl ListManager {
    pub fn new(slot: usize, pinned: Option<usize>, target: usize) -> Self {
        let title = if slot == 0 {
            WINDOW_TITLE.to_string()
        } else {
            format!("{WINDOW_TITLE} #{}", slot + 1)
        };
        Self {
            slot,
            title,
            pinned,
            target,
            mode: LmMode::Gallery,
            icon_px: 96.0,
            selected: HashSet::new(),
            anchor: None,
            sel_target: target,
            sel_folder: None,
            drag: None,
            pin_request: None,
            close_request: false,
            open_folder_request: false,
            delete_request: None,
            move_request: None,
            rename_request: None,
            status: String::new(),
            taken_targets: [false; 4],
            taken_pins: [false; 4],
            covered_was_playing: false,
            fullscreen: false,
            menu_owns: false,
            claimed_menu: None,
            suppress: false,
            popup_open_prev: false,
            kb_focus_prev: false,
            glass: false,
            glass_attempts: 0,
            applied_abgr: None,
            sort_request: None,
            refresh_request: false,
            refresh_flash: None,
            dropped_paths: None,
            rename_view: false,
            rename_order: Vec::new(),
            rn_sort: RnSort::NameAsc,
            rename_name: false,
            base_input: String::new(),
            rename_ext: false,
            ext_input: String::new(),
            rename_confirm: None,
            rename_done: None,
            dupe_view: false,
            dupe_groups: Vec::new(),
            dupe_request: false,
            bg_menu: None,
            saved_marks: None,
            menu_action_backlog: None,
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// Drop one path from the selection (it was deleted/moved elsewhere).
    pub fn unselect(&mut self, path: &Path) {
        self.selected.remove(path);
        if self.anchor.as_deref() == Some(path) {
            self.anchor = None;
        }
    }

    /// Drop selected paths that no longer exist on disk (after a rename, move
    /// or delete batch) — the rest of the selection survives untouched.
    pub fn prune_dead_selection(&mut self) {
        self.selected.retain(|p| p.exists());
        if let Some(a) = &self.anchor {
            if !a.exists() {
                self.anchor = None;
            }
        }
    }

    /// Put back the marks captured when the duplicates view opened. No-op
    /// without a snapshot. The snapshot always belongs to the pane and folder
    /// it was taken from (`sel_target`/`sel_folder`) — if that pane browses
    /// something else by now, the stale snapshot is simply dropped (a fresh
    /// folder starts fully marked anyway).
    pub fn restore_marks(&mut self, panes: &mut [Pane; 4]) {
        let Some(excluded) = self.saved_marks.take() else {
            return;
        };
        let pane = &mut panes[self.sel_target.min(3)];
        if pane.folder == self.sel_folder {
            pane.restore_excluded(excluded);
        }
    }

    /// True while one of the rename confirmation modals is up.
    pub fn modal_open(&self) -> bool {
        self.rename_confirm.is_some() || self.rename_done.is_some()
    }

    /// Close whichever rename modal is up (Escape).
    pub fn close_modal(&mut self) {
        if self.rename_done.is_some() {
            self.rename_done = None;
        } else {
            self.rename_confirm = None;
        }
    }

    pub fn exit_views(&mut self) {
        self.rename_view = false;
        self.dupe_view = false;
        self.rename_confirm = None;
        self.rename_done = None;
        self.dupe_groups.clear();
        self.rename_order.clear();
    }

    pub fn reset_glass(&mut self) {
        self.glass = false;
        self.glass_attempts = 0;
        self.applied_abgr = None;
    }

    /// Apply (or re-apply) the standalone window's acrylic with the user's
    /// current tint — re-applies whenever the tint changes.
    fn try_enable_glass(&mut self, tint: u32) {
        if self.glass && self.applied_abgr == Some(tint) {
            return;
        }
        if !self.glass && self.glass_attempts > 240 {
            return;
        }
        self.glass_attempts += 1;
        if let Some(hwnd) = crate::os::find_window_by_title(&self.title) {
            if crate::os::enable_acrylic(hwnd, tint) {
                self.glass = true;
                self.applied_abgr = Some(tint);
            }
        }
    }
}

// --- Hosts ------------------------------------------------------------------

pub fn window_ui(
    lm: &mut ListManager,
    panes: &mut [Pane; 4],
    thumbs: &mut ThumbStore,
    pdfium: Option<&Pdfium>,
    ctx: &egui::Context,
    style: LmStyle,
) {
    lm.try_enable_glass(style.tint_abgr);
    // `menu_owns` is set positively by the body: true only while one of THIS
    // browser's own context menus is open (as of the last drawn frame). The
    // global BarState must not be consulted here — it is shared across every
    // viewport, so a menu open in the MAIN window would freeze this one.
    let menu_open = lm.menu_owns;
    if !menu_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        // Escape peels one layer at a time: an open rename modal first, then
        // fullscreen, then a focused text field (egui itself just drops
        // focus), and only with nothing else to close does it close the window.
        if lm.modal_open() {
            lm.close_modal();
        } else if lm.fullscreen {
            lm.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        } else if !lm.kb_focus_prev {
            lm.close_request = true;
        }
    }
    if ctx.input(|i| i.pointer.any_pressed()) {
        lm.suppress = menu_open || lm.popup_open_prev;
    }
    lm.popup_open_prev = menu_open;
    let suppress = lm.suppress;

    // OS files/folders dropped onto THIS window replace the managed panel's
    // content (the app performs the load).
    let dropped: Vec<PathBuf> = ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .filter_map(|f| f.path.clone())
            .collect()
    });
    if !dropped.is_empty() {
        lm.dropped_paths = Some(dropped);
    }

    egui::TopBottomPanel::top("lm_titlebar")
        .exact_height(34.0)
        .frame(
            egui::Frame::none()
                .fill(theme::HEADER_BG)
                .inner_margin(egui::Margin::symmetric(10.0, 0.0)),
        )
        .show(ctx, |ui| {
            let bar = ui.interact(
                ui.max_rect(),
                Id::new("lm_titlebar_bg"),
                Sense::click_and_drag(),
            );
            if bar.drag_started_by(egui::PointerButton::Primary) {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                crate::app::forget_pointer_after_wm_drag(ctx);
            }
            ui.horizontal_centered(|ui| {
                ui.label(
                    RichText::new("Gallery management")
                        .color(theme::SILVER)
                        .strong()
                        .size(15.0),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // right_to_left → the first added button is the rightmost.
                    let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
                    if widgets::window_button(ui, Icon::Close, true, "Close").clicked() {
                        lm.close_request = true;
                    }
                    if widgets::window_button(ui, Icon::Fullscreen, false, "Fullscreen").clicked() {
                        lm.fullscreen = !lm.fullscreen;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(lm.fullscreen));
                    }
                    let (mic, mtip) = if maximized {
                        (Icon::Restore, "Restore")
                    } else {
                        (Icon::Maximize, "Maximize")
                    };
                    if widgets::window_button(ui, mic, false, mtip).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                    }
                    if widgets::window_button(ui, Icon::Minimize, false, "Minimize").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                });
            });
        });

    // Content background = the user's app background (transparent under this
    // window's own acrylic, else the opaque tint colour).
    let fill = if lm.glass {
        Color32::TRANSPARENT
    } else {
        style.bg_rgb
    };
    egui::CentralPanel::default()
        .frame(egui::Frame::none().fill(fill).inner_margin(egui::Margin::same(8.0)))
        .show(ctx, |ui| {
            body(lm, panes, thumbs, pdfium, ui, suppress, style);
        });

    // Frameless edge/corner resize, like the main window — off while
    // maximized or fullscreen (the OS disables edge-resize then too).
    let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
    if !lm.fullscreen && !maximized {
        let prefix = format!("lm{}", lm.slot);
        for (name, zone, dir, cursor) in
            crate::app::resize_zones(ctx.screen_rect(), crate::app::RESIZE_BAND)
        {
            crate::app::place_resize_area(ctx, &prefix, name, zone, dir, cursor);
        }
    }

    // After all widgets ran: remember whether a field holds focus, for the
    // next frame's Escape decision (see `kb_focus_prev`).
    lm.kb_focus_prev = ctx.wants_keyboard_input();
}

pub fn pinned_ui(
    lm: &mut ListManager,
    panes: &mut [Pane; 4],
    thumbs: &mut ThumbStore,
    pdfium: Option<&Pdfium>,
    ui: &mut Ui,
    rect: Rect,
    suppress: bool,
    style: LmStyle,
) {
    // Follow the app background: transparent (so the main window's acrylic
    // shows) when glass, else the opaque tint colour.
    let fill = if style.glass {
        Color32::TRANSPARENT
    } else {
        style.bg_rgb
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, fill);
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0_f32, theme::ACCENT_DIM));
    let inner = rect.shrink(8.0);
    // The explicit id_salt pins this child ui's identity to the SLOT: without
    // it the id embeds the parent's auto-id counter, which shifts when another
    // pinned browser earlier in the draw order appears/disappears — resetting
    // this one's scroll position and field focus.
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("lm_pinned", lm.slot))
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(inner);
    body(lm, panes, thumbs, pdfium, &mut child, suppress, style);
}

// --- The shared body ---------------------------------------------------------

pub fn body(
    lm: &mut ListManager,
    panes: &mut [Pane; 4],
    thumbs: &mut ThumbStore,
    pdfium: Option<&Pdfium>,
    ui: &mut Ui,
    suppress: bool,
    style: LmStyle,
) {
    lm.target = lm.target.min(3);
    // Ownership check: the app-global context-menu ROOT (BarState is shared
    // by every viewport and persists in ctx.data across passes) is ours iff
    // its id is the one we claimed when it opened. Root gone, or replaced by
    // another viewport's menu → we own nothing.
    let root_id = egui::menu::BarState::load(ui.ctx(), Id::new("__egui::context_menu"))
        .as_ref()
        .map(|r| r.id);
    if root_id != lm.claimed_menu {
        lm.claimed_menu = None;
    }
    lm.menu_owns = lm.claimed_menu.is_some();

    let folder = panes[lm.target].folder.clone();
    if lm.sel_target != lm.target || lm.sel_folder != folder {
        // An implicit way out of the duplicates view: the OLD pane gets its
        // pre-duplicates marks back before we let go of it.
        lm.restore_marks(panes);
        lm.clear_selection();
        lm.exit_views();
        lm.sel_target = lm.target;
        lm.sel_folder = folder.clone();
        lm.status.clear();
    }

    // Deferred pane mutations (sort / refresh) raised by the toolbar.
    if let Some(order) = lm.sort_request.take() {
        panes[lm.target].sort = order;
        panes[lm.target].resort();
    }
    if std::mem::take(&mut lm.refresh_request) {
        panes[lm.target].refresh_folder();
        // A refresh also re-tries thumbnails that failed once (a file still
        // mid-copy on first sight decodes fine now); cache entries stay.
        thumbs.clear_failed();
    }

    // Entering the duplicates view resets marks + selection, then scans. The
    // marks are snapshotted first — every exit path restores them (the wipe
    // is a working state for picking dupes, not a lasting edit).
    if std::mem::take(&mut lm.dupe_request) {
        let pane = &mut panes[lm.target];
        if lm.saved_marks.is_none() {
            lm.saved_marks = Some(pane.excluded.clone());
        }
        lm.clear_selection();
        for p in pane.files.clone() {
            pane.set_included(&p, false);
        }
        let sized: Vec<(PathBuf, u64)> = pane
            .files
            .iter()
            .map(|p| (p.clone(), std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)))
            .collect();
        lm.dupe_groups = file_ops::find_duplicate_groups(&sized);
        lm.status = if lm.dupe_groups.is_empty() {
            "No possible duplicates found.".into()
        } else {
            format!(
                "{} possible duplicate group(s) — mark, then delete or move.",
                lm.dupe_groups.len()
            )
        };
    }

    toolbar(lm, panes, ui, suppress, style);
    // The toolbar may just have left the duplicates view (its toggle, the
    // Rename button, or the mode switch): give the marks back right away so
    // this same frame already draws them restored.
    if !lm.dupe_view {
        lm.restore_marks(panes);
    }
    ui.separator();

    if folder.is_none() || panes[lm.target].files.is_empty() {
        empty_state(lm, ui, folder.is_some(), suppress);
    } else if lm.rename_view {
        rename_body(lm, panes, ui, suppress, style);
    } else if lm.dupe_view {
        dupe_body(lm, panes, thumbs, pdfium, ui, suppress, style);
    } else {
        content(lm, panes, thumbs, pdfium, ui, suppress, style);
    }

    if let Some(dp) = &lm.drag {
        if !dp.dropped {
            if let Some(pos) = ui.ctx().pointer_hover_pos() {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    Id::new(("lm_drag_badge", lm.slot)),
                ));
                let text = format!("{} file(s)", dp.picked.len());
                let font = FontId::proportional(12.0);
                let galley = painter.layout_no_wrap(text, font, Color32::WHITE);
                let at = pos + vec2(16.0, 18.0);
                let r = Rect::from_min_size(at, galley.size() + vec2(12.0, 6.0));
                painter.rect_filled(r, Rounding::same(5.0), theme::ACCENT);
                painter.galley(r.min + vec2(6.0, 3.0), galley, Color32::WHITE);
            }
        }
    }

    // The refresh pulse: the same gentle glow as a folder-boundary flash,
    // played in the middle of the browser.
    if let Some(t0) = lm.refresh_flash {
        let now = ui.input(|i| i.time);
        let t = ((now - t0) / 0.5) as f32;
        if t >= 1.0 {
            lm.refresh_flash = None;
        } else {
            crate::app::radial_glow(ui.painter(), ui.min_rect(), t, theme::ACCENT, 0.45, 0.26);
            ui.ctx().request_repaint();
        }
    }
}

fn toolbar(lm: &mut ListManager, panes: &mut [Pane; 4], ui: &mut Ui, suppress: bool, style: LmStyle) {
    // Row 1: close (far left), panel selector, pin selector, mode toggle,
    // refresh.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if widgets::window_button(ui, Icon::Close, true, "Close this Gallery management").clicked()
            && !suppress
        {
            lm.close_request = true;
        }
        ui.add_space(4.0);
        ui.label(RichText::new("Panel").color(style.text).size(12.0));
        for (k, &idx) in PANEL_ORDER.iter().enumerate() {
            let own = lm.target == idx;
            let enabled = own || !lm.taken_targets[idx];
            let what = panes[idx]
                .folder
                .as_ref()
                .and_then(|f| f.file_name())
                .and_then(|s| s.to_str())
                .map(|s| s.to_owned())
                .unwrap_or_else(|| "empty".into());
            let tip = if enabled {
                format!("Manage panel {} ({what})", PANEL_LABELS[k])
            } else {
                format!("Panel {} is managed by another Gallery management", PANEL_LABELS[k])
            };
            if letter_button(ui, PANEL_LABELS[k], own, enabled, &tip).clicked()
                && !suppress
                && enabled
                && !own
            {
                lm.target = idx;
            }
        }

        ui.add_space(8.0);
        ui.label(RichText::new("Pin").color(style.text).size(12.0));
        for (k, &idx) in PANEL_ORDER.iter().enumerate() {
            let active = lm.pinned == Some(idx);
            let enabled = active || !lm.taken_pins[idx];
            let tip = if active {
                format!("Unpin from panel {} — back to its own window", PANEL_LABELS[k])
            } else if !enabled {
                format!("Another Gallery management is pinned in panel {}", PANEL_LABELS[k])
            } else {
                format!("Pin into panel {} — its content freezes and returns on unpin", PANEL_LABELS[k])
            };
            if letter_button(ui, PANEL_LABELS[k], active, enabled, &tip).clicked()
                && !suppress
                && enabled
            {
                lm.pin_request = Some(if active { None } else { Some(idx) });
            }
        }

        ui.add_space(10.0);
        // Mode toggle (off = gallery, on = file management).
        let mut on = lm.mode == LmMode::Files;
        if toggle_switch(ui, &mut on, "File management", style.text).changed() && !suppress {
            lm.mode = if on { LmMode::Files } else { LmMode::Gallery };
            lm.exit_views();
            lm.status.clear();
        }

        ui.add_space(6.0);
        if widgets::small_icon_button(ui, Icon::Refresh, false, "Refresh — re-scan the folder for added or removed files")
            .clicked()
            && !suppress
        {
            lm.refresh_request = true;
            lm.refresh_flash = Some(ui.input(|i| i.time));
        }
    });

    // Row 2: sort, file-type filter, search.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new("Sort").color(style.text).size(12.0));
        let mut order = panes[lm.target].sort;
        egui::ComboBox::from_id_salt(("lm_sort", lm.slot))
            .selected_text(order.label())
            .width(120.0)
            .show_ui(ui, |ui| {
                for o in SortOrder::ALL {
                    ui.selectable_value(&mut order, o, o.label());
                }
            });
        if order != panes[lm.target].sort && !suppress {
            lm.sort_request = Some(order);
        }

        ui.add_space(4.0);
        ui.label(RichText::new("File type").color(style.text).size(12.0));
        type_filter_menu(lm, &mut panes[lm.target], ui, suppress);

        ui.add_space(4.0);
        // Search: case-insensitive "contains" on the file name, live, whole
        // subtree (like every filter here).
        ui.scope(|ui| {
            let w = &mut ui.visuals_mut().widgets;
            w.inactive.bg_stroke = Stroke::new(1.0_f32, theme::ACCENT);
            w.hovered.bg_stroke = Stroke::new(1.0_f32, theme::ACCENT);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut panes[lm.target].search)
                    .hint_text("Search…")
                    .desired_width(150.0),
            );
            // No dismiss-latch gate here: the text can only change by TYPING
            // (a menu-dismissing click never edits it), and gating meant a
            // search typed right after closing a menu silently didn't filter.
            if resp.changed() {
                panes[lm.target].apply_filter();
            }
        });
    });

    let pane = &panes[lm.target];
    let marked = pane.files.iter().filter(|p| pane.is_included(p)).count();

    // Its own row: the counts — or the active status message (find-duplicates,
    // rename/move/delete results) which replaces the counts while present.
    ui.horizontal(|ui| {
        if lm.status.is_empty() {
            ui.label(
                RichText::new(format!(
                    "{} of {} marked · {} selected",
                    marked,
                    pane.files.len(),
                    lm.selected.len()
                ))
                .color(style.text)
                .size(12.0),
            );
        } else {
            ui.label(RichText::new(&lm.status).color(style.text).size(12.0));
        }
    });

    // Select / mark buttons, with the size slider on the right.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if ui.button("Select all").clicked() && !suppress {
            lm.selected = pane.files.iter().cloned().collect();
            lm.anchor = None;
        }
        if ui.button("Select none").clicked() && !suppress {
            lm.clear_selection();
        }
        let has_sel = !lm.selected.is_empty();
        if ui.add_enabled(has_sel, egui::Button::new("Mark selected")).clicked() && !suppress {
            lm.menu_action_backlog = Some(MenuAction::MarkSelected);
        }
        if ui.add_enabled(has_sel, egui::Button::new("Unmark selected")).clicked() && !suppress {
            lm.menu_action_backlog = Some(MenuAction::UnmarkSelected);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            blue_handles(ui);
            ui.add(
                egui::Slider::new(&mut lm.icon_px, MIN_ICON..=MAX_ICON)
                    .show_value(false)
                    .logarithmic(true),
            );
            ui.label(RichText::new("Size").color(style.text).size(12.0));
        });
    });

    // File-management tool row.
    if lm.mode == LmMode::Files {
        let marked_paths: Vec<PathBuf> =
            pane.files.iter().filter(|p| pane.is_included(p)).cloned().collect();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let any = !marked_paths.is_empty();
            if ui
                .add_enabled(any || lm.rename_view, egui::Button::new("Rename marked…").selected(lm.rename_view))
                .clicked()
                && !suppress
            {
                lm.rename_view = !lm.rename_view;
                lm.dupe_view = false;
                lm.status.clear();
                if lm.rename_view {
                    lm.rename_order = marked_paths.clone();
                }
            }
            if ui.add(egui::Button::new("Find duplicates").selected(lm.dupe_view)).clicked() && !suppress {
                lm.dupe_view = !lm.dupe_view;
                lm.rename_view = false;
                lm.status.clear();
                if lm.dupe_view {
                    lm.dupe_request = true;
                } else {
                    lm.dupe_groups.clear();
                }
            }
            if ui.add_enabled(any, egui::Button::new("Delete marked…")).clicked() && !suppress {
                lm.delete_request = Some(marked_paths.clone());
            }
            if ui.add_enabled(any, egui::Button::new("Move marked…")).clicked() && !suppress {
                lm.move_request = Some(marked_paths.clone());
            }
        });
    }
}

/// The "File type" menu: include/exclude extensions singly or by category;
/// changes apply live (no manual refresh needed).
fn type_filter_menu(lm: &ListManager, pane: &mut Pane, ui: &mut Ui, suppress: bool) {
    const GROUPS: [(&str, &[&str]); 4] = [
        ("Pictures", gallery::IMAGE_EXTS),
        ("Videos", gallery::VIDEO_EXTS),
        ("Music", gallery::AUDIO_EXTS),
        ("Files (PDF)", gallery::PDF_EXTS),
    ];
    let total = gallery::all_supported_exts().len();
    let picked = pane.type_filter.len();
    let label = if picked >= total {
        "All types".to_string()
    } else {
        format!("{picked} type(s)")
    };
    let mut dirty = false;
    ui.menu_button(label, |ui| {
        ui.set_min_width(260.0);
        ui.horizontal(|ui| {
            if ui.button("Select all").clicked() {
                pane.type_filter = gallery::all_supported_exts();
                dirty = true;
            }
            if ui.button("Select none").clicked() {
                pane.type_filter.clear();
                dirty = true;
            }
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(340.0)
            .show(ui, |ui| {
                for (name, exts) in GROUPS {
                    let mut whole = exts.iter().all(|e| pane.type_filter.contains(*e));
                    if ui
                        .checkbox(&mut whole, RichText::new(name).strong())
                        .changed()
                    {
                        for e in exts {
                            if whole {
                                pane.type_filter.insert(e.to_string());
                            } else {
                                pane.type_filter.remove(*e);
                            }
                        }
                        dirty = true;
                    }
                    ui.horizontal_wrapped(|ui| {
                        for e in exts {
                            let mut on = pane.type_filter.contains(*e);
                            if ui.checkbox(&mut on, *e).changed() {
                                if on {
                                    pane.type_filter.insert(e.to_string());
                                } else {
                                    pane.type_filter.remove(*e);
                                }
                                dirty = true;
                            }
                        }
                    });
                    ui.separator();
                }
            });
    });
    if dirty && !suppress {
        pane.apply_filter();
    }
    let _ = lm;
}

fn empty_state(lm: &mut ListManager, ui: &mut Ui, has_folder: bool, suppress: bool) {
    let letter = letter_of(lm.target);
    ui.add_space(36.0);
    ui.vertical_centered(|ui| {
        let msg = if has_folder {
            format!("Panel {letter} has no supported files in its folder.")
        } else {
            format!("Panel {letter} is empty.")
        };
        ui.label(RichText::new(msg).color(theme::HINT).size(15.0));
        ui.add_space(10.0);
        if widgets::text_button(ui, "Open folder…", true, true).clicked() && !suppress {
            lm.open_folder_request = true;
        }
    });
}

// --- The file browser --------------------------------------------------------

fn content(
    lm: &mut ListManager,
    panes: &mut [Pane; 4],
    thumbs: &mut ThumbStore,
    pdfium: Option<&Pdfium>,
    ui: &mut Ui,
    suppress: bool,
    style: LmStyle,
) {
    // Ctrl+scroll over the browser resizes the thumbnails. (egui turns
    // ctrl+wheel into zoom_delta, so the scroll area doesn't also scroll.)
    let view = ui.available_rect_before_wrap();
    let zoom = ui.input(|i| i.zoom_delta());
    if zoom != 1.0 && ui.rect_contains_pointer(view) {
        lm.icon_px = (lm.icon_px * zoom).clamp(MIN_ICON, MAX_ICON);
    }

    // Every file currently on screen in ANY pane gets a now-playing marker.
    let playing: HashSet<PathBuf> = panes.iter().filter_map(|p| p.current_path().cloned()).collect();
    let pane = &mut panes[lm.target];
    let n = pane.files.len();
    let ppp = ui.ctx().pixels_per_point();
    let avail_w = ui.available_width();
    // Content never uses the reserved scrollbar gutter, so the bar's
    // appearance never moves or resizes anything.
    let content_w = (avail_w - SCROLL_GUTTER).max(MIN_ICON + 2.0 * GRID_PAD);
    let icon = lm.icon_px.min(content_w - 2.0 * GRID_PAD).max(MIN_ICON);
    let list_mode = lm.icon_px < LIST_THRESHOLD;
    let tier = thumbs::tier_for(icon * ppp);

    let mut act = Actions::default();
    let mut pdf_budget = 1usize;

    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        floating_blue_scrollbar(ui);
        let scroll = egui::ScrollArea::vertical()
            .id_salt(("lm_scroll", lm.slot))
            .auto_shrink([false, true]);
        if list_mode {
            scroll.show_rows(ui, LIST_ROW_H, n, |ui, range| {
                for i in range {
                    let (rect, _) = ui.allocate_exact_size(vec2(content_w, LIST_ROW_H), Sense::hover());
                    draw_cell(
                        ui, lm, pane, thumbs, pdfium, &mut act, &mut pdf_budget, i, rect, icon,
                        tier, true, &playing, suppress, style,
                    );
                }
                leftover_bg(ui, lm.slot, content_w, &mut act, suppress);
            });
        } else {
            grid(
                ui, lm, pane, thumbs, pdfium, &mut act, &mut pdf_budget, content_w, icon, tier,
                &playing, suppress, style,
            );
        }
    });

    apply_actions(lm, pane, act, ui, suppress);
}

/// Grid view with folder separators, virtualized: only rows intersecting the
/// visible viewport are built, so a huge tree stays cheap and thumbnails load
/// lazily.
#[allow(clippy::too_many_arguments)]
fn grid(
    ui: &mut Ui,
    lm: &ListManager,
    pane: &Pane,
    thumbs: &mut ThumbStore,
    pdfium: Option<&Pdfium>,
    act: &mut Actions,
    pdf_budget: &mut usize,
    content_w: f32,
    icon: f32,
    tier: u32,
    playing: &HashSet<PathBuf>,
    suppress: bool,
    style: LmStyle,
) {
    let (cols, cell_w, cell_h) = grid_dims(content_w, icon);
    // Contiguous same-folder runs (the recursive scan groups by folder).
    let groups = folder_groups(&pane.files, pane.folder.as_deref());
    let show_headers = groups.len() > 1;

    // Flattened virtual rows with their heights + y offsets.
    enum VRow {
        Header(String),
        Tiles { start: usize, count: usize },
    }
    let mut rows: Vec<(f32, f32, VRow)> = Vec::new(); // (y, h, row)
    // Each folder section's vertical span (header included), so a right-click
    // on the section's EMPTY space still knows which folder it belongs to.
    let mut sections: Vec<(f32, f32, PathBuf, String)> = Vec::new(); // (y0, y1, dir, label)
    let mut y = 0.0f32;
    for g in &groups {
        let sec_y0 = y;
        if show_headers {
            rows.push((y, HEADER_H, VRow::Header(g.label.clone())));
            y += HEADER_H;
        }
        let mut i = g.start;
        while i < g.start + g.len {
            let count = (g.start + g.len - i).min(cols);
            rows.push((y, cell_h, VRow::Tiles { start: i, count }));
            y += cell_h;
            i += count;
        }
        let dir = pane.files[g.start]
            .parent()
            .unwrap_or(Path::new(""))
            .to_path_buf();
        sections.push((sec_y0, y, dir, g.label.clone()));
    }
    let total_h = y;

    egui::ScrollArea::vertical()
        .id_salt(("lm_grid", lm.slot))
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, vp| {
            let (area, bg) = ui.allocate_exact_size(vec2(content_w, total_h.max(vp.height())), Sense::click());
            if bg.clicked() && !suppress {
                act.bg_clicked = true;
            }
            let origin = area.min;

            // Right-click on a section's EMPTY space (or its header) opens the
            // same folder-scoped menu its files show. The target folder is
            // resolved ON THE OPENING CLICK and persisted (`lm.bg_menu` via
            // apply_actions) — resolving from the live pointer would retarget
            // the open menu as the mouse moves onto it.
            let fresh: Option<Option<(PathBuf, String)>> = if bg.secondary_clicked() {
                Some(bg.interact_pointer_pos().and_then(|pos| {
                    let ry = pos.y - origin.y;
                    sections
                        .iter()
                        .find(|(y0, y1, _, _)| ry >= *y0 && ry < *y1)
                        .map(|(_, _, d, l)| (d.clone(), l.clone()))
                }))
            } else {
                None
            };
            let menu_target = match &fresh {
                Some(v) => v.clone(),
                None => lm.bg_menu.clone(),
            };
            let fresh_click = fresh.is_some();
            act.bg_menu_set = fresh;
            let menu_shown = bg
                .context_menu(|ui| match &menu_target {
                    Some((dir, label)) => {
                        let m = cell_menu(ui, dir, label, None, lm.mode == LmMode::Files);
                        if m != MenuAction::None {
                            act.menu = Some(m);
                        }
                    }
                    // Below every section (the stretch-fill area): no folder
                    // to act on — don't show an empty menu.
                    None => ui.close_menu(),
                })
                .is_some();
            // The opening click claims ownership immediately — context_menu()
            // returns None on the opening pass (see the note in draw_cell).
            if menu_shown || (fresh_click && menu_target.is_some()) {
                act.menu_open = true;
                act.menu_id = Some(bg.id);
            }
            for (ry, rh, row) in &rows {
                if *ry + *rh < vp.min.y || *ry > vp.max.y {
                    continue; // cull off-screen rows
                }
                match row {
                    VRow::Header(label) => {
                        let p = ui.painter();
                        let yy = origin.y + *ry + *rh * 0.5;
                        p.text(
                            pos2(origin.x + 2.0, yy),
                            Align2::LEFT_CENTER,
                            label,
                            FontId::proportional(12.0),
                            style.text,
                        );
                        let tx = origin.x
                            + p.layout_no_wrap(label.clone(), FontId::proportional(12.0), style.text)
                                .size()
                                .x
                            + 12.0;
                        p.hline(tx..=origin.x + content_w - 4.0, yy, Stroke::new(1.0_f32, style.text));
                    }
                    VRow::Tiles { start, count } => {
                        for c in 0..*count {
                            let i = start + c;
                            let cell = Rect::from_min_size(
                                pos2(origin.x + c as f32 * cell_w, origin.y + *ry),
                                vec2(cell_w, cell_h),
                            );
                            draw_cell(
                                ui, lm, pane, thumbs, pdfium, act, pdf_budget, i, cell, icon, tier,
                                false, playing, suppress, style,
                            );
                        }
                    }
                }
            }
        });
}

/// A group of contiguous files sharing one immediate folder.
struct FGroup {
    label: String,
    start: usize,
    len: usize,
}

fn folder_groups(files: &[PathBuf], root: Option<&Path>) -> Vec<FGroup> {
    let mut out: Vec<FGroup> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let label = rel_dir(root, f).unwrap_or_else(|| "(main folder)".to_string());
        match out.last_mut() {
            Some(g) if g.label == label => g.len += 1,
            _ => out.push(FGroup { label, start: i, len: 1 }),
        }
    }
    out
}

/// A background click-catcher below a short list clears the selection.
fn leftover_bg(ui: &mut Ui, slot: usize, w: f32, act: &mut Actions, suppress: bool) {
    let left = ui.available_size_before_wrap();
    if left.y > 4.0 {
        let (rect, _) = ui.allocate_exact_size(vec2(w, left.y), Sense::hover());
        let resp = ui.interact(rect, Id::new(("lm_bg", slot)), Sense::click());
        if resp.clicked() && !suppress {
            act.bg_clicked = true;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_cell(
    ui: &mut Ui,
    lm: &ListManager,
    pane: &Pane,
    thumbs: &mut ThumbStore,
    pdfium: Option<&Pdfium>,
    act: &mut Actions,
    pdf_budget: &mut usize,
    i: usize,
    cell: Rect,
    icon: f32,
    tier: u32,
    list_mode: bool,
    playing: &HashSet<PathBuf>,
    suppress: bool,
    style: LmStyle,
) {
    let path = &pane.files[i];
    let name = file_ops::name_of(path);
    let rel = rel_dir(pane.folder.as_deref(), path);
    let included = pane.is_included(path);
    let selected = lm.selected.contains(path);
    let is_current = playing.contains(path);

    let resp = ui.interact(cell, Id::new(("lm_cell", lm.slot, i)), Sense::click_and_drag());
    let painter = ui.painter_at(cell);

    let (thumb_rect, tick_rect, name_pos) = if list_mode {
        let tick = Rect::from_center_size(pos2(cell.left() + 14.0, cell.center().y), vec2(13.0, 13.0));
        let th = Rect::from_center_size(pos2(cell.left() + 34.0, cell.center().y), vec2(20.0, 20.0));
        (th, tick, pos2(cell.left() + 50.0, cell.center().y))
    } else {
        let th = Rect::from_min_size(pos2(cell.left() + GRID_PAD, cell.top() + GRID_PAD), vec2(icon, icon));
        let tick = Rect::from_min_size(th.right_top() + vec2(-15.0, 2.0), vec2(13.0, 13.0));
        (th, tick, pos2(cell.center().x, cell.bottom() - GRID_LABEL_H * 0.5 - GRID_PAD * 0.5))
    };

    if selected {
        painter.rect_filled(
            cell.shrink(1.0),
            Rounding::same(5.0),
            Color32::from_rgba_unmultiplied(0x4C, 0x82, 0xD3, 42),
        );
        painter.rect_stroke(cell.shrink(1.0), Rounding::same(5.0), Stroke::new(1.6_f32, theme::ACCENT));
    } else if resp.hovered() {
        painter.rect_stroke(cell.shrink(1.0), Rounding::same(5.0), Stroke::new(1.0_f32, theme::ACCENT_DIM));
    }

    // Thumbnail / badge — full strength whether marked or not.
    draw_preview(&painter, thumb_rect, path, thumbs, pdfium, tier, pdf_budget, ui.ctx());

    // Name — always the user's chosen colour; marked names are faux-bold in
    // list view (double-drawn, so the layout never shifts).
    if list_mode {
        painter.text(name_pos, Align2::LEFT_CENTER, &name, FontId::proportional(13.0), style.text);
        if included {
            painter.text(name_pos + vec2(0.4, 0.0), Align2::LEFT_CENTER, &name, FontId::proportional(13.0), style.text);
        }
        if let Some(rel) = &rel {
            let w = painter.layout_no_wrap(name.clone(), FontId::proportional(13.0), style.text).size().x;
            painter.text(
                pos2(name_pos.x + w + 10.0, name_pos.y),
                Align2::LEFT_CENTER,
                format!("— {rel}"),
                FontId::proportional(11.0),
                theme::HINT,
            );
        }
    } else {
        // A name too long for the tile shows its START (clipped right by the
        // cell painter); short names stay centred. The full name is the
        // hover tooltip either way.
        let font = FontId::proportional(11.0);
        let text_w = painter
            .layout_no_wrap(name.clone(), font.clone(), style.text)
            .size()
            .x;
        if text_w > cell.width() - 6.0 {
            painter.text(
                pos2(cell.left() + 3.0, name_pos.y),
                Align2::LEFT_CENTER,
                &name,
                font,
                style.text,
            );
        } else {
            painter.text(name_pos, Align2::CENTER_CENTER, &name, font, style.text);
        }
    }

    // Now-playing marker (kept clear of the scrollbar gutter in list mode).
    if is_current {
        let c = if list_mode {
            pos2(cell.right() - 10.0, cell.center().y)
        } else {
            pos2(thumb_rect.left() + 9.0, thumb_rect.top() + 9.0)
        };
        painter.circle_filled(c, 7.0, theme::ACCENT);
        painter.add(egui::Shape::convex_polygon(
            vec![pos2(c.x - 2.0, c.y - 3.5), pos2(c.x - 2.0, c.y + 3.5), pos2(c.x + 3.5, c.y)],
            Color32::WHITE,
            Stroke::NONE,
        ));
    }

    // The mark.
    if included {
        painter.rect_filled(tick_rect, Rounding::same(3.0), theme::ACCENT);
        let t = tick_rect;
        let st = Stroke::new(1.8_f32, Color32::WHITE);
        painter.line_segment([pos2(t.left() + 3.0, t.center().y + 0.5), pos2(t.center().x - 0.5, t.bottom() - 3.0)], st);
        painter.line_segment([pos2(t.center().x - 0.5, t.bottom() - 3.0), pos2(t.right() - 2.5, t.top() + 3.0)], st);
    } else {
        painter.rect_filled(tick_rect, Rounding::same(3.0), Color32::from_rgba_unmultiplied(255, 255, 255, 26));
        painter.rect_stroke(tick_rect, Rounding::same(3.0), Stroke::new(1.2_f32, theme::HINT));
    }

    if !suppress {
        let on_tick = resp
            .interact_pointer_pos()
            .map(|p| tick_rect.expand(3.0).contains(p))
            .unwrap_or(false);
        if resp.double_clicked() && !on_tick {
            act.jump = Some(i);
        } else if resp.clicked() {
            if on_tick {
                act.tick = Some(i);
            } else {
                act.clicked = Some(i);
            }
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            act.drag_from = Some(i);
        }
    }
    let shown = resp
        .context_menu(|ui| {
            let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
            let label = rel.clone().unwrap_or_else(|| "(main folder)".to_string());
            let m = cell_menu(ui, &dir, &label, Some(path.as_path()), lm.mode == LmMode::Files);
            if m != MenuAction::None {
                act.menu = Some(m);
            }
        })
        .is_some();
    // Claim menu ownership on the opening click itself — context_menu()
    // returns None on the pass that OPENS the menu (and on heartbeat passes
    // without child input), so waiting for Some would leave an Escape in the
    // gap reading "no menu" and closing the whole window.
    if shown || resp.secondary_clicked() {
        act.menu_open = true;
        act.menu_id = Some(resp.id);
    }
    if !list_mode {
        match &rel {
            Some(rel) => resp.on_hover_text(format!("{rel}{}{name}", std::path::MAIN_SEPARATOR)),
            None => resp.on_hover_text(&name),
        };
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_preview(
    painter: &Painter,
    rect: Rect,
    path: &Path,
    thumbs: &mut ThumbStore,
    pdfium: Option<&Pdfium>,
    tier: u32,
    pdf_budget: &mut usize,
    ctx: &egui::Context,
) {
    if gallery::is_video(path) {
        draw_badge(painter, rect, Badge::Video);
        return;
    }
    if gallery::is_audio(path) {
        draw_badge(painter, rect, Badge::Audio);
        return;
    }
    if gallery::is_pdf(path) {
        if thumbs.get(path, tier).is_none() && !thumbs.is_failed(path) {
            match pdfium {
                Some(p) if *pdf_budget > 0 => {
                    *pdf_budget -= 1;
                    let img = pdf::render_page(p, path, 0, tier as f32, tier as f32, 1.0, 0);
                    thumbs.insert(ctx, path, tier, img);
                }
                Some(_) => ctx.request_repaint(),
                None => {
                    draw_badge(painter, rect, Badge::Pdf);
                    return;
                }
            }
        }
    } else if gallery::is_image(path) {
        if !thumbs.is_failed(path) {
            thumbs.request(path, tier);
        }
    } else {
        draw_badge(painter, rect, Badge::Broken);
        return;
    }

    if thumbs.is_failed(path) {
        draw_badge(painter, rect, if gallery::is_pdf(path) { Badge::Pdf } else { Badge::Broken });
        return;
    }
    let thumb = thumbs.get(path, tier).or_else(|| thumbs.get_any(path));
    match thumb {
        Some(t) => {
            let (w, h) = (t.size[0] as f32, t.size[1] as f32);
            if w <= 0.0 || h <= 0.0 {
                draw_badge(painter, rect, Badge::Broken);
                return;
            }
            let fit = (rect.width() / w).min(rect.height() / h);
            let size = vec2(w * fit, h * fit);
            let r = Rect::from_center_size(rect.center(), size);
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(t.tex.id(), r, uv, Color32::WHITE);
        }
        None => {
            draw_badge(painter, rect, Badge::Loading);
            ctx.request_repaint();
        }
    }
}

fn apply_actions(lm: &mut ListManager, pane: &mut Pane, mut act: Actions, ui: &Ui, suppress: bool) {
    if act.menu_open {
        lm.menu_owns = true;
        if act.menu_id.is_some() {
            lm.claimed_menu = act.menu_id; // held until this root closes
        }
    }
    if let Some(target) = act.bg_menu_set.take() {
        lm.bg_menu = target; // a background right-click re-resolved the folder
    }
    if act.menu.is_none() {
        act.menu = lm.menu_action_backlog.take();
    }
    // MENU actions apply even while the dismiss-click latch is up: clicking a
    // menu item is itself the popup click that raises the latch — gating them
    // on it would silently discard every context-menu pick.
    apply_menu_action(lm, pane, act.menu.take());
    if suppress {
        return;
    }
    let (shift, ctrl) = ui.input(|i| (i.modifiers.shift, i.modifiers.ctrl || i.modifiers.command));

    if let Some(i) = act.clicked {
        if let Some(path) = pane.files.get(i).cloned() {
            if shift {
                if let Some(anchor) = lm.anchor.clone() {
                    if let Some(range) = range_between(&pane.files, &anchor, &path) {
                        lm.selected = range.into_iter().collect();
                    }
                } else {
                    lm.selected = std::iter::once(path.clone()).collect();
                    lm.anchor = Some(path);
                }
            } else if ctrl {
                if !lm.selected.remove(&path) {
                    lm.selected.insert(path.clone());
                }
                lm.anchor = Some(path);
            } else {
                lm.selected = std::iter::once(path.clone()).collect();
                lm.anchor = Some(path);
            }
        }
    }
    if act.bg_clicked && !shift && !ctrl {
        lm.clear_selection();
    }
    if let Some(i) = act.tick {
        if let Some(path) = pane.files.get(i).cloned() {
            let now = !pane.is_included(&path);
            pane.set_included(&path, now);
        }
    }
    if let Some(i) = act.jump {
        if let Some(path) = pane.files.get(i).cloned() {
            pane.jump_to(&path);
        }
    }
    if let Some(i) = act.drag_from {
        if let Some(path) = pane.files.get(i).cloned() {
            if !lm.selected.contains(&path) {
                lm.selected = std::iter::once(path.clone()).collect();
                lm.anchor = Some(path);
            }
            if let Some(folder) = pane.folder.clone() {
                let picked: Vec<PathBuf> =
                    pane.files.iter().filter(|p| lm.selected.contains(*p)).cloned().collect();
                if !picked.is_empty() {
                    lm.drag = Some(DragPayload { folder, files: pane.files.clone(), picked, dropped: false });
                }
            }
        }
    }
    if let Some(dp) = &mut lm.drag {
        if !dp.dropped && ui.input(|i| i.pointer.primary_released()) {
            dp.dropped = true;
        }
    }
}

fn apply_menu_action(lm: &mut ListManager, pane: &mut Pane, menu: Option<MenuAction>) {
    match menu {
        Some(MenuAction::MarkSelected) => {
            for p in lm.selected.clone() {
                pane.set_included(&p, true);
            }
        }
        Some(MenuAction::UnmarkSelected) => {
            for p in lm.selected.clone() {
                pane.set_included(&p, false);
            }
        }
        Some(MenuAction::SelectFolder(dir)) => {
            for p in pane.files.iter().filter(|p| p.parent() == Some(dir.as_path())) {
                lm.selected.insert(p.clone());
            }
        }
        Some(MenuAction::DeselectFolder(dir)) => {
            lm.selected.retain(|p| p.parent() != Some(dir.as_path()));
        }
        Some(MenuAction::MarkFolder(dir)) => {
            let in_dir: Vec<PathBuf> = pane
                .files
                .iter()
                .filter(|p| p.parent() == Some(dir.as_path()))
                .cloned()
                .collect();
            for p in in_dir {
                pane.set_included(&p, true);
            }
        }
        Some(MenuAction::UnmarkFolder(dir)) => {
            let in_dir: Vec<PathBuf> = pane
                .files
                .iter()
                .filter(|p| p.parent() == Some(dir.as_path()))
                .cloned()
                .collect();
            for p in in_dir {
                pane.set_included(&p, false);
            }
        }
        // Single-file operations: the same app machinery as the marked-files
        // buttons (confirm prompt / destination picker + conflict flow), just
        // with a one-file payload.
        Some(MenuAction::DeleteFile(p)) => {
            lm.delete_request = Some(vec![p]);
        }
        Some(MenuAction::MoveFile(p)) => {
            lm.move_request = Some(vec![p]);
        }
        _ => {}
    }
}

// --- Files mode: rename subset view -------------------------------------------

fn rename_body(lm: &mut ListManager, panes: &mut [Pane; 4], ui: &mut Ui, suppress: bool, style: LmStyle) {
    let pane = &mut panes[lm.target];
    // Prune against the FULL scan, not the filtered view: only files that
    // vanished from disk or were unmarked leave the batch. Typing in the
    // still-visible search box must not silently shrink it (retain is
    // destructive — a filtered-out file would never come back).
    let on_disk: HashSet<&PathBuf> = pane.all_files().iter().collect();
    lm.rename_order.retain(|p| on_disk.contains(p) && pane.is_included(p));

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "Renaming {} marked file(s) — the order below sets the numbers (top = _0001).",
                lm.rename_order.len()
            ))
            .color(theme::BRIGHT)
            .size(13.0),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut chosen = lm.rn_sort;
            egui::ComboBox::from_id_salt(("lm_rn_sort", lm.slot))
                .selected_text(chosen.label())
                .show_ui(ui, |ui| {
                    for s in RnSort::ALL {
                        ui.selectable_value(&mut chosen, s, s.label());
                    }
                });
            if chosen != lm.rn_sort {
                lm.rn_sort = chosen;
                lm.rn_sort.apply(&mut lm.rename_order);
            }
        });
    });

    let root = pane.folder.clone();
    let controls_h = 130.0;
    let list_h = (ui.available_height() - controls_h).max(100.0);
    let mut swap: Option<(usize, usize)> = None;
    let mut unmark: Option<PathBuf> = None;
    egui::ScrollArea::vertical()
        .id_salt(("lm_rename_scroll", lm.slot))
        .auto_shrink([false, true])
        .max_height(list_h)
        .show(ui, |ui| {
            floating_blue_scrollbar(ui);
            ui.spacing_mut().item_spacing.y = 2.0;
            let n = lm.rename_order.len();
            for i in 0..n {
                let path = lm.rename_order[i].clone();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{:>4}", i + 1)).color(theme::HINT).size(12.0));
                    let (tick, resp) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::click());
                    let p = ui.painter();
                    p.rect_filled(tick, Rounding::same(3.0), theme::ACCENT);
                    let st = Stroke::new(1.8_f32, Color32::WHITE);
                    p.line_segment([pos2(tick.left() + 3.0, tick.center().y + 0.5), pos2(tick.center().x - 0.5, tick.bottom() - 3.0)], st);
                    p.line_segment([pos2(tick.center().x - 0.5, tick.bottom() - 3.0), pos2(tick.right() - 2.5, tick.top() + 3.0)], st);
                    if resp.on_hover_text("Unmark — remove from this rename").clicked() && !suppress {
                        unmark = Some(path.clone());
                    }
                    ui.label(RichText::new(file_ops::name_of(&path)).color(style.text).size(14.0));
                    if let Some(rel) = rel_dir(root.as_deref(), &path) {
                        ui.label(RichText::new(format!("— {rel}")).color(theme::HINT).size(11.0));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::small_icon_button(ui, Icon::Down, false, "Move down").clicked() && !suppress && i + 1 < n {
                            swap = Some((i, i + 1));
                        }
                        if widgets::small_icon_button(ui, Icon::Up, false, "Move up").clicked() && !suppress && i > 0 {
                            swap = Some((i, i - 1));
                        }
                    });
                });
            }
        });
    if let Some((a, b)) = swap {
        lm.rename_order.swap(a, b);
    }
    if let Some(p) = unmark {
        pane.set_included(&p, false);
    }
    ui.separator();

    ui.horizontal(|ui| {
        ui.checkbox(&mut lm.rename_name, RichText::new("Change name").color(theme::BRIGHT).size(14.0));
        styled_edit(ui, lm.rename_name, &mut lm.base_input, "new base name, e.g. AAA");
        ui.add_space(10.0);
        ui.checkbox(&mut lm.rename_ext, RichText::new("Change extension").color(theme::BRIGHT).size(14.0));
        styled_edit(ui, lm.rename_ext, &mut lm.ext_input, "e.g. jpg");
    });

    let can_apply = !lm.rename_order.is_empty()
        && ((lm.rename_name && !lm.base_input.trim().is_empty())
            || (lm.rename_ext && !lm.ext_input.trim().is_empty()));
    ui.horizontal(|ui| {
        if widgets::text_button(ui, "Apply", true, can_apply).clicked() && can_apply && !suppress {
            let base = lm.base_input.trim().to_string();
            let ext = lm.ext_input.trim().trim_start_matches('.').to_string();
            let base = (lm.rename_name && !base.is_empty()).then_some(base);
            let ext = (lm.rename_ext && !ext.is_empty()).then_some(ext);
            let plan = file_ops::plan_renames(&lm.rename_order, base.as_deref(), ext.as_deref());
            if plan.is_empty() {
                lm.status = "No changes — names already match.".into();
            } else {
                let summary = rename_summary(base.as_deref(), ext.as_deref(), &plan);
                lm.rename_confirm = Some(RenameConfirm { plan, summary });
            }
        }
        if widgets::text_button(ui, "Cancel", false, true).clicked() && !suppress {
            lm.rename_view = false;
        }
    });

    // Pre-apply confirmation.
    if let Some(confirm) = &lm.rename_confirm {
        let summary = confirm.summary.clone();
        let mut proceed = false;
        let mut cancel = false;
        modal_window(ui.ctx(), "Confirm rename", |ui| {
            ui.label(RichText::new(summary).color(theme::INK_BLUE).size(14.0));
            ui.add_space(6.0);
            ui.label(RichText::new("This renames files on disk and cannot be undone here.").color(theme::HINT).size(11.0));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if widgets::text_button(ui, "Proceed", true, true).clicked() {
                    proceed = true;
                }
                if widgets::text_button(ui, "Cancel", false, true).clicked() {
                    cancel = true;
                }
            });
        });
        if proceed {
            if let Some(c) = lm.rename_confirm.take() {
                // Deferred to the app: a player holding one of these files
                // open must be torn down before its rename can succeed, and
                // every pane browsing the tree refreshes afterwards.
                lm.rename_request = Some(c.plan);
            }
        } else if cancel {
            lm.rename_confirm = None;
        }
    }

    // Post-apply result confirmation.
    if let Some(done) = lm.rename_done.clone() {
        let mut ok = false;
        modal_window(ui.ctx(), "Rename complete", |ui| {
            ui.label(RichText::new(done).color(theme::INK_BLUE).size(14.0));
            ui.add_space(10.0);
            if widgets::text_button(ui, "OK", true, true).clicked() {
                ok = true;
            }
        });
        if ok {
            lm.rename_done = None;
            lm.rename_view = false;
        }
    }
}

fn rename_summary(base: Option<&str>, ext: Option<&str>, plan: &[(PathBuf, PathBuf)]) -> String {
    let n = plan.len();
    if base.is_some() {
        let first = file_ops::name_of(&plan.first().unwrap().1);
        let mut s = if n == 1 {
            format!("Rename 1 file to  {first}  ?")
        } else {
            let last = file_ops::name_of(&plan.last().unwrap().1);
            format!("Rename {n} files to  {first} … {last}  ?")
        };
        if let Some(e) = ext {
            s.push_str(&format!("  (extension to .{e})"));
        }
        s
    } else {
        format!("Change the extension of {n} file(s) to  .{}  ?", ext.unwrap_or(""))
    }
}

// --- Files mode: duplicates view ------------------------------------------------

fn dupe_body(
    lm: &mut ListManager,
    panes: &mut [Pane; 4],
    thumbs: &mut ThumbStore,
    pdfium: Option<&Pdfium>,
    ui: &mut Ui,
    suppress: bool,
    style: LmStyle,
) {
    let playing: HashSet<PathBuf> = panes.iter().filter_map(|p| p.current_path().cloned()).collect();
    let pane = &mut panes[lm.target];
    let index_of: std::collections::HashMap<&PathBuf, usize> =
        pane.files.iter().enumerate().map(|(i, p)| (p, i)).collect();
    let rows: Vec<Option<usize>> = {
        let mut v = Vec::new();
        for (gi, g) in lm.dupe_groups.iter().enumerate() {
            if gi > 0 {
                v.push(None);
            }
            for p in g {
                if let Some(&i) = index_of.get(p) {
                    v.push(Some(i));
                }
            }
        }
        v
    };
    if rows.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No possible duplicates in this folder tree.").color(theme::HINT).size(14.0));
        });
        return;
    }

    let content_w = (ui.available_width() - SCROLL_GUTTER).max(120.0);
    let ppp = ui.ctx().pixels_per_point();
    let tier = thumbs::tier_for(20.0 * ppp);
    let mut act = Actions::default();
    let mut pdf_budget = 1usize;

    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        floating_blue_scrollbar(ui);
        egui::ScrollArea::vertical()
            .id_salt(("lm_dupe_scroll", lm.slot))
            .auto_shrink([false, true])
            .show_rows(ui, LIST_ROW_H, rows.len(), |ui, range| {
                for r in range {
                    let (rect, _) = ui.allocate_exact_size(vec2(content_w, LIST_ROW_H), Sense::hover());
                    match rows[r] {
                        Some(i) => draw_cell(
                            ui, lm, pane, thumbs, pdfium, &mut act, &mut pdf_budget, i, rect, 20.0,
                            tier, true, &playing, suppress, style,
                        ),
                        None => {
                            ui.painter().hline(
                                rect.left() + 8.0..=rect.right() - 8.0,
                                rect.center().y,
                                Stroke::new(1.0_f32, theme::ACCENT_DIM),
                            );
                        }
                    }
                }
            });
    });
    apply_actions(lm, pane, act, ui, suppress);
}

/// The right-click menu on an item or on a folder section's empty space:
/// every folder action is scoped to that folder (the toolbar buttons are the
/// whole-list versions). In Files mode a click ON a file additionally offers
/// delete/move of THAT file only — regardless of selection or marks.
fn cell_menu(
    ui: &mut Ui,
    dir: &Path,
    label: &str,
    file: Option<&Path>,
    files_mode: bool,
) -> MenuAction {
    let mut act = MenuAction::None;
    ui.set_min_width(170.0);
    ui.label(
        RichText::new(format!("In {label}:"))
            .color(theme::INK_BLUE)
            .size(11.0),
    );
    ui.separator();
    if ui.button("Select all").clicked() {
        act = MenuAction::SelectFolder(dir.to_path_buf());
        ui.close_menu();
    }
    if ui.button("Select none").clicked() {
        act = MenuAction::DeselectFolder(dir.to_path_buf());
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Mark all").clicked() {
        act = MenuAction::MarkFolder(dir.to_path_buf());
        ui.close_menu();
    }
    if ui.button("Unmark all").clicked() {
        act = MenuAction::UnmarkFolder(dir.to_path_buf());
        ui.close_menu();
    }
    if files_mode {
        if let Some(f) = file {
            ui.separator();
            ui.label(
                RichText::new(format!("This file ({}):", file_ops::name_of(f)))
                    .color(theme::INK_BLUE)
                    .size(11.0),
            );
            if ui.button("Delete this file…").clicked() {
                act = MenuAction::DeleteFile(f.to_path_buf());
                ui.close_menu();
            }
            if ui.button("Move this file…").clicked() {
                act = MenuAction::MoveFile(f.to_path_buf());
                ui.close_menu();
            }
        }
    }
    act
}

// --- Small drawing helpers ---------------------------------------------------

/// A centred app-styled modal window (shared by the rename confirmations).
fn modal_window(ctx: &egui::Context, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_max_width(380.0);
            add(ui);
        });
}

/// Make this ui's scrollbar float (so it never reserves/rescales layout) and
/// paint its handle in the app accent blue.
fn floating_blue_scrollbar(ui: &mut Ui) {
    let s = ui.style_mut();
    s.spacing.scroll = egui::style::ScrollStyle::floating();
    s.visuals.widgets.inactive.bg_fill = theme::ACCENT_DIM;
    s.visuals.widgets.hovered.bg_fill = theme::ACCENT;
    s.visuals.widgets.active.bg_fill = theme::ACCENT;
}

/// Paint the NEXT slider's handle in the app accent blue.
fn blue_handles(ui: &mut Ui) {
    let w = &mut ui.style_mut().visuals.widgets;
    w.inactive.bg_fill = theme::ACCENT;
    w.hovered.bg_fill = Color32::from_rgb(0x5E, 0x93, 0xE0);
    w.active.bg_fill = Color32::from_rgb(0x5E, 0x93, 0xE0);
}

fn letter_button(ui: &mut Ui, label: &str, active: bool, enabled: bool, tip: &str) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 20.0), sense);
    let hovered = enabled && resp.hovered();
    let bg = if active {
        theme::ACCENT
    } else if hovered {
        theme::ACCENT_DIM
    } else {
        theme::PANEL_STRONG
    };
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(5.0), bg);
    let col = if !enabled {
        theme::HINT
    } else if active || hovered {
        Color32::WHITE
    } else {
        theme::SILVER
    };
    p.text(rect.center(), Align2::CENTER_CENTER, label, FontId::proportional(12.0), col);
    resp.on_hover_text(tip)
}

/// A small on/off pill switch with a trailing label. Off = accent-dim track,
/// on = accent track; the knob slides. Returns the response (`.changed()`).
fn toggle_switch(ui: &mut Ui, on: &mut bool, label: &str, label_col: Color32) -> egui::Response {
    let track = vec2(34.0, 18.0);
    let label_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), FontId::proportional(12.0), label_col)
        .size()
        .x;
    let (rect, mut resp) = ui.allocate_exact_size(vec2(track.x + 6.0 + label_w, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let p = ui.painter();
    let tr = Rect::from_min_size(pos2(rect.left(), rect.center().y - track.y * 0.5), track);
    let track_col = if *on { theme::ACCENT } else { theme::PANEL_STRONG };
    p.rect_filled(tr, Rounding::same(track.y * 0.5), track_col);
    p.rect_stroke(tr, Rounding::same(track.y * 0.5), Stroke::new(1.0_f32, theme::ACCENT_DIM));
    let knob_x = if *on { tr.right() - track.y * 0.5 } else { tr.left() + track.y * 0.5 };
    p.circle_filled(pos2(knob_x, tr.center().y), track.y * 0.5 - 2.0, Color32::WHITE);
    p.text(
        pos2(tr.right() + 6.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        label_col,
    );
    resp
}

fn styled_edit(ui: &mut Ui, active: bool, text: &mut String, hint: &str) {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        if active {
            v.extreme_bg_color = Color32::from_rgb(0xEC, 0xF0, 0xF8);
        } else {
            v.extreme_bg_color = Color32::from_rgba_unmultiplied(255, 255, 255, 12);
        }
        let mut te = egui::TextEdit::singleline(text)
            .hint_text(hint)
            .font(egui::FontId::proportional(15.0))
            .desired_width(180.0);
        if active {
            te = te.text_color(Color32::from_rgb(0x12, 0x1C, 0x30));
        } else {
            te = te.text_color(theme::SILVER);
        }
        let resp = ui.add_enabled(active, te);
        if active {
            ui.painter().rect_stroke(
                resp.rect.expand(1.5),
                Rounding::same(4.0),
                Stroke::new(1.4_f32, theme::ACCENT),
            );
        }
    });
}

enum Badge {
    Video,
    Audio,
    Pdf,
    Broken,
    Loading,
}

fn draw_badge(p: &Painter, rect: Rect, badge: Badge) {
    p.rect_filled(rect, Rounding::same(4.0), theme::PANEL_STRONG);
    let c = rect.center();
    let s = (rect.width().min(rect.height()) * 0.22).clamp(6.0, 34.0);
    let stroke = Stroke::new((s * 0.18).clamp(1.2, 2.6), theme::SILVER);
    match badge {
        Badge::Video => {
            let f = Rect::from_center_size(c, vec2(s * 2.2, s * 1.7));
            p.rect_stroke(f, Rounding::same(2.0), stroke);
            for k in 0..3 {
                let x = f.left() + f.width() * (0.25 + 0.25 * k as f32);
                p.line_segment([pos2(x, f.top()), pos2(x, f.top() + s * 0.28)], stroke);
                p.line_segment([pos2(x, f.bottom() - s * 0.28), pos2(x, f.bottom())], stroke);
            }
            p.add(egui::Shape::convex_polygon(
                vec![pos2(c.x - s * 0.32, c.y - s * 0.42), pos2(c.x - s * 0.32, c.y + s * 0.42), pos2(c.x + s * 0.5, c.y)],
                theme::SILVER,
                Stroke::NONE,
            ));
        }
        Badge::Audio => {
            let head = pos2(c.x - s * 0.35, c.y + s * 0.55);
            p.circle_filled(head, s * 0.38, theme::SILVER);
            p.line_segment([pos2(head.x + s * 0.32, head.y), pos2(head.x + s * 0.32, c.y - s * 0.75)], stroke);
            p.line_segment([pos2(head.x + s * 0.32, c.y - s * 0.75), pos2(head.x + s * 1.05, c.y - s * 0.35)], stroke);
        }
        Badge::Pdf => {
            let f = Rect::from_center_size(c, vec2(s * 1.5, s * 1.9));
            p.rect_stroke(f, Rounding::same(2.0), stroke);
            p.text(c, Align2::CENTER_CENTER, "PDF", FontId::proportional((s * 0.62).clamp(8.0, 14.0)), theme::SILVER);
        }
        Badge::Broken => {
            p.text(c, Align2::CENTER_CENTER, "!", FontId::proportional((s * 1.2).clamp(11.0, 22.0)), theme::HINT);
        }
        Badge::Loading => {
            for k in 0..3i32 {
                p.circle_filled(pos2(c.x + (k - 1) as f32 * s * 0.55, c.y), s * 0.16, theme::HINT);
            }
        }
    }
}

// --- Pure helpers (unit-tested) -----------------------------------------------

pub(crate) fn grid_dims(avail_w: f32, icon_px: f32) -> (usize, f32, f32) {
    let cell_w = icon_px + GRID_PAD * 2.0;
    let cols = ((avail_w / cell_w).floor() as usize).max(1);
    let cell_h = icon_px + GRID_LABEL_H + GRID_PAD * 2.0;
    (cols, cell_w, cell_h)
}

pub(crate) fn range_between(files: &[PathBuf], a: &Path, b: &Path) -> Option<Vec<PathBuf>> {
    let ia = files.iter().position(|p| p == a)?;
    let ib = files.iter().position(|p| p == b)?;
    let (lo, hi) = if ia <= ib { (ia, ib) } else { (ib, ia) };
    Some(files[lo..=hi].to_vec())
}

pub(crate) fn rel_dir(root: Option<&Path>, path: &Path) -> Option<String> {
    let root = root?;
    let rel = path.parent()?.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy();
    (!s.is_empty()).then(|| s.into_owned())
}

#[cfg(test)]
mod tests {
    use super::{folder_groups, grid_dims, letter_of, range_between, rel_dir, GRID_PAD};
    use std::path::{Path, PathBuf};

    #[test]
    fn shift_range_is_inclusive_and_order_free() {
        let files: Vec<PathBuf> = ["a", "b", "c", "d", "e"].iter().map(PathBuf::from).collect();
        let fwd = range_between(&files, Path::new("b"), Path::new("d")).unwrap();
        assert_eq!(fwd, vec![PathBuf::from("b"), PathBuf::from("c"), PathBuf::from("d")]);
        let rev = range_between(&files, Path::new("d"), Path::new("b")).unwrap();
        assert_eq!(rev, fwd);
        assert!(range_between(&files, Path::new("zz"), Path::new("b")).is_none());
    }

    #[test]
    fn grid_dims_are_sane() {
        let (cols, cell_w, cell_h) = grid_dims(600.0, 96.0);
        assert!(cols >= 5, "{cols}");
        assert!(cell_w > 96.0 && cell_h > 96.0);
        let (cols, _, _) = grid_dims(400.0, 768.0);
        assert_eq!(cols, 1);
        assert!(cell_w >= 96.0 + 2.0 * GRID_PAD - 0.01);
    }

    #[test]
    fn panel_letters_match_pane_indices() {
        assert_eq!(letter_of(0), "A");
        assert_eq!(letter_of(2), "B");
        assert_eq!(letter_of(1), "C");
        assert_eq!(letter_of(3), "D");
    }

    #[test]
    fn rel_dir_is_relative_to_root() {
        // join()-built, so separators are native on every platform.
        let root = std::env::temp_dir().join("media");
        assert_eq!(rel_dir(Some(root.as_path()), &root.join("a.png")), None);
        let deep = root.join("sub").join("deep");
        assert_eq!(
            rel_dir(Some(root.as_path()), &deep.join("a.png")),
            Some(Path::new("sub").join("deep").to_string_lossy().into_owned())
        );
    }

    /// Files grouped by their contiguous immediate folder (as the recursive
    /// scan lays them out), for the grid separators.
    #[test]
    fn folder_groups_are_contiguous_runs() {
        let root = std::env::temp_dir().join("m");
        let files: Vec<PathBuf> = vec![
            root.join("a.png"),
            root.join("b.png"),
            root.join("sub").join("c.png"),
            root.join("sub").join("d.png"),
            root.join("two").join("e.png"),
        ];
        let g = folder_groups(&files, Some(root.as_path()));
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].label, "(main folder)");
        assert_eq!((g[0].start, g[0].len), (0, 2));
        assert_eq!(g[1].label, "sub");
        assert_eq!((g[1].start, g[1].len), (2, 2));
        assert_eq!(g[2].label, "two");
        assert_eq!((g[2].start, g[2].len), (4, 1));
    }
}
